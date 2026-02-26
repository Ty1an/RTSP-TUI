use super::helpers::{camera_ip_key, percent_decode_userinfo};
use crate::cache::SelectedStream;
use crate::cli::TransportMode;
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_ENGINE;
use fast_image_resize as fir;
use futures_util::StreamExt;
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use parking_lot::{Mutex, RwLock};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use retina::client::{
    Credentials, PlayOptions, Session, SessionOptions, SetupOptions, TcpTransportOptions,
    Transport, UdpTransportOptions,
};
use retina::codec::{CodecItem, ParametersRef};
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use url::Url;

const DELTA_MAX_AREA_PERCENT: usize = 60;
const LIVE_TARGET_FPS_CAP: u16 = 20;
const DELTA_MOTION_SAMPLE_COLS: usize = 16;
const DELTA_MOTION_SAMPLE_ROWS: usize = 10;
const DELTA_MOTION_PIXEL_DIFF_THRESHOLD: u16 = 72;
const DELTA_MOTION_SKIP_PERCENT: usize = 75;
const DECODE_QUEUE_CAPACITY: usize = 8;
const UPLOAD_PREP_QUEUE_CAPACITY: usize = 6;
const MIN_RENDER_WIDTH: usize = 96;
const MIN_RENDER_HEIGHT: usize = 54;
const MAX_RENDER_WIDTH_MANY: usize = 640;
const MAX_RENDER_HEIGHT_MANY: usize = 360;
const MAX_RENDER_WIDTH_MEDIUM: usize = 960;
const MAX_RENDER_HEIGHT_MEDIUM: usize = 540;
const MAX_RENDER_WIDTH_FEW: usize = 1280;
const MAX_RENDER_HEIGHT_FEW: usize = 720;
const STREAM_MODE_GEOMETRY_SCALE: f32 = 1.0;
const RGB_SIMD_PIXELS: usize = 16;
const RGB_SIMD_BYTES: usize = RGB_SIMD_PIXELS * 3;
const RGB_SIMD_RESIZE_MIN_AREA_PERCENT: usize = 80;

static RENDER_SCALE_OVERRIDE: OnceLock<f32> = OnceLock::new();
static STREAM_MODE_SCALE_OVERRIDE: OnceLock<f32> = OnceLock::new();

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::{
    veorq_u8, vget_high_u8, vget_low_u8, vld1q_u8, vmaxv_u8, vorr_u8, vorrq_u8,
};
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{_mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderGeometry {
    pub(crate) width: usize,
    pub(crate) height: usize,
}

impl Default for RenderGeometry {
    fn default() -> Self {
        Self {
            width: 320,
            height: 180,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KittyTransferMode {
    Stream,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelRect {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

#[derive(Debug, Clone)]
struct ScaleAxisLut {
    src0: Vec<usize>,
    src1: Vec<usize>,
    w1: Vec<u16>,
}

#[derive(Debug, Clone)]
struct Yuv420ScalePlan {
    src_width: usize,
    src_height: usize,
    output_width: usize,
    output_height: usize,
    y_x: ScaleAxisLut,
    y_y: ScaleAxisLut,
    uv_x: ScaleAxisLut,
    uv_y: ScaleAxisLut,
}

struct RgbResizeState {
    resizer: fir::Resizer,
    options: fir::ResizeOptions,
    src_rgb: Vec<u8>,
}

impl Default for RgbResizeState {
    fn default() -> Self {
        Self {
            resizer: fir::Resizer::new(),
            options: fir::ResizeOptions::new()
                .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::Bilinear)),
            src_rgb: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct KittyFileTransfer {
    pub(crate) path: PathBuf,
    pub(crate) encoded_path: String,
    pub(crate) file: File,
    pub(crate) size: usize,
}

impl Drop for KittyFileTransfer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
pub(crate) struct UploadPrepJob {
    pub(crate) frame_seq: u64,
    pub(crate) frame_width: usize,
    pub(crate) frame_height: usize,
    pub(crate) frame_rgb: Arc<Vec<u8>>,
}

#[derive(Debug)]
struct LatestFrame {
    seq: u64,
    width: usize,
    height: usize,
    rgb: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadPrepKind {
    Unchanged,
    Full,
    Delta { rect: PixelRect },
}

#[derive(Debug)]
pub(crate) struct UploadPrepResult {
    pub(crate) frame_seq: u64,
    pub(crate) frame_width: usize,
    pub(crate) frame_height: usize,
    pub(crate) kind: UploadPrepKind,
    pub(crate) encoded_payload: String,
}

#[derive(Debug)]
pub(crate) struct UploadPrepWorker {
    pub(crate) tx: SyncSender<UploadPrepJob>,
    pub(crate) rx: Receiver<UploadPrepResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadPrepSendOutcome {
    Queued,
    Dropped,
    Disconnected,
}

#[derive(Debug)]
pub(crate) struct KittyUploadState {
    pub(crate) source_frame_seq: u64,
    pub(crate) source_frame_width: usize,
    pub(crate) source_frame_height: usize,
    pub(crate) source_frame_rgb: Option<Arc<Vec<u8>>>,
    pub(crate) source_frame_scratch: Vec<u8>,
    pub(crate) row: u32,
    pub(crate) col: u32,
    pub(crate) cell_cols: u16,
    pub(crate) cell_rows: u16,
    pub(crate) frame_width: usize,
    pub(crate) frame_height: usize,
    pub(crate) encoded_seq: u64,
    pub(crate) queued_seq: u64,
    pub(crate) encoded_payload: String,
    pub(crate) control_buf: String,
    pub(crate) prepared: Option<UploadPrepResult>,
    pub(crate) upload_worker: UploadPrepWorker,
    pub(crate) next_upload_at: Option<Instant>,
    pub(crate) file_transfer: Option<KittyFileTransfer>,
}

impl Default for KittyUploadState {
    fn default() -> Self {
        Self::new()
    }
}

impl KittyUploadState {
    pub(crate) fn new() -> Self {
        Self {
            source_frame_seq: 0,
            source_frame_width: 0,
            source_frame_height: 0,
            source_frame_rgb: None,
            source_frame_scratch: Vec::new(),
            row: 0,
            col: 0,
            cell_cols: 0,
            cell_rows: 0,
            frame_width: 0,
            frame_height: 0,
            encoded_seq: u64::MAX,
            queued_seq: u64::MAX,
            encoded_payload: String::new(),
            control_buf: String::new(),
            prepared: None,
            upload_worker: spawn_upload_prep_worker(),
            next_upload_at: None,
            file_transfer: None,
        }
    }

    pub(crate) fn ensure_file_transfer(&mut self, tile_idx: usize) -> Result<()> {
        if self.file_transfer.is_some() {
            return Ok(());
        }

        let path = std::env::temp_dir().join(format!(
            "rtsp-tui-kitty-tty-graphics-protocol-{}-{tile_idx}.rgb",
            std::process::id()
        ));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed creating kitty image file at {}", path.display()))?;
        let mut encoded_path = String::new();
        BASE64_ENGINE.encode_string(path.to_string_lossy().as_bytes(), &mut encoded_path);
        self.file_transfer = Some(KittyFileTransfer {
            path,
            encoded_path,
            file,
            size: 0,
        });
        Ok(())
    }
}

fn spawn_upload_prep_worker() -> UploadPrepWorker {
    let (job_tx, job_rx) = mpsc::sync_channel::<UploadPrepJob>(UPLOAD_PREP_QUEUE_CAPACITY);
    let (result_tx, result_rx) = mpsc::sync_channel::<UploadPrepResult>(UPLOAD_PREP_QUEUE_CAPACITY);

    let _ = std::thread::Builder::new()
        .name("kitty-upload-prep".to_owned())
        .spawn(move || {
            let mut previous_rgb: Option<Arc<Vec<u8>>> = None;
            let mut previous_width = 0_usize;
            let mut previous_height = 0_usize;
            let mut delta_rgb = Vec::new();

            while let Ok(job) = job_rx.recv() {
                let current_rgb = job.frame_rgb.as_slice();
                let previous_slice = previous_rgb.as_ref().map(|rgb| rgb.as_slice());
                let can_try_delta = previous_width == job.frame_width
                    && previous_height == job.frame_height
                    && previous_slice
                        .map(|rgb| rgb.len() == current_rgb.len())
                        .unwrap_or(false);
                let skip_precise_delta_scan = can_try_delta
                    && is_probably_high_motion(
                        previous_slice.unwrap_or(&[]),
                        current_rgb,
                        job.frame_width,
                        job.frame_height,
                    );
                let changed_rect = if can_try_delta && !skip_precise_delta_scan {
                    compute_changed_rect(
                        previous_slice.unwrap_or(&[]),
                        current_rgb,
                        job.frame_width,
                        job.frame_height,
                    )
                } else {
                    None
                };

                let kind = if can_try_delta && !skip_precise_delta_scan && changed_rect.is_none() {
                    UploadPrepKind::Unchanged
                } else if skip_precise_delta_scan {
                    UploadPrepKind::Full
                } else if let Some(rect) = changed_rect {
                    let rect_area = rect.width.saturating_mul(rect.height);
                    let full_area = job.frame_width.saturating_mul(job.frame_height).max(1);
                    if rect_area.saturating_mul(100)
                        < full_area.saturating_mul(DELTA_MAX_AREA_PERCENT)
                    {
                        UploadPrepKind::Delta { rect }
                    } else {
                        UploadPrepKind::Full
                    }
                } else {
                    UploadPrepKind::Full
                };

                let mut encoded_payload = String::new();
                match kind {
                    UploadPrepKind::Unchanged => {}
                    UploadPrepKind::Full => {
                        BASE64_ENGINE.encode_string(current_rgb, &mut encoded_payload);
                    }
                    UploadPrepKind::Delta { rect } => {
                        extract_rgb_rect(current_rgb, job.frame_width, rect, &mut delta_rgb);
                        BASE64_ENGINE.encode_string(&delta_rgb, &mut encoded_payload);
                    }
                }

                previous_rgb = Some(job.frame_rgb);
                previous_width = job.frame_width;
                previous_height = job.frame_height;

                let prepared = UploadPrepResult {
                    frame_seq: job.frame_seq,
                    frame_width: job.frame_width,
                    frame_height: job.frame_height,
                    kind,
                    encoded_payload,
                };
                if result_tx.send(prepared).is_err() {
                    break;
                }
            }
        });

    UploadPrepWorker {
        tx: job_tx,
        rx: result_rx,
    }
}

pub(crate) fn queue_upload_prep_job(
    tx: &SyncSender<UploadPrepJob>,
    job: UploadPrepJob,
) -> UploadPrepSendOutcome {
    match tx.try_send(job) {
        Ok(()) => UploadPrepSendOutcome::Queued,
        Err(TrySendError::Full(_)) => UploadPrepSendOutcome::Dropped,
        Err(TrySendError::Disconnected(_)) => UploadPrepSendOutcome::Disconnected,
    }
}

enum DecodeJob {
    Reset,
    ExtraConfig(Vec<u8>),
    FrameAvcc {
        avcc: Vec<u8>,
        geometry: RenderGeometry,
    },
}

#[derive(Debug)]
struct PendingFrameDecode {
    avcc: Vec<u8>,
    geometry: RenderGeometry,
}

impl DecodeJob {
    fn should_block_when_full(&self) -> bool {
        match self {
            Self::Reset | Self::ExtraConfig(_) => true,
            Self::FrameAvcc { .. } => false,
        }
    }
}

fn absorb_decode_job(
    job: DecodeJob,
    pending_reset: &mut bool,
    pending_extra: &mut Option<Vec<u8>>,
    pending_frame: &mut Option<PendingFrameDecode>,
    tile: &EmbeddedTileState,
) {
    match job {
        DecodeJob::Reset => {
            if pending_frame.take().is_some() {
                tile.inc_dropped_frame();
            }
            *pending_reset = true;
            *pending_extra = None;
        }
        DecodeJob::ExtraConfig(extra) => {
            *pending_extra = Some(extra);
        }
        DecodeJob::FrameAvcc { avcc, geometry } => {
            if pending_frame
                .replace(PendingFrameDecode { avcc, geometry })
                .is_some()
            {
                tile.inc_dropped_frame();
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecodeJobSendOutcome {
    Queued,
    Dropped,
    Disconnected,
}

#[derive(Debug)]
pub(crate) struct EmbeddedTileSnapshot {
    pub(crate) stream_label: String,
    pub(crate) frame_seq: u64,
    pub(crate) status: String,
}

impl EmbeddedTileSnapshot {
    pub(crate) fn new(stream_label: String) -> Self {
        Self {
            stream_label,
            frame_seq: 0,
            status: "connecting".to_owned(),
        }
    }
}

pub(crate) struct EmbeddedTileState {
    pub(crate) inner: RwLock<EmbeddedTileSnapshot>,
    latest_frame: Mutex<Option<LatestFrame>>,
    recycle_frame: Mutex<Vec<u8>>,
}

impl EmbeddedTileState {
    pub(crate) fn new(stream_label: String) -> Self {
        Self {
            inner: RwLock::new(EmbeddedTileSnapshot::new(stream_label)),
            latest_frame: Mutex::new(None),
            recycle_frame: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn set_status(&self, status: impl Into<String>) {
        self.inner.write().status = status.into();
    }

    pub(crate) fn set_frame(
        &self,
        frame_rgb: &mut Vec<u8>,
        frame_width: usize,
        frame_height: usize,
    ) {
        let frame_seq = {
            let mut snapshot = self.inner.write();
            snapshot.frame_seq = snapshot.frame_seq.saturating_add(1);
            "streaming".clone_into(&mut snapshot.status);
            snapshot.frame_seq
        };

        let produced_rgb = std::mem::take(frame_rgb);
        let mut latest = self.latest_frame.lock();
        if let Some(mut previous) = latest.replace(LatestFrame {
            seq: frame_seq,
            width: frame_width,
            height: frame_height,
            rgb: produced_rgb,
        }) {
            std::mem::swap(frame_rgb, &mut previous.rgb);
            return;
        }
        drop(latest);

        let mut recycle = self.recycle_frame.lock();
        std::mem::swap(frame_rgb, &mut recycle);
    }

    pub(crate) fn take_latest_frame(&self, out_rgb: &mut Vec<u8>) -> Option<(u64, usize, usize)> {
        let mut latest = self.latest_frame.lock();
        let mut frame = latest.take()?;
        std::mem::swap(out_rgb, &mut frame.rgb);
        drop(latest);

        let mut recycle = self.recycle_frame.lock();
        std::mem::swap(&mut *recycle, &mut frame.rgb);
        Some((frame.seq, frame.width, frame.height))
    }

    pub(crate) fn inc_decode_error(&self) {}

    pub(crate) fn inc_dropped_frame(&self) {}
}

pub(crate) async fn run_embedded_stream_worker(
    stream_url: String,
    transport_mode: TransportMode,
    target_fps: u16,
    tile: Arc<EmbeddedTileState>,
    geometry_rx: watch::Receiver<RenderGeometry>,
) {
    let auth_user = Url::parse(&stream_url)
        .ok()
        .map(|u| u.username().to_owned())
        .unwrap_or_default();
    let has_auth = !auth_user.trim().is_empty();
    let mut retry_delay = Duration::from_millis(300);
    let mut software_decode_tx: Option<SyncSender<DecodeJob>> = None;

    loop {
        tile.set_status(format!("connecting: {}", display_endpoint(&stream_url)));
        let decode_tx = software_decode_tx
            .get_or_insert_with(|| spawn_stream_decoder_thread(tile.clone(), target_fps));
        let outcome = run_embedded_stream_session(
            stream_url.clone(),
            transport_mode,
            target_fps,
            tile.clone(),
            geometry_rx.clone(),
            decode_tx,
        )
        .await;

        if outcome.is_ok() {
            retry_delay = Duration::from_millis(300);
            tile.set_status("stream ended, reconnecting");
        } else {
            let err = outcome.expect_err("checked is_ok above");
            let err_text = format!("{err:#}");
            if err_text.contains("Unauthorized") {
                if has_auth {
                    tile.set_status(format!(
                        "error: unauthorized using '{}'. Check username/password for this camera.",
                        auth_user
                    ));
                } else {
                    tile.set_status("error: unauthorized. Set camera auth in Settings.");
                }
                retry_delay = Duration::from_secs(3);
            } else {
                tile.set_status(format!("reconnecting: {}", err));
                let next_ms = u64::try_from(retry_delay.as_millis())
                    .unwrap_or(u64::MAX)
                    .saturating_mul(3)
                    .saturating_div(2)
                    .min(2_000);
                retry_delay = Duration::from_millis(next_ms.max(300));
            }
        }

        tokio::time::sleep(retry_delay).await;
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

pub(crate) fn display_stream_label(stream: &SelectedStream) -> String {
    if let Some(display_name) = stream.display_name.as_deref() {
        let trimmed = display_name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    camera_ip_key(&stream.base_url).unwrap_or_else(|| stream.base_url.clone())
}

async fn run_embedded_stream_session(
    stream_url: String,
    transport_mode: TransportMode,
    _target_fps: u16,
    tile: Arc<EmbeddedTileState>,
    geometry_rx: watch::Receiver<RenderGeometry>,
    decode_tx: &SyncSender<DecodeJob>,
) -> Result<()> {
    let parsed_with_auth =
        Url::parse(&stream_url).with_context(|| format!("invalid RTSP URL: {stream_url}"))?;
    let mut parsed = parsed_with_auth.clone();

    let creds = extract_credentials(&parsed);
    if !parsed.username().is_empty() {
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

    let mut session = match Session::describe(parsed.clone(), session_options).await {
        Ok(session) => session,
        Err(primary_err) => {
            // Some cameras accept credentials only when embedded in the RTSP URL.
            // Retry once with URL userinfo preserved before surfacing failure.
            if parsed_with_auth.username().is_empty() {
                return Err(primary_err).context("RTSP DESCRIBE failed");
            }

            match Session::describe(parsed_with_auth, SessionOptions::default()).await {
                Ok(session) => session,
                Err(fallback_err) => {
                    return Err(anyhow!(
                        "RTSP DESCRIBE failed (session creds): {primary_err:#}; fallback failed (URL creds): {fallback_err:#}"
                    ));
                }
            }
        }
    };

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
    if send_decode_job(decode_tx, DecodeJob::Reset, true) != DecodeJobSendOutcome::Queued {
        return Err(anyhow!("decode thread unavailable"));
    }

    if let Some(extra_config) = read_h264_extra_config(&demuxed, stream_index) {
        if send_decode_job(decode_tx, DecodeJob::ExtraConfig(extra_config), true)
            != DecodeJobSendOutcome::Queued
        {
            return Err(anyhow!("decode thread unavailable"));
        }
    }

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
            if send_decode_job(decode_tx, DecodeJob::ExtraConfig(extra_config), true)
                == DecodeJobSendOutcome::Disconnected
            {
                return Err(anyhow!("decode thread unavailable"));
            }
        }

        let payload = frame.into_data();
        let geometry = *geometry_rx.borrow();
        match send_decode_job(
            decode_tx,
            DecodeJob::FrameAvcc {
                avcc: payload,
                geometry,
            },
            false,
        ) {
            DecodeJobSendOutcome::Queued => {}
            DecodeJobSendOutcome::Dropped => {
                tile.inc_dropped_frame();
            }
            DecodeJobSendOutcome::Disconnected => {
                return Err(anyhow!("decode thread unavailable"));
            }
        }
    }

    Ok(())
}

fn spawn_stream_decoder_thread(
    tile: Arc<EmbeddedTileState>,
    target_fps: u16,
) -> SyncSender<DecodeJob> {
    let (tx, rx) = mpsc::sync_channel::<DecodeJob>(DECODE_QUEUE_CAPACITY);
    let thread_tile = tile;
    let target_interval = if target_fps == 0 {
        None
    } else {
        Some(Duration::from_millis(
            1_000_u64 / u64::from(target_fps.max(1)),
        ))
    };

    let _ = std::thread::Builder::new()
        .name("rtsp-decode".to_owned())
        .spawn(move || {
            let mut decoder = create_stream_decoder(&thread_tile);
            let mut annexb_buffer = Vec::with_capacity(4096);
            let mut rgb_buffer = Vec::new();
            let mut scale_plan = None;
            let mut resize_state = RgbResizeState::default();
            let mut last_present = Instant::now();

            loop {
                let Ok(first_job) = rx.recv() else {
                    break;
                };

                let mut pending_reset = false;
                let mut pending_extra = None;
                let mut pending_frame = None;
                absorb_decode_job(
                    first_job,
                    &mut pending_reset,
                    &mut pending_extra,
                    &mut pending_frame,
                    &thread_tile,
                );
                while let Ok(next_job) = rx.try_recv() {
                    absorb_decode_job(
                        next_job,
                        &mut pending_reset,
                        &mut pending_extra,
                        &mut pending_frame,
                        &thread_tile,
                    );
                }

                if pending_reset {
                    decoder = create_stream_decoder(&thread_tile);
                    annexb_buffer.clear();
                    rgb_buffer.clear();
                    scale_plan = None;
                    resize_state.src_rgb.clear();
                    last_present = Instant::now();
                }

                if let Some(extra) = pending_extra {
                    let _ = decoder.decode(&extra);
                }

                if let Some(PendingFrameDecode { avcc, geometry }) = pending_frame {
                    if let Err(_err) = avcc_frame_to_annexb(&avcc, &mut annexb_buffer) {
                        thread_tile.inc_decode_error();
                        continue;
                    }

                    match decoder.decode(&annexb_buffer) {
                        Ok(Some(yuv)) => {
                            if let Some(target_interval) = target_interval
                                && last_present.elapsed() < target_interval
                            {
                                thread_tile.inc_dropped_frame();
                                continue;
                            }
                            let (src_w, src_h) = yuv.dimensions();
                            if let Some((resolved_w, resolved_h)) =
                                resolved_output_dims(src_w, src_h, geometry.width, geometry.height)
                                && resolved_w == src_w
                                && resolved_h == src_h
                            {
                                let needed = yuv.rgb8_len();
                                rgb_buffer.resize(needed, 0);
                                // openh264's built-in converter uses SIMD where available.
                                yuv.write_rgb8(&mut rgb_buffer);
                                thread_tile.set_frame(&mut rgb_buffer, src_w, src_h);
                                if target_interval.is_some() {
                                    last_present = Instant::now();
                                }
                                continue;
                            }

                            if let Some((resolved_w, resolved_h)) =
                                resolved_output_dims(src_w, src_h, geometry.width, geometry.height)
                                && should_use_rgb_simd_resize(src_w, src_h, resolved_w, resolved_h)
                            {
                                let src_needed = yuv.rgb8_len();
                                resize_state.src_rgb.resize(src_needed, 0);
                                yuv.write_rgb8(&mut resize_state.src_rgb);
                                if resize_rgb_simd(
                                    &mut resize_state,
                                    src_w,
                                    src_h,
                                    resolved_w,
                                    resolved_h,
                                    &mut rgb_buffer,
                                )
                                .is_ok()
                                {
                                    thread_tile.set_frame(&mut rgb_buffer, resolved_w, resolved_h);
                                    if target_interval.is_some() {
                                        last_present = Instant::now();
                                    }
                                    continue;
                                }
                            }

                            let (stride_y, stride_u, stride_v) = yuv.strides();
                            let (out_w, out_h) = yuv420_to_rgb_scaled(
                                yuv.y(),
                                yuv.u(),
                                yuv.v(),
                                src_w,
                                src_h,
                                stride_y,
                                stride_u,
                                stride_v,
                                geometry.width,
                                geometry.height,
                                &mut scale_plan,
                                &mut rgb_buffer,
                            );
                            thread_tile.set_frame(&mut rgb_buffer, out_w, out_h);
                            if target_interval.is_some() {
                                last_present = Instant::now();
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            thread_tile.inc_decode_error();
                            if err.to_string().to_ascii_lowercase().contains("fatal") {
                                thread_tile.set_status(format!("decode error: {err}"));
                            }
                        }
                    }
                }
            }
        });

    tx
}

pub(crate) fn live_target_fps_for_stream_count(_stream_count: usize) -> u16 {
    LIVE_TARGET_FPS_CAP
}

fn create_stream_decoder(tile: &EmbeddedTileState) -> Decoder {
    if let Ok(decoder) = Decoder::new() {
        return decoder;
    }

    loop {
        match Decoder::new() {
            Ok(decoder) => return decoder,
            Err(err) => {
                tile.inc_decode_error();
                tile.set_status(format!("decode init error: {err}"));
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn send_decode_job(
    tx: &SyncSender<DecodeJob>,
    job: DecodeJob,
    allow_blocking_when_full: bool,
) -> DecodeJobSendOutcome {
    let should_block_when_full = allow_blocking_when_full || job.should_block_when_full();
    match tx.try_send(job) {
        Ok(()) => DecodeJobSendOutcome::Queued,
        Err(TrySendError::Full(job)) => {
            if !should_block_when_full {
                return DecodeJobSendOutcome::Dropped;
            }
            if tx.send(job).is_ok() {
                DecodeJobSendOutcome::Queued
            } else {
                DecodeJobSendOutcome::Disconnected
            }
        }
        Err(TrySendError::Disconnected(_)) => DecodeJobSendOutcome::Disconnected,
    }
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
    let extra_needed = input.len().saturating_sub(output.capacity());
    if extra_needed > 0 {
        output.reserve(extra_needed);
    }

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

fn yuv420_to_rgb_scaled(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    src_width: usize,
    src_height: usize,
    stride_y: usize,
    stride_u: usize,
    stride_v: usize,
    target_width: usize,
    target_height: usize,
    plan_cache: &mut Option<Yuv420ScalePlan>,
    out: &mut Vec<u8>,
) -> (usize, usize) {
    let Some(plan) = ensure_yuv420_scale_plan(
        plan_cache,
        src_width,
        src_height,
        target_width,
        target_height,
    ) else {
        out.clear();
        return (0, 0);
    };

    let needed = plan
        .output_width
        .saturating_mul(plan.output_height)
        .saturating_mul(3);
    out.resize(needed, 0);

    let mut dst = 0_usize;
    for y in 0..plan.output_height {
        let y_src0 = plan.y_y.src0[y];
        let y_src1 = plan.y_y.src1[y];
        let y_w1 = plan.y_y.w1[y];
        let y_row0 = y_src0.saturating_mul(stride_y);
        let y_row1 = y_src1.saturating_mul(stride_y);

        let uv_y = y / 2;
        let uv_src0 = plan.uv_y.src0[uv_y];
        let uv_src1 = plan.uv_y.src1[uv_y];
        let uv_w1 = plan.uv_y.w1[uv_y];
        let u_row0 = uv_src0.saturating_mul(stride_u);
        let u_row1 = uv_src1.saturating_mul(stride_u);
        let v_row0 = uv_src0.saturating_mul(stride_v);
        let v_row1 = uv_src1.saturating_mul(stride_v);

        for x in 0..plan.output_width {
            let x_src0 = plan.y_x.src0[x];
            let x_src1 = plan.y_x.src1[x];
            let x_w1 = plan.y_x.w1[x];

            let y00 = y_plane[y_row0 + x_src0];
            let y10 = y_plane[y_row0 + x_src1];
            let y01 = y_plane[y_row1 + x_src0];
            let y11 = y_plane[y_row1 + x_src1];
            let luma = bilinear_sample_u8(y00, y10, y01, y11, x_w1, y_w1);

            let uv_x = x / 2;
            let uv_src_x0 = plan.uv_x.src0[uv_x];
            let uv_src_x1 = plan.uv_x.src1[uv_x];
            let uv_x_w1 = plan.uv_x.w1[uv_x];

            let u00 = u_plane[u_row0 + uv_src_x0];
            let u10 = u_plane[u_row0 + uv_src_x1];
            let u01 = u_plane[u_row1 + uv_src_x0];
            let u11 = u_plane[u_row1 + uv_src_x1];
            let v00 = v_plane[v_row0 + uv_src_x0];
            let v10 = v_plane[v_row0 + uv_src_x1];
            let v01 = v_plane[v_row1 + uv_src_x0];
            let v11 = v_plane[v_row1 + uv_src_x1];

            let cb = bilinear_sample_u8(u00, u10, u01, u11, uv_x_w1, uv_w1) - 128;
            let cr = bilinear_sample_u8(v00, v10, v01, v11, uv_x_w1, uv_w1) - 128;
            let c = (luma - 16).max(0);

            let r = ((298 * c + 409 * cr + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 100 * cb - 208 * cr + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 516 * cb + 128) >> 8).clamp(0, 255) as u8;
            out[dst] = r;
            out[dst + 1] = g;
            out[dst + 2] = b;
            dst = dst.saturating_add(3);
        }
    }

    (plan.output_width, plan.output_height)
}

fn resolved_output_dims(
    src_width: usize,
    src_height: usize,
    target_width: usize,
    target_height: usize,
) -> Option<(usize, usize)> {
    if src_width == 0 || src_height == 0 || target_width == 0 || target_height == 0 {
        return None;
    }

    let output_width = target_width.min(src_width).max(2) & !1;
    let output_height = target_height.min(src_height).max(2) & !1;
    Some((output_width, output_height))
}

fn should_use_rgb_simd_resize(
    src_width: usize,
    src_height: usize,
    dst_width: usize,
    dst_height: usize,
) -> bool {
    let src_area = src_width.saturating_mul(src_height).max(1);
    let dst_area = dst_width.saturating_mul(dst_height);
    dst_area.saturating_mul(100) >= src_area.saturating_mul(RGB_SIMD_RESIZE_MIN_AREA_PERCENT)
}

fn resize_rgb_simd(
    state: &mut RgbResizeState,
    src_width: usize,
    src_height: usize,
    dst_width: usize,
    dst_height: usize,
    out: &mut Vec<u8>,
) -> Result<()> {
    let dst_needed = dst_width.saturating_mul(dst_height).saturating_mul(3);
    out.resize(dst_needed, 0);

    let src_image = fir::images::Image::from_slice_u8(
        src_width as u32,
        src_height as u32,
        state.src_rgb.as_mut_slice(),
        fir::PixelType::U8x3,
    )
    .context("failed creating SIMD resize source image")?;
    let mut dst_image = fir::images::Image::from_slice_u8(
        dst_width as u32,
        dst_height as u32,
        out.as_mut_slice(),
        fir::PixelType::U8x3,
    )
    .context("failed creating SIMD resize destination image")?;

    state
        .resizer
        .resize(&src_image, &mut dst_image, Some(&state.options))
        .context("SIMD resize failed")
}

fn ensure_yuv420_scale_plan<'a>(
    cache: &'a mut Option<Yuv420ScalePlan>,
    src_width: usize,
    src_height: usize,
    target_width: usize,
    target_height: usize,
) -> Option<&'a Yuv420ScalePlan> {
    let (output_width, output_height) =
        resolved_output_dims(src_width, src_height, target_width, target_height)?;

    let needs_rebuild = cache.as_ref().is_none_or(|plan| {
        plan.src_width != src_width
            || plan.src_height != src_height
            || plan.output_width != output_width
            || plan.output_height != output_height
    });
    if needs_rebuild {
        let src_uv_width = src_width.div_ceil(2);
        let src_uv_height = src_height.div_ceil(2);
        let dst_uv_width = output_width.div_ceil(2);
        let dst_uv_height = output_height.div_ceil(2);
        *cache = Some(Yuv420ScalePlan {
            src_width,
            src_height,
            output_width,
            output_height,
            y_x: build_scale_axis_lut(src_width, output_width),
            y_y: build_scale_axis_lut(src_height, output_height),
            uv_x: build_scale_axis_lut(src_uv_width, dst_uv_width),
            uv_y: build_scale_axis_lut(src_uv_height, dst_uv_height),
        });
    }

    cache.as_ref()
}

fn build_scale_axis_lut(src_len: usize, dst_len: usize) -> ScaleAxisLut {
    let mut src0 = Vec::with_capacity(dst_len);
    let mut src1 = Vec::with_capacity(dst_len);
    let mut w1 = Vec::with_capacity(dst_len);
    if src_len == 0 || dst_len == 0 {
        return ScaleAxisLut { src0, src1, w1 };
    }

    let src_max = src_len.saturating_sub(1) as f32;
    let src_len_f = src_len as f32;
    let dst_len_f = dst_len as f32;
    for i in 0..dst_len {
        let mapped = (((i as f32 + 0.5) * src_len_f) / dst_len_f - 0.5).clamp(0.0, src_max);
        let base = mapped.floor() as usize;
        let next = (base + 1).min(src_len.saturating_sub(1));
        let frac = (mapped - base as f32).clamp(0.0, 1.0);
        let weight_1 = (frac * 256.0).round().clamp(0.0, 256.0) as u16;

        src0.push(base);
        src1.push(next);
        w1.push(weight_1);
    }

    ScaleAxisLut { src0, src1, w1 }
}

fn bilinear_sample_u8(
    top_left: u8,
    top_right: u8,
    bottom_left: u8,
    bottom_right: u8,
    x_w1: u16,
    y_w1: u16,
) -> i32 {
    let x1 = i32::from(x_w1.min(256));
    let y1 = i32::from(y_w1.min(256));
    let x0 = 256 - x1;
    let y0 = 256 - y1;

    let top = i32::from(top_left) * x0 + i32::from(top_right) * x1;
    let bottom = i32::from(bottom_left) * x0 + i32::from(bottom_right) * x1;
    (top * y0 + bottom * y1 + 32_768) >> 16
}

fn is_probably_high_motion(previous: &[u8], current: &[u8], width: usize, height: usize) -> bool {
    if previous.len() != current.len()
        || current.len() != width.saturating_mul(height).saturating_mul(3)
        || width == 0
        || height == 0
    {
        return true;
    }

    let step_x = (width / DELTA_MOTION_SAMPLE_COLS.max(1)).max(1);
    let step_y = (height / DELTA_MOTION_SAMPLE_ROWS.max(1)).max(1);
    let mut changed = 0_usize;
    let mut sampled = 0_usize;

    for y in (0..height).step_by(step_y) {
        let row_start = y.saturating_mul(width).saturating_mul(3);
        for x in (0..width).step_by(step_x) {
            let idx = row_start + x.saturating_mul(3);
            let dr = previous[idx].abs_diff(current[idx]);
            let dg = previous[idx + 1].abs_diff(current[idx + 1]);
            let db = previous[idx + 2].abs_diff(current[idx + 2]);
            let delta_sum = u16::from(dr) + u16::from(dg) + u16::from(db);
            if delta_sum >= DELTA_MOTION_PIXEL_DIFF_THRESHOLD {
                changed = changed.saturating_add(1);
            }
            sampled = sampled.saturating_add(1);
        }
    }

    sampled > 0 && changed.saturating_mul(100) >= sampled.saturating_mul(DELTA_MOTION_SKIP_PERCENT)
}

#[inline]
#[allow(unsafe_code)]
fn rgb_simd_chunk_equal(previous: &[u8], current: &[u8]) -> bool {
    debug_assert!(previous.len() >= RGB_SIMD_BYTES);
    debug_assert!(current.len() >= RGB_SIMD_BYTES);

    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: caller guarantees both slices are at least RGB_SIMD_BYTES bytes long.
        unsafe { return rgb_simd_chunk_equal_neon(previous.as_ptr(), current.as_ptr()) };
    }

    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: caller guarantees both slices are at least RGB_SIMD_BYTES bytes long.
        unsafe { return rgb_simd_chunk_equal_sse2(previous.as_ptr(), current.as_ptr()) };
    }

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        previous[..RGB_SIMD_BYTES] == current[..RGB_SIMD_BYTES]
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(unsafe_code)]
unsafe fn rgb_simd_chunk_equal_sse2(previous: *const u8, current: *const u8) -> bool {
    let prev0 = unsafe { _mm_loadu_si128(previous as *const _) };
    let curr0 = unsafe { _mm_loadu_si128(current as *const _) };
    let prev1 = unsafe { _mm_loadu_si128(previous.add(16) as *const _) };
    let curr1 = unsafe { _mm_loadu_si128(current.add(16) as *const _) };
    let prev2 = unsafe { _mm_loadu_si128(previous.add(32) as *const _) };
    let curr2 = unsafe { _mm_loadu_si128(current.add(32) as *const _) };

    let eq0 = _mm_movemask_epi8(_mm_cmpeq_epi8(prev0, curr0)) == 0xffff;
    let eq1 = _mm_movemask_epi8(_mm_cmpeq_epi8(prev1, curr1)) == 0xffff;
    let eq2 = _mm_movemask_epi8(_mm_cmpeq_epi8(prev2, curr2)) == 0xffff;

    eq0 && eq1 && eq2
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[allow(unsafe_code)]
unsafe fn rgb_simd_chunk_equal_neon(previous: *const u8, current: *const u8) -> bool {
    let prev0 = unsafe { vld1q_u8(previous) };
    let curr0 = unsafe { vld1q_u8(current) };
    let prev1 = unsafe { vld1q_u8(previous.add(16)) };
    let curr1 = unsafe { vld1q_u8(current.add(16)) };
    let prev2 = unsafe { vld1q_u8(previous.add(32)) };
    let curr2 = unsafe { vld1q_u8(current.add(32)) };

    let diff0 = veorq_u8(prev0, curr0);
    let diff1 = veorq_u8(prev1, curr1);
    let diff2 = veorq_u8(prev2, curr2);
    let diff_any = vorrq_u8(vorrq_u8(diff0, diff1), diff2);
    let paired = vorr_u8(vget_low_u8(diff_any), vget_high_u8(diff_any));
    vmaxv_u8(paired) == 0
}

fn compute_changed_rect(
    previous: &[u8],
    current: &[u8],
    width: usize,
    height: usize,
) -> Option<PixelRect> {
    if previous.len() != current.len()
        || current.len() != width.saturating_mul(height).saturating_mul(3)
    {
        return None;
    }

    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0_usize;
    let mut max_y = 0_usize;
    let mut changed = false;
    let mut changed_pixels = 0_usize;
    let full_area = width.saturating_mul(height).max(1);

    for y in 0..height {
        let row_start = y.saturating_mul(width).saturating_mul(3);
        let row_end = row_start + width.saturating_mul(3);
        let row_previous = &previous[row_start..row_end];
        let row_current = &current[row_start..row_end];

        let mut x = 0_usize;
        while x + RGB_SIMD_PIXELS <= width {
            let byte_offset = x.saturating_mul(3);
            if !rgb_simd_chunk_equal(&row_previous[byte_offset..], &row_current[byte_offset..]) {
                let chunk_end = x + RGB_SIMD_PIXELS;
                for chunk_x in x..chunk_end {
                    let idx = row_start + chunk_x.saturating_mul(3);
                    if previous[idx] != current[idx]
                        || previous[idx + 1] != current[idx + 1]
                        || previous[idx + 2] != current[idx + 2]
                    {
                        changed = true;
                        changed_pixels = changed_pixels.saturating_add(1);
                        min_x = min_x.min(chunk_x);
                        min_y = min_y.min(y);
                        max_x = max_x.max(chunk_x);
                        max_y = max_y.max(y);

                        if changed_pixels.saturating_mul(100)
                            >= full_area.saturating_mul(DELTA_MAX_AREA_PERCENT)
                        {
                            return Some(PixelRect {
                                x: 0,
                                y: 0,
                                width,
                                height,
                            });
                        }
                    }
                }
            }
            x += RGB_SIMD_PIXELS;
        }

        while x < width {
            let idx = row_start + x.saturating_mul(3);
            if previous[idx] != current[idx]
                || previous[idx + 1] != current[idx + 1]
                || previous[idx + 2] != current[idx + 2]
            {
                changed = true;
                changed_pixels = changed_pixels.saturating_add(1);
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);

                if changed_pixels.saturating_mul(100)
                    >= full_area.saturating_mul(DELTA_MAX_AREA_PERCENT)
                {
                    return Some(PixelRect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    });
                }
            }
            x = x.saturating_add(1);
        }
    }

    if changed {
        Some(PixelRect {
            x: min_x,
            y: min_y,
            width: max_x.saturating_sub(min_x).saturating_add(1),
            height: max_y.saturating_sub(min_y).saturating_add(1),
        })
    } else {
        None
    }
}

fn extract_rgb_rect(source: &[u8], source_width: usize, rect: PixelRect, out: &mut Vec<u8>) {
    let row_bytes = rect.width.saturating_mul(3);
    let needed = rect.height.saturating_mul(row_bytes);
    out.resize(needed, 0);

    for row in 0..rect.height {
        let src_start = ((rect.y + row).saturating_mul(source_width) + rect.x).saturating_mul(3);
        let src_end = src_start.saturating_add(row_bytes);
        let dst_start = row.saturating_mul(row_bytes);
        let dst_end = dst_start.saturating_add(row_bytes);
        out[dst_start..dst_end].copy_from_slice(&source[src_start..src_end]);
    }
}

pub(crate) fn compute_render_geometry(tile: Rect, stream_count: usize) -> RenderGeometry {
    let tile_cols = usize::from(tile.width.max(2));
    let tile_rows = usize::from(tile.height.max(2));
    let (cell_px_w, cell_px_h) = terminal_cell_pixel_size().unwrap_or((8, 16));

    let base_width = tile_cols.saturating_mul(cell_px_w);
    let base_height = tile_rows.saturating_mul(cell_px_h);
    let quality = quality_scale_for_grid(stream_count, tile_cols, tile_rows)
        * oversample_for_stream_count(stream_count)
        * render_scale_override();
    let (max_width, max_height) = render_max_dims_for_stream_count(stream_count);

    let scaled_width = (base_width as f32 * quality).round() as usize;
    let scaled_height = (base_height as f32 * quality).round() as usize;

    RenderGeometry {
        width: even_clamp(scaled_width, MIN_RENDER_WIDTH, max_width),
        height: even_clamp(scaled_height, MIN_RENDER_HEIGHT, max_height),
    }
}

pub(crate) fn adapt_render_geometry_for_transfer_mode(
    geometry: RenderGeometry,
    transfer_mode: KittyTransferMode,
    stream_count: usize,
) -> RenderGeometry {
    if transfer_mode != KittyTransferMode::Stream {
        return geometry;
    }

    let (max_width, max_height) = render_max_dims_for_stream_count(stream_count);
    let stream_mode_scale = *STREAM_MODE_SCALE_OVERRIDE.get_or_init(|| {
        std::env::var("RTSP_TUI_STREAM_MODE_SCALE")
            .ok()
            .and_then(|raw| raw.parse::<f32>().ok())
            .map(|scale| scale.clamp(0.5, 1.0))
            .unwrap_or(STREAM_MODE_GEOMETRY_SCALE)
    });
    let scaled_width = (geometry.width as f32 * stream_mode_scale).round() as usize;
    let scaled_height = (geometry.height as f32 * stream_mode_scale).round() as usize;

    RenderGeometry {
        width: even_clamp(scaled_width, MIN_RENDER_WIDTH, max_width),
        height: even_clamp(scaled_height, MIN_RENDER_HEIGHT, max_height),
    }
}

fn render_scale_override() -> f32 {
    *RENDER_SCALE_OVERRIDE.get_or_init(|| {
        std::env::var("RTSP_TUI_RENDER_SCALE")
            .ok()
            .and_then(|raw| raw.parse::<f32>().ok())
            .map(|scale| scale.clamp(0.5, 1.0))
            .unwrap_or(1.0)
    })
}

fn terminal_cell_pixel_size() -> Option<(usize, usize)> {
    let window = crossterm::terminal::window_size().ok()?;
    if window.columns == 0 || window.rows == 0 {
        return None;
    }

    let cell_px_w = usize::from((window.width / window.columns).max(1));
    let cell_px_h = usize::from((window.height / window.rows).max(1));
    Some((cell_px_w, cell_px_h))
}

fn quality_scale_for_grid(stream_count: usize, tile_cols: usize, tile_rows: usize) -> f32 {
    let tile_cells = tile_cols.saturating_mul(tile_rows) as f32;
    let density = (tile_cells / 300.0).clamp(0.5, 1.0);
    let stream_pressure = (5.0 / stream_count.max(1) as f32).sqrt().clamp(0.6, 1.0);
    (density * stream_pressure).clamp(0.45, 1.0)
}

fn oversample_for_stream_count(stream_count: usize) -> f32 {
    match stream_count {
        0 | 1 => 1.45,
        2 => 1.35,
        3..=4 => 1.2,
        5..=8 => 1.0,
        _ => 0.9,
    }
}

fn render_max_dims_for_stream_count(stream_count: usize) -> (usize, usize) {
    match stream_count {
        0..=2 => (MAX_RENDER_WIDTH_FEW, MAX_RENDER_HEIGHT_FEW),
        3..=4 => (MAX_RENDER_WIDTH_MEDIUM, MAX_RENDER_HEIGHT_MEDIUM),
        _ => (MAX_RENDER_WIDTH_MANY, MAX_RENDER_HEIGHT_MANY),
    }
}

fn even_clamp(value: usize, min: usize, max: usize) -> usize {
    let clamped = value.clamp(min, max);
    let even = clamped & !1;
    even.max(2)
}

pub(crate) fn compute_grid_dimensions(
    count: usize,
    area: Rect,
    aspect_w: u32,
    aspect_h: u32,
) -> (usize, usize) {
    let count = count.max(1);
    let mut best = (1_usize, count);
    let mut best_score = 0_u64;
    let mut best_empty = usize::MAX;

    for rows in 1..=count {
        let cols = count.div_ceil(rows);
        let cell_w = usize::from(area.width).div_ceil(cols);
        let cell_h = usize::from(area.height).div_ceil(rows);
        if cell_w == 0 || cell_h == 0 {
            continue;
        }
        let (fit_w, fit_h) = fit_dims_to_aspect(
            cell_w,
            cell_h,
            usize::try_from(aspect_w).unwrap_or(16),
            usize::try_from(aspect_h).unwrap_or(9),
        );
        let per_tile = u64::try_from(fit_w.saturating_mul(fit_h)).unwrap_or(u64::MAX);
        let visible_tiles = rows.saturating_mul(cols).min(count);
        let score = per_tile.saturating_mul(u64::try_from(visible_tiles).unwrap_or(u64::MAX));
        let empty = rows.saturating_mul(cols).saturating_sub(count);

        if score > best_score || (score == best_score && empty < best_empty) {
            best = (rows, cols);
            best_score = score;
            best_empty = empty;
        }
    }

    best
}

pub(crate) fn target_video_aspect_in_cells() -> (u32, u32) {
    // Default to roughly 16:9 in terminals where character cells are ~2x taller than wide.
    let default = (32_u32, 9_u32);
    let Ok(window) = crossterm::terminal::window_size() else {
        return default;
    };

    if window.columns == 0 || window.rows == 0 || window.width == 0 || window.height == 0 {
        return default;
    }

    // PixelAspect = (cols/rows) * (cell_width/cell_height), so:
    // cols/rows target = (video_w/video_h) * (cell_height/cell_width)
    // Use integer math: w:h = 16*height*columns : 9*rows*width
    let numer = 16_u64
        .saturating_mul(u64::from(window.height))
        .saturating_mul(u64::from(window.columns));
    let denom = 9_u64
        .saturating_mul(u64::from(window.rows))
        .saturating_mul(u64::from(window.width));
    if numer == 0 || denom == 0 {
        return default;
    }

    let gcd = gcd_u64(numer, denom).max(1);
    let reduced_w = (numer / gcd).min(u64::from(u32::MAX));
    let reduced_h = (denom / gcd).min(u64::from(u32::MAX));
    (
        u32::try_from(reduced_w).unwrap_or(default.0).max(1),
        u32::try_from(reduced_h).unwrap_or(default.1).max(1),
    )
}

fn gcd_u64(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let rem = left % right;
        left = right;
        right = rem;
    }
    left
}

fn fit_dims_to_aspect(
    width: usize,
    height: usize,
    aspect_w: usize,
    aspect_h: usize,
) -> (usize, usize) {
    if width == 0 || height == 0 || aspect_w == 0 || aspect_h == 0 {
        return (width, height);
    }

    if width.saturating_mul(aspect_h) > height.saturating_mul(aspect_w) {
        let h = height;
        let w = h.saturating_mul(aspect_w).div_ceil(aspect_h).max(1);
        (w.min(width), h)
    } else {
        let w = width;
        let h = w.saturating_mul(aspect_h).div_ceil(aspect_w).max(1);
        (w, h.min(height))
    }
}

pub(crate) fn build_grid_rects_for_aspect(
    area: Rect,
    rows: usize,
    cols: usize,
    aspect_w: u32,
    aspect_h: u32,
) -> Vec<Rect> {
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
        for col_area in col_chunks.iter().copied() {
            rects.push(fit_rect_to_aspect(col_area, aspect_w, aspect_h));
        }
    }

    rects
}

fn fit_rect_to_aspect(area: Rect, aspect_w: u32, aspect_h: u32) -> Rect {
    if area.width == 0 || area.height == 0 || aspect_w == 0 || aspect_h == 0 {
        return area;
    }

    let area_w = u32::from(area.width);
    let area_h = u32::from(area.height);

    let (target_w, target_h) = if area_w.saturating_mul(aspect_h) > area_h.saturating_mul(aspect_w)
    {
        let h = area_h;
        let w = (h.saturating_mul(aspect_w) / aspect_h).max(1);
        (w, h)
    } else {
        let w = area_w;
        let h = (w.saturating_mul(aspect_h) / aspect_w).max(1);
        (w, h)
    };

    let target_w = u16::try_from(target_w.min(u32::from(area.width))).unwrap_or(area.width);
    let target_h = u16::try_from(target_h.min(u32::from(area.height))).unwrap_or(area.height);
    let offset_x = (area.width.saturating_sub(target_w)) / 2;
    let offset_y = (area.height.saturating_sub(target_h)) / 2;

    Rect {
        x: area.x.saturating_add(offset_x),
        y: area.y.saturating_add(offset_y),
        width: target_w,
        height: target_h,
    }
}

pub(crate) fn inner_cell(cell: Rect) -> Rect {
    Rect {
        x: cell.x.saturating_add(1),
        y: cell.y.saturating_add(1),
        width: cell.width.saturating_sub(2),
        height: cell.height.saturating_sub(2),
    }
}

pub(crate) fn push_kitty_chunked_bytes(
    out: &mut Vec<u8>,
    control: &str,
    payload: &str,
    continuation_control: Option<&str>,
) {
    const CHUNK: usize = 4_096;

    let mut offset = 0_usize;
    let payload_len = payload.len();

    while offset < payload_len {
        let next = (offset + CHUNK).min(payload_len);
        let chunk = &payload[offset..next];
        let more = if next < payload_len { b'1' } else { b'0' };

        out.extend_from_slice(b"\x1b_G");
        if offset == 0 {
            out.extend_from_slice(control.as_bytes());
            out.extend_from_slice(b",m=");
        } else {
            if let Some(continuation_control) = continuation_control {
                out.extend_from_slice(continuation_control.as_bytes());
                out.extend_from_slice(b",");
            }
            out.extend_from_slice(b"m=");
        }
        out.push(more);
        out.extend_from_slice(b";");
        out.extend_from_slice(chunk.as_bytes());
        out.extend_from_slice(b"\x1b\\");
        offset = next;
    }

    if payload_len == 0 {
        out.extend_from_slice(b"\x1b_G");
        out.extend_from_slice(control.as_bytes());
        out.extend_from_slice(b",m=0;\x1b\\");
    }
}

pub(crate) fn detect_kitty_graphics_support() -> bool {
    if let Some(explicit) = parse_bool_env("RTSP_CLI_KITTY_GRAPHICS") {
        return explicit;
    }

    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_lowercase();

    // TERM and TERM_PROGRAM cover known values used by terminals that implement Kitty graphics.
    if term_or_program_indicates_kitty_graphics(&term, &term_program) {
        return true;
    }

    // Additional environment markers for terminals that often keep TERM as xterm-256color.
    [
        "KITTY_WINDOW_ID",
        "KITTY_PID",
        "WEZTERM_EXECUTABLE",
        "WEZTERM_PANE",
        "GHOSTTY_RESOURCES_DIR",
        "KONSOLE_VERSION",
        "KONSOLE_PROFILE_NAME",
    ]
    .into_iter()
    .any(|name| std::env::var_os(name).is_some())
}

pub(crate) fn detect_kitty_transfer_mode() -> KittyTransferMode {
    if let Ok(explicit) = std::env::var("RTSP_CLI_KITTY_TRANSFER_MODE") {
        match explicit.trim().to_ascii_lowercase().as_str() {
            "stream" => return KittyTransferMode::Stream,
            "file" | "shared" | "shm" => return KittyTransferMode::File,
            _ => {}
        }
    }

    let is_ssh =
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    let has_namespace_isolation = detect_namespace_isolation();
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_lowercase();
    let has_native_kitty_session =
        std::env::var_os("KITTY_WINDOW_ID").is_some() || std::env::var_os("KITTY_PID").is_some();
    let local_graphics_hint =
        has_native_kitty_session || term_or_program_indicates_kitty_graphics(&term, &term_program);
    choose_kitty_transfer_mode(is_ssh, has_namespace_isolation, local_graphics_hint)
}

fn choose_kitty_transfer_mode(
    is_ssh: bool,
    has_namespace_isolation: bool,
    local_graphics_hint: bool,
) -> KittyTransferMode {
    // Remote and containerized sessions commonly do not share a filesystem namespace with
    // the terminal GUI process, so kitty file transfer can render as black frames.
    if is_ssh || has_namespace_isolation {
        return KittyTransferMode::Stream;
    }

    // Prefer local file transport whenever we're in a known local graphics-capable terminal.
    if local_graphics_hint {
        KittyTransferMode::File
    } else {
        KittyTransferMode::Stream
    }
}

fn detect_namespace_isolation() -> bool {
    let env_markers = [
        "CONTAINER",
        "FLATPAK_ID",
        "FLATPAK_SANDBOX_DIR",
        "TOOLBOX_PATH",
        "DISTROBOX_ENTER_PATH",
    ];
    if env_markers
        .into_iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return true;
    }

    let file_markers = ["/run/.containerenv", "/.dockerenv"];
    file_markers
        .into_iter()
        .any(|path| std::path::Path::new(path).exists())
}

fn term_or_program_indicates_kitty_graphics(term: &str, term_program: &str) -> bool {
    let term = term.to_ascii_lowercase();
    let term_program = term_program.to_ascii_lowercase();

    [
        "kitty",
        "ghostty",
        "wezterm",
        "foot",
        "foot-extra",
        "konsole",
    ]
    .into_iter()
    .any(|hint| term.contains(hint))
        || ["ghostty", "wezterm", "konsole"]
            .into_iter()
            .any(|hint| term_program.contains(hint))
}

fn parse_bool_env(name: &str) -> Option<bool> {
    let value = std::env::var(name).ok()?;
    parse_bool_value(&value)
}

fn parse_bool_value(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enable" | "enabled" => Some(true),
        "0" | "false" | "no" | "off" | "disable" | "disabled" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        KittyTransferMode, PixelRect, choose_kitty_transfer_mode, compute_changed_rect,
        parse_bool_value, term_or_program_indicates_kitty_graphics,
    };

    #[test]
    fn kitty_term_hints_are_detected() {
        assert!(term_or_program_indicates_kitty_graphics(
            "xterm-kitty",
            "unknown"
        ));
        assert!(term_or_program_indicates_kitty_graphics(
            "wezterm", "unknown"
        ));
        assert!(term_or_program_indicates_kitty_graphics(
            "xterm-ghostty",
            "unknown"
        ));
        assert!(term_or_program_indicates_kitty_graphics("foot", "unknown"));
        assert!(term_or_program_indicates_kitty_graphics(
            "foot-extra",
            "unknown"
        ));
        assert!(term_or_program_indicates_kitty_graphics(
            "konsole", "unknown"
        ));
    }

    #[test]
    fn kitty_term_program_hints_are_detected() {
        assert!(term_or_program_indicates_kitty_graphics(
            "xterm-256color",
            "WezTerm"
        ));
        assert!(term_or_program_indicates_kitty_graphics(
            "xterm-256color",
            "Ghostty"
        ));
        assert!(term_or_program_indicates_kitty_graphics(
            "xterm-256color",
            "Konsole"
        ));
        assert!(!term_or_program_indicates_kitty_graphics(
            "xterm-256color",
            "Apple_Terminal"
        ));
    }

    #[test]
    fn bool_values_parse_as_expected() {
        assert_eq!(parse_bool_value("1"), Some(true));
        assert_eq!(parse_bool_value(" enabled "), Some(true));
        assert_eq!(parse_bool_value("0"), Some(false));
        assert_eq!(parse_bool_value("disabled"), Some(false));
        assert_eq!(parse_bool_value("maybe"), None);
    }

    #[test]
    fn kitty_transfer_prefers_stream_for_unknown_local_session() {
        assert_eq!(
            choose_kitty_transfer_mode(false, false, false),
            KittyTransferMode::Stream
        );
    }

    #[test]
    fn kitty_transfer_prefers_file_for_native_kitty_session() {
        assert_eq!(
            choose_kitty_transfer_mode(false, false, true),
            KittyTransferMode::File
        );
    }

    #[test]
    fn kitty_transfer_uses_stream_for_ssh_or_container() {
        assert_eq!(
            choose_kitty_transfer_mode(true, false, true),
            KittyTransferMode::Stream
        );
        assert_eq!(
            choose_kitty_transfer_mode(false, true, true),
            KittyTransferMode::Stream
        );
    }

    #[test]
    fn compute_changed_rect_detects_single_pixel_change() {
        let width = 32;
        let height = 3;
        let mut previous = vec![0_u8; width * height * 3];
        let mut current = previous.clone();

        let x = 17;
        let y = 1;
        let idx = (y * width + x) * 3;
        current[idx + 2] = 200;

        let rect = compute_changed_rect(&previous, &current, width, height);
        assert_eq!(
            rect,
            Some(PixelRect {
                x,
                y,
                width: 1,
                height: 1
            })
        );

        previous[idx + 2] = 200;
        assert_eq!(
            compute_changed_rect(&previous, &current, width, height),
            None
        );
    }

    #[test]
    fn compute_changed_rect_escalates_to_full_rect_for_heavy_change() {
        let width = 20;
        let height = 6;
        let previous = vec![0_u8; width * height * 3];
        let current = vec![255_u8; width * height * 3];

        let rect = compute_changed_rect(&previous, &current, width, height);
        assert_eq!(
            rect,
            Some(PixelRect {
                x: 0,
                y: 0,
                width,
                height
            })
        );
    }
}
