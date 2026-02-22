use crate::cache;
use crate::cli::{TransportMode, ViewArgs};
use anyhow::{Context, Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use parking_lot::RwLock;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders, Paragraph};
use retina::client::{
    Credentials, PlayOptions, Session, SessionOptions, SetupOptions, TcpTransportOptions,
    Transport, UdpTransportOptions,
};
use retina::codec::{CodecItem, ParametersRef};
use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use url::Url;

const BRIGHTNESS_RAMP: &[u8] = b" .:-=+*#%@";

#[derive(Debug, Clone)]
struct TileSnapshot {
    stream_label: String,
    frame_ascii: String,
    status: String,
    frames_rendered: u64,
    decode_errors: u64,
}

impl TileSnapshot {
    fn new(stream_label: String) -> Self {
        Self {
            stream_label,
            frame_ascii: String::new(),
            status: "connecting".to_owned(),
            frames_rendered: 0,
            decode_errors: 0,
        }
    }
}

#[derive(Debug)]
struct TileState {
    inner: RwLock<TileSnapshot>,
}

impl TileState {
    fn new(stream_label: String) -> Self {
        Self {
            inner: RwLock::new(TileSnapshot::new(stream_label)),
        }
    }

    fn set_status(&self, status: impl Into<String>) {
        self.inner.write().status = status.into();
    }

    fn set_frame(&self, frame_ascii: String) {
        let mut snapshot = self.inner.write();
        snapshot.frame_ascii = frame_ascii;
        snapshot.frames_rendered = snapshot.frames_rendered.saturating_add(1);
        "streaming".clone_into(&mut snapshot.status);
    }

    fn inc_decode_error(&self) {
        let mut snapshot = self.inner.write();
        snapshot.decode_errors = snapshot.decode_errors.saturating_add(1);
    }

    fn snapshot(&self) -> TileSnapshot {
        self.inner.read().clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellGeometry {
    width: usize,
    height: usize,
    vertical_scale: usize,
}

impl Default for CellGeometry {
    fn default() -> Self {
        Self {
            width: 48,
            height: 18,
            vertical_scale: 2,
        }
    }
}

pub async fn run_view(args: &ViewArgs) -> Result<()> {
    let stream_count = args.streams.len();
    let (rows, cols) = compute_grid_dimensions(stream_count, args.rows, args.cols);

    let mut tiles = Vec::with_capacity(stream_count);
    let (geometry_tx, geometry_rx) = watch::channel(CellGeometry {
        vertical_scale: usize::from(args.vertical_scale.max(1)),
        ..CellGeometry::default()
    });

    let mut workers = Vec::with_capacity(stream_count);
    for stream_url in &args.streams {
        let tile = Arc::new(TileState::new(stream_url.clone()));
        let worker_tile = tile.clone();
        let worker_geometry_rx = geometry_rx.clone();
        let worker_stream = stream_url.clone();
        let worker_transport = args.transport;
        let worker_target_fps = args.target_fps.max(1);

        workers.push(tokio::spawn(async move {
            run_stream_worker(
                worker_stream,
                worker_transport,
                worker_target_fps,
                worker_tile,
                worker_geometry_rx,
            )
            .await;
        }));
        tiles.push(tile);
    }

    enable_raw_mode().context("failed to enable terminal raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;

    let render_result = render_loop(
        &mut terminal,
        &tiles,
        &geometry_tx,
        rows,
        cols,
        args.refresh_fps.max(1),
    )
    .await;

    for worker in workers {
        worker.abort();
    }

    disable_raw_mode().context("failed to disable terminal raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("failed to leave alternate screen")?;
    terminal
        .show_cursor()
        .context("failed to restore terminal cursor")?;

    render_result
}

async fn render_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    tiles: &[Arc<TileState>],
    geometry_tx: &watch::Sender<CellGeometry>,
    rows: usize,
    cols: usize,
    refresh_fps: u16,
) -> Result<()> {
    let frame_delay = Duration::from_millis(1_000_u64 / u64::from(refresh_fps.max(1)));
    let mut running = true;

    while running {
        terminal
            .draw(|frame| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(1), Constraint::Length(1)])
                    .split(frame.area());

                let grid_area = chunks[0];
                let footer_area = chunks[1];

                let grid_rects = build_grid_rects(grid_area, rows, cols);
                if let Some(first) = grid_rects.first() {
                    let inner = inner_cell(*first);
                    let geometry = CellGeometry {
                        width: usize::from(inner.width.max(2)),
                        height: usize::from(inner.height.max(2)),
                        vertical_scale: geometry_tx.borrow().vertical_scale,
                    };
                    if *geometry_tx.borrow() != geometry {
                        let _ = geometry_tx.send(geometry);
                    }
                }

                for (idx, tile) in tiles.iter().enumerate() {
                    if idx >= grid_rects.len() {
                        break;
                    }

                    let area = grid_rects[idx];
                    let snapshot = tile.snapshot();
                    let title = format!(
                        "{} | {} | frames={} decode_errs={}",
                        idx + 1,
                        snapshot.stream_label,
                        snapshot.frames_rendered,
                        snapshot.decode_errors
                    );

                    let body = if snapshot.frame_ascii.is_empty() {
                        format!("status: {}", snapshot.status)
                    } else {
                        snapshot.frame_ascii
                    };

                    let block = Block::default().title(title).borders(Borders::ALL);
                    frame.render_widget(Paragraph::new(body).block(block), area);
                }

                let footer = Paragraph::new("q/esc: quit | terminal RTSP grid viewer");
                frame.render_widget(footer, footer_area);
            })
            .context("terminal draw failed")?;

        if event::poll(Duration::from_millis(5)).context("event poll failed")?
            && let Event::Key(key) = event::read().context("event read failed")?
            && key.kind == KeyEventKind::Press
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            running = false;
        }

        tokio::time::sleep(frame_delay).await;
    }

    Ok(())
}

async fn run_stream_worker(
    stream_url: String,
    transport_mode: TransportMode,
    target_fps: u16,
    tile: Arc<TileState>,
    geometry_rx: watch::Receiver<CellGeometry>,
) {
    let candidates = build_stream_candidates(&stream_url);
    let mut retry_delay_secs = 1_u64;

    loop {
        let mut session_ended = false;
        let mut last_error = None;

        for (idx, candidate) in candidates.iter().enumerate() {
            tile.set_status(format!(
                "connecting {}/{}: {}",
                idx + 1,
                candidates.len(),
                display_endpoint(candidate)
            ));

            match run_stream_session(
                candidate.clone(),
                transport_mode,
                target_fps,
                &tile,
                geometry_rx.clone(),
            )
            .await
            {
                Ok(()) => {
                    session_ended = true;
                    break;
                }
                Err(err) => {
                    last_error = Some((candidate.clone(), err));
                }
            }
        }

        if session_ended {
            retry_delay_secs = 1;
            tile.set_status("stream ended, reconnecting");
        } else if let Some((url, err)) = last_error {
            tile.set_status(format!(
                "error: all endpoints failed (last: {}): {err:#}",
                display_endpoint(&url)
            ));
        } else {
            tile.set_status("error: no candidate endpoints available");
        }

        tokio::time::sleep(Duration::from_secs(retry_delay_secs)).await;
        retry_delay_secs = (retry_delay_secs.saturating_mul(2)).min(10);
    }
}

fn build_stream_candidates(primary_url: &str) -> Vec<String> {
    let Ok(mut primary) = Url::parse(primary_url) else {
        return vec![primary_url.to_owned()];
    };
    let username = primary.username().to_owned();
    let password = primary.password().unwrap_or("").to_owned();
    let _ = primary.set_username("");
    let _ = primary.set_password(None);

    let host = primary.host_str().unwrap_or("").to_owned();
    let port = primary.port_or_known_default().unwrap_or(554);
    let primary_no_auth = primary.to_string();

    let mut discovered_candidates = cache::load_cache()
        .map(|cache| {
            cache
                .streams
                .into_iter()
                .filter_map(|stream| {
                    let Ok(candidate) = Url::parse(&stream.url) else {
                        return None;
                    };
                    let same_host = candidate.host_str().unwrap_or("") == host;
                    let same_port = candidate.port_or_known_default().unwrap_or(554) == port;
                    if !same_host || !same_port {
                        return None;
                    }
                    Some((strip_auth_url(candidate), stream.status))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    discovered_candidates.push((primary_no_auth, 200));
    discovered_candidates.sort_by_key(|candidate| std::cmp::Reverse(endpoint_rank(candidate)));

    let mut seen = HashSet::new();
    let mut final_urls = Vec::new();
    for (no_auth_url, _) in discovered_candidates {
        if !seen.insert(no_auth_url.clone()) {
            continue;
        }
        final_urls.push(apply_rtsp_credentials(
            &no_auth_url,
            username.as_str(),
            password.as_str(),
        ));
    }

    if final_urls.is_empty() {
        vec![primary_url.to_owned()]
    } else {
        final_urls
    }
}

fn strip_auth_url(mut parsed: Url) -> String {
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.to_string()
}

fn apply_rtsp_credentials(url: &str, username: &str, password: &str) -> String {
    let Ok(mut parsed) = Url::parse(url) else {
        return url.to_owned();
    };
    if username.is_empty() {
        return parsed.to_string();
    }

    if parsed.set_username(username).is_err() {
        return parsed.to_string();
    }
    if password.is_empty() {
        let _ = parsed.set_password(None);
    } else {
        let _ = parsed.set_password(Some(password));
    }
    parsed.to_string()
}

fn endpoint_rank(candidate: &(String, u16)) -> i32 {
    let (url, status) = candidate;
    let hint = endpoint_hint(url);

    let status_score = match *status {
        200 => 1000,
        401 => 650,
        _ => 0,
    };

    let path_score = if hint.contains("/streaming/channels/101") {
        240
    } else if hint.contains("/cam/realmonitor?channel=1&subtype=0") {
        220
    } else if hint.contains("/h264preview_01_main") {
        200
    } else if hint == "/" {
        30
    } else if hint.contains("/live") {
        170
    } else if hint.contains("/stream1") || hint.contains("/h264") {
        150
    } else {
        120 - i32::try_from(hint.len().min(80)).unwrap_or(80)
    };

    status_score + path_score
}

fn endpoint_hint(url: &str) -> String {
    let Ok(parsed) = Url::parse(url) else {
        return "/".to_owned();
    };
    let mut hint = parsed.path().to_ascii_lowercase();
    if let Some(query) = parsed.query()
        && !query.is_empty()
    {
        hint.push('?');
        hint.push_str(&query.to_ascii_lowercase());
    }
    if hint.is_empty() {
        "/".to_owned()
    } else {
        hint
    }
}

fn display_endpoint(url: &str) -> String {
    let Ok(parsed) = Url::parse(url) else {
        return url.to_owned();
    };
    let host = parsed.host_str().unwrap_or("unknown");
    let port = parsed.port_or_known_default().unwrap_or(554);
    let mut endpoint = parsed.path().to_owned();
    if let Some(query) = parsed.query()
        && !query.is_empty()
    {
        endpoint.push('?');
        endpoint.push_str(query);
    }
    format!("{host}:{port}{endpoint}")
}

async fn run_stream_session(
    stream_url: String,
    transport_mode: TransportMode,
    target_fps: u16,
    tile: &TileState,
    geometry_rx: watch::Receiver<CellGeometry>,
) -> Result<()> {
    let mut parsed =
        Url::parse(&stream_url).with_context(|| format!("invalid RTSP URL: {stream_url}"))?;

    let creds = extract_credentials(&parsed);
    if parsed.username() != "" {
        parsed
            .set_username("")
            .map_err(|()| anyhow!("failed stripping username from RTSP URL"))?;
    }
    if parsed.password().is_some() {
        parsed
            .set_password(None)
            .map_err(|()| anyhow!("failed stripping password from RTSP URL"))?;
    }

    let mut session_options = SessionOptions::default();
    if let Some(credentials) = creds {
        session_options = session_options.creds(Some(credentials));
    }

    let mut session = Session::describe(parsed, session_options)
        .await
        .context("RTSP DESCRIBE failed")?;

    let stream_index = pick_h264_stream(&session)?;
    session
        .setup(
            stream_index,
            SetupOptions::default().transport(to_transport(transport_mode)),
        )
        .await
        .context("RTSP SETUP failed")?;

    let playing = session
        .play(PlayOptions::default())
        .await
        .context("RTSP PLAY failed")?;

    let mut demuxed = playing.demuxed().context("RTSP demux setup failed")?;
    let mut decoder = Decoder::new().context("failed to initialize H264 decoder")?;

    if let Some(extra_config) = read_h264_extra_config(&demuxed, stream_index) {
        let _ = decoder.decode(&extra_config);
    }

    let mut frame_buffer = Vec::with_capacity(4096);
    let target_interval = Duration::from_millis(1_000_u64 / u64::from(target_fps.max(1)));
    let mut last_render = Instant::now()
        .checked_sub(target_interval)
        .unwrap_or_else(Instant::now);

    tile.set_status("streaming");

    while let Some(item) = demuxed.next().await {
        let item = item.context("demux receive failed")?;

        let CodecItem::VideoFrame(frame) = item else {
            continue;
        };
        if frame.stream_id() != stream_index {
            continue;
        }

        if frame.has_new_parameters()
            && let Some(extra_config) = read_h264_extra_config(&demuxed, stream_index)
        {
            let _ = decoder.decode(&extra_config);
        }

        if let Err(err) = avcc_frame_to_annexb(frame.data(), &mut frame_buffer) {
            tile.inc_decode_error();
            tile.set_status(format!("frame convert error: {err:#}"));
            continue;
        }

        match decoder.decode(&frame_buffer) {
            Ok(Some(yuv)) => {
                if last_render.elapsed() < target_interval {
                    continue;
                }

                let geometry = *geometry_rx.borrow();
                let (src_w, src_h) = yuv.dimensions();
                let (stride_y, _, _) = yuv.strides();
                let ascii = luma_to_ascii(
                    yuv.y(),
                    src_w,
                    src_h,
                    stride_y,
                    geometry.width,
                    geometry.height,
                    geometry.vertical_scale,
                );
                tile.set_frame(ascii);
                last_render = Instant::now();
            }
            Ok(None) => {}
            Err(err) => {
                tile.inc_decode_error();
                tile.set_status(format!("decode error: {err}"));
            }
        }
    }

    Ok(())
}

fn extract_credentials(parsed: &Url) -> Option<Credentials> {
    if parsed.username().is_empty() {
        return None;
    }

    Some(Credentials {
        username: percent_decode_userinfo(parsed.username()),
        password: percent_decode_userinfo(parsed.password().unwrap_or("")),
    })
}

fn percent_decode_userinfo(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut idx = 0;

    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[idx + 1]), hex_value(bytes[idx + 2]))
        {
            decoded.push((hi << 4) | lo);
            idx += 3;
            continue;
        }

        decoded.push(bytes[idx]);
        idx += 1;
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn pick_h264_stream(session: &Session<retina::client::Described>) -> Result<usize> {
    session
        .streams()
        .iter()
        .enumerate()
        .find(|(_, stream)| stream.media() == "video" && stream.encoding_name() == "h264")
        .map(|(idx, _)| idx)
        .ok_or_else(|| anyhow!("no H264 video stream found in RTSP presentation"))
}

fn to_transport(mode: TransportMode) -> Transport {
    match mode {
        TransportMode::Tcp => Transport::Tcp(TcpTransportOptions::default()),
        TransportMode::Udp => Transport::Udp(UdpTransportOptions::default()),
    }
}

fn read_h264_extra_config(
    demuxed: &retina::client::Demuxed,
    stream_index: usize,
) -> Option<Vec<u8>> {
    let stream = demuxed.streams().get(stream_index)?;
    let ParametersRef::Video(video_params) = stream.parameters()? else {
        return None;
    };

    avcc_extra_data_to_annexb(video_params.extra_data()).ok()
}

fn avcc_frame_to_annexb(input: &[u8], output: &mut Vec<u8>) -> Result<()> {
    output.clear();

    let mut cursor = 0_usize;
    while cursor + 4 <= input.len() {
        let nal_len = u32::from_be_bytes([
            input[cursor],
            input[cursor + 1],
            input[cursor + 2],
            input[cursor + 3],
        ]) as usize;
        cursor += 4;

        if cursor + nal_len > input.len() {
            return Err(anyhow!("invalid AVCC frame: NAL length exceeds payload"));
        }

        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(&input[cursor..cursor + nal_len]);
        cursor += nal_len;
    }

    if cursor != input.len() {
        return Err(anyhow!("invalid AVCC frame: trailing bytes"));
    }

    Ok(())
}

fn avcc_extra_data_to_annexb(extra: &[u8]) -> Result<Vec<u8>> {
    if extra.len() < 7 {
        return Err(anyhow!("AVCC extradata too short"));
    }

    let mut cursor = 5_usize;
    let mut output = Vec::with_capacity(extra.len() + 32);

    let sps_count = usize::from(extra[cursor] & 0x1f);
    cursor += 1;

    for _ in 0..sps_count {
        if cursor + 2 > extra.len() {
            return Err(anyhow!("AVCC SPS length missing"));
        }
        let len = usize::from(u16::from_be_bytes([extra[cursor], extra[cursor + 1]]));
        cursor += 2;
        if cursor + len > extra.len() {
            return Err(anyhow!("AVCC SPS payload exceeds size"));
        }

        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(&extra[cursor..cursor + len]);
        cursor += len;
    }

    if cursor >= extra.len() {
        return Err(anyhow!("AVCC PPS count missing"));
    }

    let pps_count = usize::from(extra[cursor]);
    cursor += 1;

    for _ in 0..pps_count {
        if cursor + 2 > extra.len() {
            return Err(anyhow!("AVCC PPS length missing"));
        }
        let len = usize::from(u16::from_be_bytes([extra[cursor], extra[cursor + 1]]));
        cursor += 2;
        if cursor + len > extra.len() {
            return Err(anyhow!("AVCC PPS payload exceeds size"));
        }

        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(&extra[cursor..cursor + len]);
        cursor += len;
    }

    Ok(output)
}

fn luma_to_ascii(
    y_plane: &[u8],
    src_width: usize,
    src_height: usize,
    src_stride: usize,
    target_width: usize,
    target_height: usize,
    vertical_scale: usize,
) -> String {
    if src_width == 0 || src_height == 0 || target_width == 0 || target_height == 0 {
        return "no frame".to_owned();
    }

    let mut out = String::with_capacity(target_width.saturating_mul(target_height + 1));
    let scale = vertical_scale.max(1);
    let row_den = target_height.saturating_sub(1).max(1);
    let max_src_row = src_height.saturating_sub(1);

    for ty in 0..target_height {
        let sy_base = if target_height <= 1 {
            0
        } else {
            ty.saturating_mul(max_src_row) / row_den
        };

        for tx in 0..target_width {
            let sx = (tx.saturating_mul(src_width) / target_width).min(src_width.saturating_sub(1));

            let mut sum = 0_u32;
            let mut samples = 0_u32;
            for offset in 0..scale {
                let sy = sy_base.saturating_add(offset).min(max_src_row);
                if let Some(v) = y_plane.get(sy.saturating_mul(src_stride).saturating_add(sx)) {
                    sum = sum.saturating_add(u32::from(*v));
                    samples = samples.saturating_add(1);
                }
            }
            let lum = if samples == 0 {
                0
            } else {
                u8::try_from(sum / samples).unwrap_or(u8::MAX)
            };
            let ramp_index =
                usize::from(lum).saturating_mul(BRIGHTNESS_RAMP.len().saturating_sub(1)) / 255;
            out.push(char::from(BRIGHTNESS_RAMP[ramp_index]));
        }

        if ty + 1 < target_height {
            out.push('\n');
        }
    }

    out
}

fn compute_grid_dimensions(count: usize, rows: u16, cols: u16) -> (usize, usize) {
    let count = count.max(1);

    let mut resolved_rows = usize::from(rows);
    let mut resolved_cols = usize::from(cols);

    match (resolved_rows, resolved_cols) {
        (0, 0) => {
            resolved_cols = ceil_sqrt(count);
            resolved_cols = resolved_cols.max(1);
            resolved_rows = count.div_ceil(resolved_cols);
        }
        (0, c) => {
            resolved_cols = c.max(1);
            resolved_rows = count.div_ceil(resolved_cols);
        }
        (r, 0) => {
            resolved_rows = r.max(1);
            resolved_cols = count.div_ceil(resolved_rows);
        }
        (r, c) => {
            resolved_rows = r.max(1);
            resolved_cols = c.max(1);
            while resolved_rows.saturating_mul(resolved_cols) < count {
                resolved_rows = resolved_rows.saturating_add(1);
            }
        }
    }

    (resolved_rows, resolved_cols)
}

fn ceil_sqrt(value: usize) -> usize {
    let mut root = 1_usize;
    while root.saturating_mul(root) < value {
        root = root.saturating_add(1);
    }
    root
}

fn build_grid_rects(area: Rect, rows: usize, cols: usize) -> Vec<Rect> {
    let row_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Fill(1); rows.max(1)])
        .split(area);

    let mut rects = Vec::with_capacity(rows.saturating_mul(cols));
    for row_area in row_chunks.iter().copied() {
        let col_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![Constraint::Fill(1); cols.max(1)])
            .split(row_area);
        rects.extend(col_chunks.iter().copied());
    }

    rects
}

fn inner_cell(cell: Rect) -> Rect {
    Rect {
        x: cell.x.saturating_add(1),
        y: cell.y.saturating_add(1),
        width: cell.width.saturating_sub(2),
        height: cell.height.saturating_sub(2),
    }
}
