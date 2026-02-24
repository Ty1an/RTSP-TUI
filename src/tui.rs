#![allow(
    clippy::assigning_clones,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unnecessary_wraps
)]

use crate::cache::{self, DiscoveredStream, DiscoveryCache, SelectedStream, SelectionCache};
use crate::cli::{DiscoverArgs, TransportMode, default_paths, default_ports};
use crate::discovery;
use crate::theme::{self, ThemePalette};
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_ENGINE;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use retina::client::{
    Credentials, PlayOptions, Session, SessionOptions, SetupOptions, TcpTransportOptions,
    Transport, UdpTransportOptions,
};
use retina::codec::{CodecItem, ParametersRef};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc as tokio_mpsc, watch};
use tokio::task::JoinHandle;
use url::Url;

const LIVE_TARGET_FPS: u16 = 20;
const DISCOVERY_RESCAN_INTERVAL: Duration = Duration::from_secs(30);
const DISCOVERY_RETRY_INTERVAL: Duration = Duration::from_secs(6);
const DELTA_MAX_AREA_PERCENT: usize = 60;
const MIN_RENDER_WIDTH: usize = 96;
const MIN_RENDER_HEIGHT: usize = 54;
const MAX_RENDER_WIDTH_MANY: usize = 640;
const MAX_RENDER_HEIGHT_MANY: usize = 360;
const MAX_RENDER_WIDTH_MEDIUM: usize = 960;
const MAX_RENDER_HEIGHT_MEDIUM: usize = 540;
const MAX_RENDER_WIDTH_FEW: usize = 1280;
const MAX_RENDER_HEIGHT_FEW: usize = 720;

const GLYPH_ACTIVE: &str = "▸";
const GLYPH_CHECKED: &str = "◉";
const GLYPH_UNCHECKED: &str = "○";
const GLYPH_BULLET: &str = "•";

static THEME: OnceLock<ThemePalette> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RenderGeometry {
    width: usize,
    height: usize,
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
enum KittyTransferMode {
    Stream,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelRect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
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

#[derive(Debug)]
struct KittyFileTransfer {
    path: PathBuf,
    encoded_path: String,
    file: File,
    size: usize,
}

impl Drop for KittyFileTransfer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
struct KittyUploadState {
    row: u32,
    col: u32,
    cell_cols: u16,
    cell_rows: u16,
    frame_width: usize,
    frame_height: usize,
    encoded_seq: u64,
    encoded_payload: String,
    delta_rgb: Vec<u8>,
    previous_rgb: Vec<u8>,
    previous_width: usize,
    previous_height: usize,
    next_upload_at: Option<Instant>,
    file_transfer: Option<KittyFileTransfer>,
}

impl Default for KittyUploadState {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            cell_cols: 0,
            cell_rows: 0,
            frame_width: 0,
            frame_height: 0,
            encoded_seq: u64::MAX,
            encoded_payload: String::new(),
            delta_rgb: Vec::new(),
            previous_rgb: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            next_upload_at: None,
            file_transfer: None,
        }
    }
}

impl KittyUploadState {
    fn ensure_file_transfer(&mut self, tile_idx: usize) -> Result<()> {
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

enum DecodeJob {
    Reset,
    ExtraConfig(Vec<u8>),
    FrameAvcc {
        avcc: Vec<u8>,
        geometry: RenderGeometry,
    },
}

pub async fn run_tui() -> Result<()> {
    let loaded_theme = match theme::load_or_create_theme() {
        Ok(palette) => palette,
        Err(err) => {
            eprintln!("Warning: failed to load theme config ({err:#}). Using defaults.");
            ThemePalette::default()
        }
    };
    let _ = THEME.set(loaded_theme);

    let mut terminal = init_terminal()?;
    let mut app = App::load();

    let run_result = run_loop(&mut terminal, &mut app).await;
    let restore_result = restore_terminal(&mut terminal);

    run_result?;
    restore_result?;
    Ok(())
}

async fn run_loop(terminal: &mut AppTerminal, app: &mut App) -> Result<()> {
    let mut running = true;
    let frame_interval = Duration::from_millis(1_000_u64 / u64::from(LIVE_TARGET_FPS.max(1)));

    while running {
        let frame_started = Instant::now();
        app.sync_discovery_lifecycle();
        app.poll_discovery_events();
        app.poll_discovery_result().await;
        app.sync_live_viewer_workers();

        terminal
            .draw(|frame| app.draw(frame))
            .context("failed drawing TUI frame")?;

        if let Err(err) = app.render_kitty_graphics(terminal) {
            app.status = format!("graphics render failed: {err:#}");
            app.kitty_graphics_enabled = false;
        }

        while event::poll(Duration::ZERO).context("failed to poll input")? {
            let Event::Key(key) = event::read().context("failed reading input")? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match app.handle_key(key)? {
                AppCommand::None => {}
                AppCommand::Quit => {
                    running = false;
                    break;
                }
            }
        }

        let elapsed = frame_started.elapsed();
        if let Some(remaining) = frame_interval.checked_sub(elapsed) {
            tokio::time::sleep(remaining).await;
        }
    }

    app.stop_live_viewer_workers();
    let _ = app.clear_kitty_images(terminal);
    Ok(())
}

type AppTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn active_theme() -> &'static ThemePalette {
    THEME.get_or_init(ThemePalette::default)
}

fn init_terminal() -> Result<AppTerminal> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed entering alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("failed creating terminal")
}

fn suspend_terminal(terminal: &mut AppTerminal) -> Result<()> {
    disable_raw_mode().context("failed disabling raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("failed leaving alternate screen")?;
    terminal.show_cursor().context("failed showing cursor")?;
    Ok(())
}

fn restore_terminal(terminal: &mut AppTerminal) -> Result<()> {
    suspend_terminal(terminal)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    ViewStreams,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsFocus {
    List,
    AuthDisplayName,
    AuthUsername,
    AuthPassword,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveStreamSpec {
    url: String,
    label: String,
}

struct App {
    screen: Screen,
    focus: SettingsFocus,
    status: String,
    auth_notice: String,
    discovery_form: DiscoveryForm,
    discovered_cache: Vec<DiscoveredStream>,
    discovered: Vec<DiscoveredItem>,
    discovered_cursor: usize,
    selected_streams: Vec<SelectedStream>,
    pending_discovery: Option<JoinHandle<Result<Vec<DiscoveredStream>>>>,
    pending_discovery_events_rx: Option<tokio_mpsc::UnboundedReceiver<discovery::DiscoveryEvent>>,
    discovery_progress: discovery::DiscoveryProgress,
    discovery_started_at: Option<Instant>,
    next_discovery_at: Option<Instant>,
    discovery_enabled: bool,
    live_stream_specs: Vec<LiveStreamSpec>,
    live_tiles: Vec<Arc<EmbeddedTileState>>,
    live_workers: Vec<JoinHandle<()>>,
    live_geometry_tx: Option<watch::Sender<RenderGeometry>>,
    kitty_graphics_enabled: bool,
    kitty_transfer_mode: KittyTransferMode,
    kitty_images_drawn: bool,
    kitty_upload_states: Vec<KittyUploadState>,
}

impl App {
    fn load() -> Self {
        let mut discovery_cache = match cache::load_cache() {
            Ok(cache) => cache,
            Err(err) => {
                eprintln!("Warning: failed to load discovery cache: {err:#}");
                DiscoveryCache::default()
            }
        };
        let original_discovery_len = discovery_cache.streams.len();
        discovery_cache.streams = dedupe_discovered_streams_by_url(discovery_cache.streams);
        if discovery_cache.streams.len() != original_discovery_len {
            if let Err(err) = cache::save_cache(&discovery_cache) {
                eprintln!("Warning: failed to rewrite deduplicated discovery cache: {err:#}");
            }
        }
        let discovered = sanitize_discovered(discovery_cache.streams.clone());

        let mut selected_streams = match cache::load_selection_cache() {
            Ok(cache) => cache.streams,
            Err(err) => {
                eprintln!("Warning: failed to load selected stream cache: {err:#}");
                Vec::new()
            }
        };
        let original_selected_len = selected_streams.len();
        selected_streams = dedupe_selected_streams_by_url(selected_streams);
        if selected_streams.len() != original_selected_len {
            if let Err(err) = cache::save_selection_cache(&SelectionCache {
                streams: selected_streams.clone(),
            }) {
                eprintln!("Warning: failed to rewrite deduplicated selected stream cache: {err:#}");
            }
        }
        let loaded_auth_count = selected_streams
            .iter()
            .filter(|stream| {
                stream
                    .username
                    .as_deref()
                    .is_some_and(|name| !name.trim().is_empty())
            })
            .count();
        let auth_notice = if loaded_auth_count > 0 {
            format!("Loaded credentials for {loaded_auth_count} camera(s).")
        } else {
            "No saved camera credentials loaded yet.".to_owned()
        };

        Self {
            screen: Screen::ViewStreams,
            focus: SettingsFocus::List,
            status: String::new(),
            auth_notice,
            discovery_form: DiscoveryForm::default(),
            discovered_cache: discovery_cache.streams.clone(),
            discovered,
            discovered_cursor: 0,
            selected_streams,
            pending_discovery: None,
            pending_discovery_events_rx: None,
            discovery_progress: discovery::DiscoveryProgress::default(),
            discovery_started_at: None,
            next_discovery_at: Some(Instant::now()),
            discovery_enabled: true,
            live_stream_specs: Vec::new(),
            live_tiles: Vec::new(),
            live_workers: Vec::new(),
            live_geometry_tx: None,
            kitty_graphics_enabled: detect_kitty_graphics_support(),
            kitty_transfer_mode: detect_kitty_transfer_mode(),
            kitty_images_drawn: false,
            kitty_upload_states: Vec::new(),
        }
    }

    fn draw(&self, frame: &mut ratatui::Frame<'_>) {
        match self.screen {
            Screen::ViewStreams => self.draw_view_streams(frame),
            Screen::Settings => self.draw_settings(frame),
        }
    }

    fn draw_view_streams(&self, frame: &mut ratatui::Frame<'_>) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(3)])
            .split(frame.area());

        let viewer_area = layout[0];
        if self.live_tiles.is_empty() {
            let mut body = vec![
                Line::from(vec![
                    Span::styled(
                        format!("{GLYPH_ACTIVE} "),
                        Style::default()
                            .fg(color_accent())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "Open Settings",
                        Style::default()
                            .fg(color_text())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " and add camera endpoints.",
                        Style::default().fg(color_text()),
                    ),
                ]),
                Line::from(Span::styled(
                    "When streams are selected, live tiles appear here automatically.",
                    Style::default().fg(color_muted()),
                )),
            ];
            if !self.status.is_empty() {
                body.push(Line::default());
                body.push(Line::from(vec![
                    Span::styled("status ", Style::default().fg(color_muted())),
                    Span::styled(
                        &self.status,
                        status_message_style(&self.status).add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            let panel = Paragraph::new(body)
                .style(Style::default().fg(color_text()))
                .block(panel_block("◉", "Live Viewer", false))
                .wrap(Wrap { trim: false });
            frame.render_widget(panel, viewer_area);
        } else {
            if !self.kitty_graphics_enabled {
                let mut body = vec![
                    Line::from(vec![
                        Span::styled(
                            format!("{GLYPH_ACTIVE} "),
                            Style::default()
                                .fg(color_warning())
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            "Graphics rendering is required for embedded video.",
                            Style::default()
                                .fg(color_text())
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(Span::styled(
                        "Kitty graphics protocol was not detected in this terminal.",
                        Style::default().fg(color_muted()),
                    )),
                ];
                if !self.status.is_empty() {
                    body.push(Line::default());
                    body.push(Line::from(vec![
                        Span::styled("status ", Style::default().fg(color_muted())),
                        Span::styled(
                            &self.status,
                            status_message_style(&self.status).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
                let panel = Paragraph::new(body)
                    .style(Style::default().fg(color_text()))
                    .block(panel_block("◉", "Live Viewer", true))
                    .wrap(Wrap { trim: false });
                frame.render_widget(panel, viewer_area);
                let footer_spans = action_hint_spans(&[("Ctrl+S", "Settings"), ("Ctrl+Q", "Quit")]);
                let footer = Paragraph::new(Line::from(footer_spans))
                    .style(Style::default().fg(color_text()))
                    .block(panel_block("⌘", "Actions", false));
                frame.render_widget(footer, layout[1]);
                return;
            }

            let (aspect_w, aspect_h) = target_video_aspect_in_cells();
            let view_count = self.live_tiles.len();
            let (rows, cols) = compute_grid_dimensions(view_count, viewer_area, aspect_w, aspect_h);
            let grid_rects =
                build_grid_rects_for_aspect(viewer_area, rows, cols, aspect_w, aspect_h);

            for idx in 0..view_count {
                if idx >= grid_rects.len() {
                    break;
                }
                let tile = &self.live_tiles[idx];

                let area = grid_rects[idx];
                let snapshot = tile.inner.read();
                let status_color = stream_state_color(&snapshot.status);
                let tile_title = Line::from(vec![
                    Span::styled(
                        format!(" {GLYPH_BULLET}{} ", idx + 1),
                        Style::default()
                            .fg(color_accent())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        &snapshot.stream_label,
                        Style::default()
                            .fg(color_text())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  net {:.1} kbps", snapshot.network_kbps),
                        Style::default().fg(color_muted()),
                    ),
                    Span::styled(
                        format!("  {}", snapshot.status),
                        Style::default().fg(status_color),
                    ),
                ]);
                frame.render_widget(
                    Paragraph::new(Line::default())
                        .style(Style::default().fg(color_text()))
                        .block(
                            Block::default()
                                .title(tile_title)
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(status_color)),
                        ),
                    area,
                );
            }
        }

        let footer_spans = action_hint_spans(&[
            ("Ctrl+L", "Toggle Discovery"),
            ("Ctrl+S", "Settings"),
            ("Ctrl+Q", "Quit"),
        ]);
        let footer = Paragraph::new(Line::from(footer_spans))
            .style(Style::default().fg(color_text()))
            .block(panel_block("⌘", "Actions", false));
        frame.render_widget(footer, layout[1]);
    }

    fn draw_settings(&self, frame: &mut ratatui::Frame<'_>) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6),
                Constraint::Min(10),
                Constraint::Length(8),
                Constraint::Length(3),
            ])
            .split(frame.area());

        let list_active = self.focus == SettingsFocus::List;
        let auth_active = matches!(
            self.focus,
            SettingsFocus::AuthDisplayName
                | SettingsFocus::AuthUsername
                | SettingsFocus::AuthPassword
        );

        let mut discovery_lines = Vec::new();
        discovery_lines.push(Line::from(Span::styled(
            "Discovery runs only while this Settings page is open.",
            Style::default().fg(color_text()),
        )));
        if !self.discovery_enabled {
            discovery_lines.push(Line::from(Span::styled(
                "Discovery is paused (Ctrl+L to resume).",
                Style::default().fg(color_warning()),
            )));
        } else if self.pending_discovery.is_some() {
            discovery_lines.push(Line::from(vec![Span::styled(
                self.discovery_loading_bar(42),
                Style::default()
                    .fg(color_accent())
                    .add_modifier(Modifier::BOLD),
            )]));
            discovery_lines.push(Line::from(vec![Span::styled(
                self.discovery_phase_text(),
                Style::default().fg(color_muted()),
            )]));
            discovery_lines.push(Line::from(vec![Span::styled(
                format!(
                    "Progress: {}/{} probes",
                    self.discovery_progress
                        .completed
                        .min(self.discovery_progress.total),
                    self.discovery_progress.total
                ),
                Style::default().fg(color_muted()),
            )]));
            if let Some(started_at) = self.discovery_started_at {
                discovery_lines.push(Line::from(vec![Span::styled(
                    format!("Elapsed: {:.1}s", started_at.elapsed().as_secs_f32()),
                    Style::default().fg(color_muted()),
                )]));
            }
        } else {
            let wait_secs = self
                .next_discovery_at
                .map(|next| next.saturating_duration_since(Instant::now()).as_secs())
                .unwrap_or(0);
            discovery_lines.push(Line::from(Span::styled(
                format!("Waiting for next scan cycle... ({wait_secs}s)"),
                Style::default().fg(color_muted()),
            )));
        }

        let discovery_panel = Paragraph::new(discovery_lines)
            .style(Style::default().fg(color_text()))
            .block(panel_block("◈", "1) Discovery", false));
        frame.render_widget(discovery_panel, layout[0]);

        let mut discovered_lines = Vec::new();
        if self.discovered.is_empty() {
            discovered_lines.push(Line::from(Span::styled(
                "No discovered cameras yet. Run auto discovery above.",
                Style::default().fg(color_muted()),
            )));
        } else {
            for (idx, camera) in self.discovered.iter().enumerate() {
                let cursor = if idx == self.discovered_cursor && self.focus == SettingsFocus::List {
                    GLYPH_ACTIVE
                } else {
                    " "
                };
                let checked = checkbox(self.is_selected(&camera.base_url));
                discovered_lines.push(Line::from(vec![
                    Span::styled(
                        format!("{cursor} "),
                        Style::default().fg(if cursor == GLYPH_ACTIVE {
                            color_accent()
                        } else {
                            color_muted()
                        }),
                    ),
                    Span::styled(
                        format!("{checked} "),
                        Style::default()
                            .fg(if checked == GLYPH_CHECKED {
                                color_success()
                            } else {
                                color_muted()
                            })
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("[{}] ", camera.status),
                        Style::default().fg(status_code_color(camera.status)),
                    ),
                    Span::styled(&camera.label, Style::default().fg(color_text())),
                ]));
            }
        }
        discovered_lines.push(Line::default());
        discovered_lines.push(Line::from(Span::styled(
            "Up/Down select stream, Enter toggle, Tab switch to auth",
            Style::default().fg(color_muted()),
        )));

        let list_panel = Paragraph::new(discovered_lines)
            .style(Style::default().fg(color_text()))
            .block(panel_block("◉", "2) Select Cameras", list_active))
            .wrap(Wrap { trim: false });
        frame.render_widget(list_panel, layout[1]);

        let mut auth_lines = Vec::new();
        if let Some((selected_label, display_name, auth_user, auth_pass)) =
            self.current_selected_auth()
        {
            let display_prefix = focus_marker(self.focus == SettingsFocus::AuthDisplayName);
            let auth_user_prefix = focus_marker(self.focus == SettingsFocus::AuthUsername);
            let auth_pass_prefix = focus_marker(self.focus == SettingsFocus::AuthPassword);
            auth_lines.push(Line::from(vec![
                Span::styled(
                    format!("{GLYPH_BULLET} "),
                    Style::default().fg(color_accent()),
                ),
                Span::styled("Selected: ", Style::default().fg(color_muted())),
                Span::styled(selected_label, Style::default().fg(color_text())),
            ]));
            auth_lines.push(Line::from(vec![
                Span::styled(
                    format!("{display_prefix} "),
                    Style::default()
                        .fg(color_accent())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Display Name: ", Style::default().fg(color_muted())),
                Span::styled(display_name, Style::default().fg(color_text())),
            ]));
            auth_lines.push(Line::from(vec![
                Span::styled(
                    format!("{auth_user_prefix} "),
                    Style::default()
                        .fg(color_accent())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Auth Username: ", Style::default().fg(color_muted())),
                Span::styled(auth_user, Style::default().fg(color_text())),
            ]));
            auth_lines.push(Line::from(vec![
                Span::styled(
                    format!("{auth_pass_prefix} "),
                    Style::default()
                        .fg(color_accent())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("Auth Password: ", Style::default().fg(color_muted())),
                Span::styled(
                    "*".repeat(auth_pass.chars().count()),
                    Style::default().fg(color_text()),
                ),
            ]));
            auth_lines.push(Line::from(Span::styled(
                "Tab switches panels. Up/Down cycles auth fields.",
                Style::default().fg(color_muted()),
            )));
        } else {
            auth_lines.push(Line::from(Span::styled(
                "Check a discovered camera first, then set auth here.",
                Style::default().fg(color_muted()),
            )));
        }
        if !self.auth_notice.is_empty() {
            auth_lines.push(Line::default());
            auth_lines.push(Line::from(vec![
                Span::styled("auth ", Style::default().fg(color_muted())),
                Span::styled(&self.auth_notice, Style::default().fg(color_accent())),
            ]));
        }
        if !self.status.is_empty() {
            auth_lines.push(Line::from(vec![
                Span::styled("status ", Style::default().fg(color_muted())),
                Span::styled(
                    &self.status,
                    status_message_style(&self.status).add_modifier(Modifier::BOLD),
                ),
            ]));
        }

        let auth_panel = Paragraph::new(auth_lines)
            .style(Style::default().fg(color_text()))
            .block(panel_block("◌", "3) Camera Auth", auth_active))
            .wrap(Wrap { trim: false });
        frame.render_widget(auth_panel, layout[2]);

        let footer_spans = action_hint_spans(&[
            ("Ctrl+L", "Toggle Discovery"),
            ("Tab", "Switch Panel"),
            ("Ctrl+B", "Back to View Streams"),
        ]);
        let footer = Paragraph::new(Line::from(footer_spans))
            .style(Style::default().fg(color_text()))
            .block(panel_block("⌘", "Actions", false));
        frame.render_widget(footer, layout[3]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<AppCommand> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q')) {
            return Ok(AppCommand::Quit);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('l')) {
            self.discovery_enabled = !self.discovery_enabled;
            if self.discovery_enabled {
                self.status = "Discovery resumed.".to_owned();
                self.next_discovery_at = Some(Instant::now());
            } else {
                self.status = "Discovery paused.".to_owned();
                if let Some(handle) = self.pending_discovery.take() {
                    handle.abort();
                }
                self.pending_discovery_events_rx = None;
                self.discovery_started_at = None;
                self.discovery_progress = discovery::DiscoveryProgress::default();
            }
            return Ok(AppCommand::None);
        }

        match self.screen {
            Screen::ViewStreams => Ok(self.handle_view_streams_key(key)),
            Screen::Settings => self.handle_settings_key(key),
        }
    }

    fn handle_view_streams_key(&mut self, key: KeyEvent) -> AppCommand {
        match key.code {
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.screen = Screen::Settings;
                self.focus = SettingsFocus::List;
                self.next_discovery_at = Some(Instant::now());
                AppCommand::None
            }
            _ => AppCommand::None,
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent) -> Result<AppCommand> {
        match key.code {
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.screen = Screen::ViewStreams;
                return Ok(AppCommand::None);
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    SettingsFocus::List => SettingsFocus::AuthDisplayName,
                    SettingsFocus::AuthDisplayName
                    | SettingsFocus::AuthUsername
                    | SettingsFocus::AuthPassword => SettingsFocus::List,
                };
                return Ok(AppCommand::None);
            }
            _ => {}
        }

        match self.focus {
            SettingsFocus::List => self.handle_list_focus(key),
            SettingsFocus::AuthDisplayName => self.handle_auth_focus(key, AuthField::DisplayName),
            SettingsFocus::AuthUsername => self.handle_auth_focus(key, AuthField::Username),
            SettingsFocus::AuthPassword => self.handle_auth_focus(key, AuthField::Password),
        }
    }

    fn handle_list_focus(&mut self, key: KeyEvent) -> Result<AppCommand> {
        match key.code {
            KeyCode::Up => {
                self.discovered_cursor = self.discovered_cursor.saturating_sub(1);
            }
            KeyCode::Down => {
                if !self.discovered.is_empty() {
                    self.discovered_cursor =
                        (self.discovered_cursor + 1).min(self.discovered.len() - 1);
                }
            }
            KeyCode::Enter => {
                self.toggle_current_selection();
            }
            KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_current_selection();
            }
            _ => {}
        }

        Ok(AppCommand::None)
    }

    fn handle_auth_focus(&mut self, key: KeyEvent, field: AuthField) -> Result<AppCommand> {
        match key.code {
            KeyCode::Up => {
                self.focus = match self.focus {
                    SettingsFocus::AuthDisplayName => SettingsFocus::AuthPassword,
                    SettingsFocus::AuthUsername => SettingsFocus::AuthDisplayName,
                    SettingsFocus::AuthPassword => SettingsFocus::AuthUsername,
                    SettingsFocus::List => SettingsFocus::List,
                };
                return Ok(AppCommand::None);
            }
            KeyCode::Down => {
                self.focus = match self.focus {
                    SettingsFocus::AuthDisplayName => SettingsFocus::AuthUsername,
                    SettingsFocus::AuthUsername => SettingsFocus::AuthPassword,
                    SettingsFocus::AuthPassword => SettingsFocus::AuthDisplayName,
                    SettingsFocus::List => SettingsFocus::List,
                };
                return Ok(AppCommand::None);
            }
            _ => {}
        }

        let Some(selected_index) = self.current_selected_stream_index() else {
            self.status = "Select a discovered camera checkbox first.".to_owned();
            return Ok(AppCommand::None);
        };
        let target_base_url = self.selected_streams[selected_index].base_url.clone();

        {
            let selected = &mut self.selected_streams[selected_index];
            match field {
                AuthField::DisplayName => {
                    let value = selected.display_name.get_or_insert_with(String::new);
                    edit_text_field(value, key, true)?;
                    if value.trim().is_empty() {
                        selected.display_name = None;
                    }
                }
                AuthField::Username => {
                    let value = selected.username.get_or_insert_with(String::new);
                    edit_text_field(value, key, true)?;
                    if value.trim().is_empty() {
                        selected.username = None;
                    }
                }
                AuthField::Password => {
                    let value = selected.password.get_or_insert_with(String::new);
                    edit_text_field(value, key, true)?;
                    if value.trim().is_empty() {
                        selected.password = None;
                    }
                }
            }
        }
        self.propagate_camera_profile(selected_index);

        let target_label = camera_auth_key(&target_base_url).unwrap_or(target_base_url.clone());

        if self.persist_selection() {
            self.auth_notice = format!("Saved credentials for {}.", target_label);
            self.status = "Auth saved.".to_owned();
            // Force worker refresh even if URL comparison would otherwise look unchanged.
            self.live_stream_specs.clear();
        }
        Ok(AppCommand::None)
    }

    fn apply_discovery_result(&mut self, result: Result<Vec<DiscoveredStream>>) {
        match result {
            Ok(discovered) => {
                self.discovered_cache = dedupe_discovered_streams_by_url(discovered);
                self.refresh_discovered_view_preserving_cursor();
                self.status = format!("Discovered {} stream endpoint(s)", self.discovered.len());
                let _ = cache::save_cache(&DiscoveryCache {
                    streams: self.discovered_cache.clone(),
                });
                self.next_discovery_at = Some(Instant::now() + DISCOVERY_RESCAN_INTERVAL);
            }
            Err(err) => {
                self.status = format!("Discovery failed: {err:#}");
                self.next_discovery_at = Some(Instant::now() + DISCOVERY_RETRY_INTERVAL);
            }
        }
    }

    fn merge_discovered_stream(&mut self, stream: DiscoveredStream) {
        self.discovered_cache.push(stream);
        self.discovered_cache =
            dedupe_discovered_streams_by_url(std::mem::take(&mut self.discovered_cache));
        self.refresh_discovered_view_preserving_cursor();
        self.status = format!("Discovering... {} endpoint(s) found", self.discovered.len());
        let _ = cache::save_cache(&DiscoveryCache {
            streams: self.discovered_cache.clone(),
        });
    }

    fn refresh_discovered_view_preserving_cursor(&mut self) {
        let previous_base_url = self
            .discovered
            .get(self.discovered_cursor)
            .map(|item| item.base_url.clone());
        self.discovered = sanitize_discovered(self.discovered_cache.clone());
        if self.discovered.is_empty() {
            self.discovered_cursor = 0;
            return;
        }

        if let Some(base_url) = previous_base_url
            && let Some(index) = self
                .discovered
                .iter()
                .position(|item| item.base_url == base_url)
        {
            self.discovered_cursor = index;
            return;
        }

        self.discovered_cursor = self
            .discovered_cursor
            .min(self.discovered.len().saturating_sub(1));
    }

    fn start_discovery(&mut self, args: DiscoverArgs) {
        if self.pending_discovery.is_some() {
            self.status = "Discovery is already running...".to_owned();
            return;
        }

        self.status = "Running discovery...".to_owned();
        self.discovery_started_at = Some(Instant::now());
        self.discovery_progress = discovery::DiscoveryProgress::default();
        let (event_tx, event_rx) = tokio_mpsc::unbounded_channel();
        self.pending_discovery_events_rx = Some(event_rx);
        self.pending_discovery = Some(tokio::spawn(async move {
            discovery::discover_streams_with_events(&args, Some(event_tx)).await
        }));
    }

    fn sync_discovery_lifecycle(&mut self) {
        if !self.discovery_enabled || self.screen != Screen::Settings {
            if let Some(handle) = self.pending_discovery.take() {
                handle.abort();
            }
            self.pending_discovery_events_rx = None;
            self.discovery_started_at = None;
            self.discovery_progress = discovery::DiscoveryProgress::default();
            return;
        }

        if self.pending_discovery.is_some() {
            return;
        }

        let should_start = self
            .next_discovery_at
            .is_none_or(|next| Instant::now() >= next);
        if should_start {
            self.start_discovery(self.discovery_args());
            self.next_discovery_at = None;
        }
    }

    fn poll_discovery_events(&mut self) {
        let Some(rx) = self.pending_discovery_events_rx.as_mut() else {
            return;
        };

        let mut found = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(discovery::DiscoveryEvent::Progress(progress)) => {
                    self.discovery_progress = progress;
                }
                Ok(discovery::DiscoveryEvent::StreamFound(stream)) => {
                    found.push(stream);
                }
                Err(tokio_mpsc::error::TryRecvError::Empty) => break,
                Err(tokio_mpsc::error::TryRecvError::Disconnected) => {
                    self.pending_discovery_events_rx = None;
                    break;
                }
            }
        }
        for stream in found {
            self.merge_discovered_stream(stream);
        }
    }

    async fn poll_discovery_result(&mut self) {
        let Some(handle) = self.pending_discovery.take() else {
            return;
        };

        if !handle.is_finished() {
            self.pending_discovery = Some(handle);
            return;
        }

        let result = match handle.await {
            Ok(result) => result,
            Err(err) => Err(anyhow!("discovery task failed: {err}")),
        };
        self.discovery_started_at = None;
        self.pending_discovery_events_rx = None;
        self.discovery_progress.completed = self.discovery_progress.total;
        self.apply_discovery_result(result);
    }

    fn selected_live_stream_specs(&self) -> Vec<LiveStreamSpec> {
        let mut streams = Vec::with_capacity(self.selected_streams.len());
        for stream in self.selected_streams.iter().filter(|stream| stream.enabled) {
            let username_raw = stream.username.as_deref().unwrap_or("");
            let username = if username_raw.trim().is_empty() {
                ""
            } else {
                username_raw
            };
            let password = stream.password.as_deref().unwrap_or("");
            let url = apply_rtsp_credentials(&stream.base_url, username, password)
                .unwrap_or_else(|_| stream.base_url.clone());
            streams.push(LiveStreamSpec {
                url,
                label: display_stream_label(stream),
            });
        }
        streams
    }

    fn sync_live_viewer_workers(&mut self) {
        let desired = self.selected_live_stream_specs();
        if desired == self.live_stream_specs {
            return;
        }

        for worker in self.live_workers.drain(..) {
            worker.abort();
        }
        self.live_tiles.clear();
        self.live_stream_specs = desired.clone();
        self.kitty_upload_states.clear();

        if desired.is_empty() {
            self.live_geometry_tx = None;
            return;
        }

        let (geometry_tx, geometry_rx) = watch::channel(RenderGeometry::default());
        self.live_geometry_tx = Some(geometry_tx);

        for stream in desired {
            let tile = Arc::new(EmbeddedTileState::new(stream.label.clone()));
            let worker_tile = tile.clone();
            let worker_geometry_rx = geometry_rx.clone();
            self.live_workers.push(tokio::spawn(async move {
                run_embedded_stream_worker(
                    stream.url,
                    TransportMode::Tcp,
                    LIVE_TARGET_FPS,
                    worker_tile,
                    worker_geometry_rx,
                )
                .await;
            }));
            self.live_tiles.push(tile);
        }

        self.kitty_upload_states = (0..self.live_tiles.len())
            .map(|_| KittyUploadState::default())
            .collect();
    }

    fn stop_live_viewer_workers(&mut self) {
        for worker in self.live_workers.drain(..) {
            worker.abort();
        }
        self.live_tiles.clear();
        self.live_stream_specs.clear();
        self.live_geometry_tx = None;
        self.kitty_upload_states.clear();
    }

    fn render_kitty_graphics(&mut self, terminal: &mut AppTerminal) -> Result<()> {
        if !self.kitty_graphics_enabled
            || self.screen != Screen::ViewStreams
            || self.live_tiles.is_empty()
        {
            if self.kitty_images_drawn {
                self.clear_kitty_images(terminal)?;
            }
            return Ok(());
        }
        let area = terminal
            .size()
            .context("failed to read terminal size for graphics")?;
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(3)])
            .split(area.into());
        let viewer_area = layout[0];
        let view_count = self.live_tiles.len();
        let (aspect_w, aspect_h) = target_video_aspect_in_cells();
        let (rows, cols) = compute_grid_dimensions(view_count, viewer_area, aspect_w, aspect_h);
        let grid_rects = build_grid_rects_for_aspect(viewer_area, rows, cols, aspect_w, aspect_h);

        if let (Some(first), Some(geometry_tx)) =
            (grid_rects.first().copied(), &self.live_geometry_tx)
        {
            let geometry = compute_render_geometry(inner_cell(first), view_count);
            if *geometry_tx.borrow() != geometry {
                let _ = geometry_tx.send(geometry);
            }
        }

        if self.kitty_upload_states.len() != view_count {
            self.kitty_upload_states = (0..view_count)
                .map(|_| KittyUploadState::default())
                .collect();
        }

        let backend = terminal.backend_mut();
        // Kitty upload burst behavior affects all decode backends, so pace uploads uniformly.
        let upload_pacing_enabled = true;
        let upload_interval = Duration::from_millis(1_000_u64 / u64::from(LIVE_TARGET_FPS.max(1)));
        let stream_count = view_count.max(1);
        for idx in 0..view_count {
            if idx >= grid_rects.len() {
                break;
            }
            let tile = &self.live_tiles[idx];

            let inner = inner_cell(grid_rects[idx]);
            if inner.width < 2 || inner.height < 2 {
                continue;
            }

            let row = u32::from(inner.y) + 1;
            let col = u32::from(inner.x) + 1;
            let cell_cols = inner.width.max(1);
            let cell_rows = inner.height.max(1);
            let upload_state = &mut self.kitty_upload_states[idx];

            let snapshot = tile.inner.read();
            if snapshot.frame_rgb.is_empty()
                || snapshot.frame_width == 0
                || snapshot.frame_height == 0
            {
                continue;
            }

            let frame_seq = snapshot.frame_seq;
            let frame_width = snapshot.frame_width;
            let frame_height = snapshot.frame_height;
            let frame_changed = upload_state.encoded_seq != frame_seq;
            let previous_rgb = if frame_changed {
                Some(snapshot.frame_rgb.clone())
            } else {
                None
            };
            let placement_changed = upload_state.row != row
                || upload_state.col != col
                || upload_state.cell_cols != cell_cols
                || upload_state.cell_rows != cell_rows
                || upload_state.frame_width != frame_width
                || upload_state.frame_height != frame_height;
            let should_upload = frame_changed || placement_changed;
            if !should_upload {
                continue;
            }

            if upload_pacing_enabled && frame_changed && !placement_changed {
                let now = Instant::now();
                let scheduled = upload_state.next_upload_at.get_or_insert_with(|| {
                    let interval_ms =
                        u64::try_from(upload_interval.as_millis()).unwrap_or(u64::MAX);
                    let phase_ms = (idx as u64)
                        .saturating_mul(interval_ms)
                        .saturating_div(stream_count as u64);
                    now + Duration::from_millis(phase_ms)
                });
                if now < *scheduled {
                    continue;
                }
            }

            let can_try_delta = frame_changed
                && !placement_changed
                && upload_state.previous_width == frame_width
                && upload_state.previous_height == frame_height
                && upload_state.previous_rgb.len() == snapshot.frame_rgb.len();
            let changed_rect = if can_try_delta {
                compute_changed_rect(
                    &upload_state.previous_rgb,
                    &snapshot.frame_rgb,
                    frame_width,
                    frame_height,
                )
            } else {
                None
            };

            let use_delta_upload = if let Some(rect) = changed_rect {
                let rect_area = rect.width.saturating_mul(rect.height);
                let full_area = frame_width.saturating_mul(frame_height).max(1);
                rect_area.saturating_mul(100) < full_area.saturating_mul(DELTA_MAX_AREA_PERCENT)
            } else {
                false
            };

            if can_try_delta && changed_rect.is_none() {
                upload_state.encoded_seq = frame_seq;
                if let Some(previous_rgb) = previous_rgb {
                    upload_state.previous_rgb = previous_rgb;
                    upload_state.previous_width = frame_width;
                    upload_state.previous_height = frame_height;
                }
                drop(snapshot);
                continue;
            }

            let delta_rect = changed_rect.unwrap_or(PixelRect {
                x: 0,
                y: 0,
                width: frame_width,
                height: frame_height,
            });

            let control = if use_delta_upload {
                extract_rgb_rect(
                    &snapshot.frame_rgb,
                    frame_width,
                    delta_rect,
                    &mut upload_state.delta_rgb,
                );
                upload_state.encoded_payload.clear();
                BASE64_ENGINE
                    .encode_string(&upload_state.delta_rgb, &mut upload_state.encoded_payload);
                format!(
                    "a=f,f=24,i={},r=1,x={},y={},s={},v={},q=2",
                    idx + 1,
                    delta_rect.x,
                    delta_rect.y,
                    delta_rect.width,
                    delta_rect.height,
                )
            } else if self.kitty_transfer_mode == KittyTransferMode::File {
                upload_state.ensure_file_transfer(idx)?;
                let transfer = upload_state
                    .file_transfer
                    .as_mut()
                    .ok_or_else(|| anyhow!("kitty file transfer state is missing"))?;

                transfer
                    .file
                    .seek(SeekFrom::Start(0))
                    .context("failed rewinding kitty transfer file")?;
                transfer
                    .file
                    .write_all(&snapshot.frame_rgb)
                    .context("failed writing kitty transfer file")?;
                let frame_len = snapshot.frame_rgb.len();
                if transfer.size != frame_len {
                    transfer
                        .file
                        .set_len(frame_len as u64)
                        .context("failed sizing kitty transfer file")?;
                    transfer.size = frame_len;
                }
                transfer
                    .file
                    .flush()
                    .context("failed flushing kitty transfer file")?;

                format!(
                    "a=T,f=24,s={},v={},i={},p=1,c={},r={},C=1,z=-1,q=2,t=f,S={},O=0",
                    frame_width,
                    frame_height,
                    idx + 1,
                    cell_cols,
                    cell_rows,
                    transfer.size
                )
            } else {
                upload_state.encoded_payload.clear();
                BASE64_ENGINE.encode_string(&snapshot.frame_rgb, &mut upload_state.encoded_payload);
                format!(
                    "a=T,f=24,s={},v={},i={},p=1,c={},r={},C=1,z=-1,q=2",
                    frame_width,
                    frame_height,
                    idx + 1,
                    cell_cols,
                    cell_rows
                )
            };
            drop(snapshot);

            write!(backend, "\x1b[{row};{col}H").context("failed writing kitty cursor command")?;
            if use_delta_upload {
                write_kitty_chunked(
                    backend,
                    &control,
                    &upload_state.encoded_payload,
                    Some("a=f"),
                )?;
            } else if let Some(transfer) = upload_state.file_transfer.as_ref() {
                if self.kitty_transfer_mode == KittyTransferMode::File {
                    write_kitty_chunked(backend, &control, &transfer.encoded_path, None)?;
                } else {
                    write_kitty_chunked(backend, &control, &upload_state.encoded_payload, None)?;
                }
            } else {
                write_kitty_chunked(backend, &control, &upload_state.encoded_payload, None)?;
            }
            upload_state.encoded_seq = frame_seq;
            if let Some(previous_rgb) = previous_rgb {
                upload_state.previous_rgb = previous_rgb;
                upload_state.previous_width = frame_width;
                upload_state.previous_height = frame_height;
            }
            upload_state.row = row;
            upload_state.col = col;
            upload_state.cell_cols = cell_cols;
            upload_state.cell_rows = cell_rows;
            upload_state.frame_width = frame_width;
            upload_state.frame_height = frame_height;
            if upload_pacing_enabled {
                let now = Instant::now();
                let mut next = upload_state.next_upload_at.unwrap_or(now + upload_interval);
                while next <= now {
                    next += upload_interval;
                }
                upload_state.next_upload_at = Some(next);
            } else {
                upload_state.next_upload_at = None;
            }
        }
        backend.flush().context("failed flushing kitty graphics")?;
        self.kitty_images_drawn = true;
        Ok(())
    }

    fn clear_kitty_images(&mut self, terminal: &mut AppTerminal) -> Result<()> {
        let backend = terminal.backend_mut();
        backend
            .write_all(b"\x1b_Ga=d,d=A,q=2;\x1b\\")
            .context("failed clearing kitty images")?;
        backend.flush().context("failed flushing kitty clear")?;
        self.kitty_images_drawn = false;
        Ok(())
    }

    fn discovery_phase_text(&self) -> String {
        if self.discovery_progress.total == 0 {
            return "Discovering: preparing probe targets".to_owned();
        }
        "Discovering: probing RTSP endpoints (DESCRIBE)".to_owned()
    }

    fn discovery_loading_bar(&self, width: usize) -> String {
        let width = width.max(12);
        let total = self.discovery_progress.total.max(1);
        let completed = self.discovery_progress.completed.min(total);
        let filled = completed.saturating_mul(width) / total;

        let mut bar = String::with_capacity(width + 2);
        bar.push('[');
        for idx in 0..width {
            if idx < filled {
                bar.push('=');
            } else {
                bar.push('-');
            }
        }
        bar.push(']');
        let pct = if self.discovery_progress.total == 0 {
            0.0
        } else {
            (completed as f32 * 100.0) / self.discovery_progress.total as f32
        };
        bar.push(' ');
        bar.push_str(&format!("{pct:>5.1}%"));
        bar
    }

    fn discovery_args(&self) -> DiscoverArgs {
        DiscoverArgs {
            cidrs: Vec::new(),
            ports: default_ports(),
            paths: default_paths(),
            username: None,
            password: None,
            timeout_ms: 1200,
            concurrency: 96,
            max_hosts_per_subnet: 2048,
            no_save: false,
            onvif: self.discovery_form.onvif,
        }
    }

    fn is_selected(&self, base_url: &str) -> bool {
        self.selected_streams
            .iter()
            .any(|stream| stream.base_url == base_url && stream.enabled)
    }

    fn toggle_current_selection(&mut self) {
        let Some(item) = self.discovered.get(self.discovered_cursor).cloned() else {
            return;
        };

        if let Some(existing_index) = self
            .selected_streams
            .iter()
            .position(|stream| stream.base_url == item.base_url)
        {
            let existing = &mut self.selected_streams[existing_index];
            existing.enabled = !existing.enabled;
            self.status = if existing.enabled {
                "Camera endpoint added to View Streams".to_owned()
            } else {
                "Camera endpoint removed from View Streams".to_owned()
            };
        } else {
            let inherited = inherited_camera_profile(&self.selected_streams, &item.base_url);
            let inherited_auth = inherited
                .as_ref()
                .and_then(|stream| stream.username.as_deref())
                .is_some_and(|name| !name.trim().is_empty());
            self.selected_streams.push(SelectedStream {
                base_url: item.base_url.clone(),
                enabled: true,
                username: inherited.as_ref().and_then(|stream| stream.username.clone()),
                password: inherited.as_ref().and_then(|stream| stream.password.clone()),
                display_name: inherited.and_then(|stream| stream.display_name.clone()),
            });
            self.status = if inherited_auth {
                "Camera endpoint added to View Streams (reused camera auth).".to_owned()
            } else {
                "Camera endpoint added to View Streams".to_owned()
            };
        }
        self.selected_streams =
            dedupe_selected_streams_by_url(std::mem::take(&mut self.selected_streams));

        let _ = self.persist_selection();
    }

    fn current_selected_stream_index(&self) -> Option<usize> {
        let item = self.discovered.get(self.discovered_cursor)?;
        self.selected_streams
            .iter()
            .position(|stream| stream.base_url == item.base_url && stream.enabled)
    }

    fn current_selected_auth(&self) -> Option<(String, String, String, String)> {
        let item = self.discovered.get(self.discovered_cursor)?;
        let selected = self
            .selected_streams
            .iter()
            .find(|stream| stream.base_url == item.base_url && stream.enabled)?;

        let selected_ip =
            camera_ip_key(&selected.base_url).unwrap_or_else(|| selected.base_url.clone());
        Some((
            selected_ip,
            selected.display_name.clone().unwrap_or_default(),
            selected.username.clone().unwrap_or_default(),
            selected.password.clone().unwrap_or_default(),
        ))
    }

    fn persist_selection(&mut self) -> bool {
        self.selected_streams =
            dedupe_selected_streams_by_url(std::mem::take(&mut self.selected_streams));
        let payload = SelectionCache {
            streams: self.selected_streams.clone(),
        };

        match cache::save_selection_cache(&payload) {
            Ok(()) => true,
            Err(err) => {
                self.status = format!("Failed saving selection: {err:#}");
                false
            }
        }
    }

    fn propagate_camera_profile(&mut self, source_index: usize) {
        let Some(source) = self.selected_streams.get(source_index).cloned() else {
            return;
        };
        let Some(source_key) = camera_auth_key(&source.base_url) else {
            return;
        };

        for (idx, stream) in self.selected_streams.iter_mut().enumerate() {
            if idx == source_index {
                continue;
            }
            if camera_auth_key(&stream.base_url).as_deref() != Some(source_key.as_str()) {
                continue;
            }
            stream.display_name = source.display_name.clone();
            stream.username = source.username.clone();
            stream.password = source.password.clone();
        }
    }
}

struct EmbeddedTileSnapshot {
    stream_label: String,
    frame_rgb: Vec<u8>,
    frame_width: usize,
    frame_height: usize,
    frame_seq: u64,
    status: String,
    network_kbps: f32,
    network_window_started_at: Instant,
    network_window_bytes: u64,
    decode_errors: u64,
}

impl EmbeddedTileSnapshot {
    fn new(stream_label: String) -> Self {
        let now = Instant::now();
        Self {
            stream_label,
            frame_rgb: Vec::new(),
            frame_width: 0,
            frame_height: 0,
            frame_seq: 0,
            status: "connecting".to_owned(),
            network_kbps: 0.0,
            network_window_started_at: now,
            network_window_bytes: 0,
            decode_errors: 0,
        }
    }
}

struct EmbeddedTileState {
    inner: RwLock<EmbeddedTileSnapshot>,
}

impl EmbeddedTileState {
    fn new(stream_label: String) -> Self {
        Self {
            inner: RwLock::new(EmbeddedTileSnapshot::new(stream_label)),
        }
    }

    fn set_status(&self, status: impl Into<String>) {
        self.inner.write().status = status.into();
    }

    fn set_frame(&self, frame_rgb: &mut Vec<u8>, frame_width: usize, frame_height: usize) {
        let mut snapshot = self.inner.write();
        std::mem::swap(&mut snapshot.frame_rgb, frame_rgb);
        frame_rgb.clear();
        snapshot.frame_width = frame_width;
        snapshot.frame_height = frame_height;
        snapshot.frame_seq = snapshot.frame_seq.saturating_add(1);
        "streaming".clone_into(&mut snapshot.status);
    }

    fn record_network_bytes(&self, bytes: usize) {
        let now = Instant::now();
        let mut snapshot = self.inner.write();
        snapshot.network_window_bytes = snapshot.network_window_bytes.saturating_add(bytes as u64);
        let elapsed = now.saturating_duration_since(snapshot.network_window_started_at);
        if elapsed >= Duration::from_millis(500) {
            let secs = elapsed.as_secs_f32().max(0.001);
            snapshot.network_kbps = (snapshot.network_window_bytes as f32 * 8.0) / (secs * 1_000.0);
            snapshot.network_window_started_at = now;
            snapshot.network_window_bytes = 0;
        }
    }

    fn inc_decode_error(&self) {
        let mut snapshot = self.inner.write();
        snapshot.decode_errors = snapshot.decode_errors.saturating_add(1);
    }
}

async fn run_embedded_stream_worker(
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

fn display_stream_label(stream: &SelectedStream) -> String {
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
    if !send_decode_job(decode_tx, DecodeJob::Reset, true) {
        return Err(anyhow!("decode thread unavailable"));
    }

    if let Some(extra_config) = read_h264_extra_config(&demuxed, stream_index) {
        if !send_decode_job(decode_tx, DecodeJob::ExtraConfig(extra_config), true) {
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
        tile.record_network_bytes(frame.data().len());

        if frame.has_new_parameters()
            && let Some(extra_config) = read_h264_extra_config(&demuxed, stream_index)
        {
            let _ = send_decode_job(decode_tx, DecodeJob::ExtraConfig(extra_config), true);
        }

        let geometry = *geometry_rx.borrow();
        let mut payload = Vec::with_capacity(frame.data().len());
        payload.extend_from_slice(frame.data());
        let sent = send_decode_job(
            decode_tx,
            DecodeJob::FrameAvcc {
                avcc: payload,
                geometry,
            },
            false,
        );
        if !sent {
            return Err(anyhow!("decode thread unavailable"));
        }
    }

    Ok(())
}

fn spawn_stream_decoder_thread(
    tile: Arc<EmbeddedTileState>,
    target_fps: u16,
) -> SyncSender<DecodeJob> {
    let (tx, rx) = mpsc::sync_channel::<DecodeJob>(8);
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
            let mut last_present = Instant::now();

            while let Ok(job) = rx.recv() {
                match job {
                    DecodeJob::Reset => {
                        decoder = create_stream_decoder(&thread_tile);
                        annexb_buffer.clear();
                        rgb_buffer.clear();
                        scale_plan = None;
                        last_present = Instant::now();
                    }
                    DecodeJob::ExtraConfig(extra) => {
                        let _ = decoder.decode(&extra);
                    }
                    DecodeJob::FrameAvcc { avcc, geometry } => {
                        if let Err(_err) = avcc_frame_to_annexb(&avcc, &mut annexb_buffer) {
                            thread_tile.inc_decode_error();
                            continue;
                        }

                        match decoder.decode(&annexb_buffer) {
                            Ok(Some(yuv)) => {
                                if let Some(target_interval) = target_interval
                                    && last_present.elapsed() < target_interval
                                {
                                    continue;
                                }
                                let (src_w, src_h) = yuv.dimensions();
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
            }
        });

    tx
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
) -> bool {
    match tx.try_send(job) {
        Ok(()) => true,
        Err(TrySendError::Full(job)) => {
            if !allow_blocking_when_full {
                return true;
            }
            tx.send(job).is_ok()
        }
        Err(TrySendError::Disconnected(_)) => false,
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

fn ensure_yuv420_scale_plan<'a>(
    cache: &'a mut Option<Yuv420ScalePlan>,
    src_width: usize,
    src_height: usize,
    target_width: usize,
    target_height: usize,
) -> Option<&'a Yuv420ScalePlan> {
    if src_width == 0 || src_height == 0 || target_width == 0 || target_height == 0 {
        return None;
    }

    let output_width = target_width.min(src_width).max(2) & !1;
    let output_height = target_height.min(src_height).max(2) & !1;

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

    for y in 0..height {
        let row_start = y.saturating_mul(width).saturating_mul(3);
        for x in 0..width {
            let idx = row_start + x.saturating_mul(3);
            if previous[idx] != current[idx]
                || previous[idx + 1] != current[idx + 1]
                || previous[idx + 2] != current[idx + 2]
            {
                changed = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
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

fn compute_render_geometry(tile: Rect, stream_count: usize) -> RenderGeometry {
    let tile_cols = usize::from(tile.width.max(2));
    let tile_rows = usize::from(tile.height.max(2));
    let (cell_px_w, cell_px_h) = terminal_cell_pixel_size().unwrap_or((8, 16));

    let base_width = tile_cols.saturating_mul(cell_px_w);
    let base_height = tile_rows.saturating_mul(cell_px_h);
    let quality = quality_scale_for_grid(stream_count, tile_cols, tile_rows)
        * oversample_for_stream_count(stream_count);
    let (max_width, max_height) = render_max_dims_for_stream_count(stream_count);

    let scaled_width = (base_width as f32 * quality).round() as usize;
    let scaled_height = (base_height as f32 * quality).round() as usize;

    RenderGeometry {
        width: even_clamp(scaled_width, MIN_RENDER_WIDTH, max_width),
        height: even_clamp(scaled_height, MIN_RENDER_HEIGHT, max_height),
    }
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

fn compute_grid_dimensions(
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

fn target_video_aspect_in_cells() -> (u32, u32) {
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

fn build_grid_rects_for_aspect(
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

fn inner_cell(cell: Rect) -> Rect {
    Rect {
        x: cell.x.saturating_add(1),
        y: cell.y.saturating_add(1),
        width: cell.width.saturating_sub(2),
        height: cell.height.saturating_sub(2),
    }
}

#[derive(Debug)]
struct DiscoveryForm {
    onvif: bool,
}

impl Default for DiscoveryForm {
    fn default() -> Self {
        Self { onvif: true }
    }
}

#[derive(Debug, Clone)]
struct DiscoveredItem {
    base_url: String,
    status: u16,
    label: String,
}

#[derive(Debug)]
enum AppCommand {
    None,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthField {
    DisplayName,
    Username,
    Password,
}

fn sanitize_discovered(streams: Vec<DiscoveredStream>) -> Vec<DiscoveredItem> {
    let mut seen = std::collections::HashSet::new();
    let mut sanitized = Vec::new();
    let mut hosts_without_stream_endpoint: std::collections::HashMap<String, u16> =
        std::collections::HashMap::new();
    let mut hosts_with_stream_endpoint = std::collections::HashSet::new();

    for stream in streams {
        let Ok(base_url) = normalize_rtsp_url_without_auth(&stream.url) else {
            continue;
        };
        if !seen.insert(base_url.clone()) {
            continue;
        }
        let Ok(parsed) = url::Url::parse(&base_url) else {
            continue;
        };
        let host = parsed.host_str().unwrap_or("unknown");
        let port = parsed.port_or_known_default().unwrap_or(554);
        let host_port = format!("{host}:{port}");

        if !is_selectable_stream_endpoint(&base_url) {
            hosts_without_stream_endpoint
                .entry(host_port)
                .and_modify(|status| *status = (*status).max(stream.status))
                .or_insert(stream.status);
            continue;
        }
        hosts_with_stream_endpoint.insert(host_port);

        sanitized.push(DiscoveredItem {
            label: format!("{host}:{port} {}", endpoint_display(&base_url)),
            base_url,
            status: stream.status,
        });
    }

    for (host_port, status) in hosts_without_stream_endpoint {
        if hosts_with_stream_endpoint.contains(&host_port) {
            continue;
        }
        for endpoint in ["/stream2", "/stream1"] {
            let base_url = format!("rtsp://{host_port}{endpoint}");
            sanitized.push(DiscoveredItem {
                label: format!("{host_port} {endpoint}"),
                base_url,
                status,
            });
        }
    }

    sanitized.sort_by(|a, b| a.label.cmp(&b.label));
    sanitized
}

fn is_selectable_stream_endpoint(url: &str) -> bool {
    let hint = endpoint_hint(url);
    // Keep explicit stream endpoints, exclude generic vendor-specific "streaming/channels" style.
    hint.contains("/stream1")
        || hint.contains("/stream2")
        || hint.ends_with("/stream")
        || hint.contains("/stream?")
}

fn endpoint_hint(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
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

fn endpoint_display(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return "/".to_owned();
    };
    let mut path = parsed.path().to_owned();
    if let Some(query) = parsed.query()
        && !query.is_empty()
    {
        path.push('?');
        path.push_str(query);
    }
    if path.is_empty() {
        "/".to_owned()
    } else {
        path
    }
}

fn camera_ip_key(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(host.to_owned())
}

fn camera_auth_key(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default().unwrap_or(554);
    Some(format!("{host}:{port}"))
}

fn inherited_camera_profile(streams: &[SelectedStream], base_url: &str) -> Option<SelectedStream> {
    let target_key = camera_auth_key(base_url)?;
    streams
        .iter()
        .find(|candidate| {
            if camera_auth_key(&candidate.base_url).as_deref() != Some(target_key.as_str()) {
                return false;
            }
            candidate
                .username
                .as_deref()
                .is_some_and(|name| !name.trim().is_empty())
                || candidate.password.as_deref().is_some_and(|pass| !pass.is_empty())
                || candidate
                    .display_name
                    .as_deref()
                    .is_some_and(|name| !name.trim().is_empty())
        })
        .cloned()
}

fn dedupe_selected_streams_by_url(streams: Vec<SelectedStream>) -> Vec<SelectedStream> {
    let mut deduped: Vec<SelectedStream> = Vec::new();
    for stream in streams {
        if let Some(existing) = deduped
            .iter_mut()
            .find(|item| item.base_url == stream.base_url)
        {
            // Keep the latest endpoint choice for this exact stream URL, preserving non-empty values.
            existing.base_url = stream.base_url.clone();
            existing.enabled = stream.enabled;
            if stream
                .display_name
                .as_deref()
                .is_some_and(|name| !name.trim().is_empty())
            {
                existing.display_name = stream.display_name.clone();
            }
            if stream.username.is_some() {
                existing.username = stream.username.clone();
            }
            if stream.password.is_some() {
                existing.password = stream.password.clone();
            }
            continue;
        }
        deduped.push(stream);
    }
    deduped
}

fn dedupe_discovered_streams_by_url(streams: Vec<DiscoveredStream>) -> Vec<DiscoveredStream> {
    let mut by_url: std::collections::HashMap<String, DiscoveredStream> =
        std::collections::HashMap::new();
    for stream in streams {
        let Ok(normalized_url) = normalize_rtsp_url_without_auth(&stream.url) else {
            continue;
        };
        let normalized = DiscoveredStream {
            url: normalized_url.clone(),
            ..stream
        };
        match by_url.entry(normalized_url) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(normalized);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let current = entry.get();
                if normalized.discovered_at_unix >= current.discovered_at_unix {
                    entry.insert(normalized);
                }
            }
        }
    }

    let mut deduped = by_url.into_values().collect::<Vec<_>>();
    deduped.sort_by(|a, b| a.url.cmp(&b.url));
    deduped
}

fn normalize_rtsp_url_without_auth(url: &str) -> Result<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty stream URL"));
    }

    let normalized = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("rtsp://{trimmed}")
    };

    let mut parsed =
        url::Url::parse(&normalized).with_context(|| format!("invalid RTSP URL '{trimmed}'"))?;
    if parsed.scheme() != "rtsp" {
        return Err(anyhow!("unsupported scheme '{}'", parsed.scheme()));
    }

    parsed
        .set_username("")
        .map_err(|()| anyhow!("failed clearing username"))?;
    parsed
        .set_password(None)
        .map_err(|()| anyhow!("failed clearing password"))?;

    Ok(parsed.to_string())
}

fn apply_rtsp_credentials(url: &str, username: &str, password: &str) -> Result<String> {
    let mut parsed = url::Url::parse(url).with_context(|| format!("invalid RTSP URL '{url}'"))?;
    if username.is_empty() {
        return Ok(parsed.to_string());
    }

    // Prefer URL crate's native userinfo handling first.
    if parsed.set_username(username).is_ok() {
        if password.is_empty() {
            parsed
                .set_password(None)
                .map_err(|()| anyhow!("failed clearing password"))?;
        } else {
            parsed
                .set_password(Some(password))
                .map_err(|()| anyhow!("failed applying password"))?;
        }
        return Ok(parsed.to_string());
    }

    // Fallback for user/pass containing characters rejected by set_username/set_password.
    let encoded_user = percent_encode_userinfo(username);
    let encoded_pass = percent_encode_userinfo(password);
    let scheme = parsed.scheme();
    let tail = &parsed[url::Position::BeforeHost..];

    if password.is_empty() {
        Ok(format!("{scheme}://{encoded_user}@{tail}"))
    } else {
        Ok(format!("{scheme}://{encoded_user}:{encoded_pass}@{tail}"))
    }
}

fn percent_encode_userinfo(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for b in value.bytes() {
        let is_unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if is_unreserved {
            encoded.push(char::from(b));
        } else {
            let _ = std::fmt::Write::write_fmt(&mut encoded, format_args!("%{:02X}", b));
        }
    }
    encoded
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

fn edit_text_field(target: &mut String, key: KeyEvent, allow_spaces: bool) -> Result<AppCommand> {
    match key.code {
        KeyCode::Backspace => {
            let _ = target.pop();
        }
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT)
            {
                return Ok(AppCommand::None);
            }
            if !allow_spaces && c == ' ' {
                return Ok(AppCommand::None);
            }
            target.push(c);
        }
        _ => {}
    }

    Ok(AppCommand::None)
}

fn write_kitty_chunked(
    backend: &mut CrosstermBackend<io::Stdout>,
    control: &str,
    payload: &str,
    continuation_control: Option<&str>,
) -> Result<()> {
    const CHUNK: usize = 4_096;

    let mut offset = 0_usize;
    let payload_len = payload.len();

    while offset < payload_len {
        let next = (offset + CHUNK).min(payload_len);
        let chunk = &payload[offset..next];
        let more = if next < payload_len { b'1' } else { b'0' };

        backend
            .write_all(b"\x1b_G")
            .context("failed writing kitty graphics prefix")?;
        if offset == 0 {
            backend
                .write_all(control.as_bytes())
                .context("failed writing kitty graphics control")?;
            backend
                .write_all(b",m=")
                .context("failed writing kitty graphics separator")?;
        } else {
            if let Some(continuation_control) = continuation_control {
                backend
                    .write_all(continuation_control.as_bytes())
                    .context("failed writing kitty continuation control")?;
                backend
                    .write_all(b",")
                    .context("failed writing kitty continuation separator")?;
            }
            backend
                .write_all(b"m=")
                .context("failed writing kitty graphics chunk marker")?;
        }
        backend
            .write_all(&[more])
            .context("failed writing kitty graphics continuation bit")?;
        backend
            .write_all(b";")
            .context("failed writing kitty graphics chunk start")?;
        backend
            .write_all(chunk.as_bytes())
            .context("failed writing kitty graphics chunk payload")?;
        backend
            .write_all(b"\x1b\\")
            .context("failed writing kitty graphics chunk terminator")?;
        offset = next;
    }

    if payload_len == 0 {
        backend
            .write_all(b"\x1b_G")
            .context("failed writing empty kitty prefix")?;
        backend
            .write_all(control.as_bytes())
            .context("failed writing empty kitty control")?;
        backend
            .write_all(b",m=0;\x1b\\")
            .context("failed writing empty kitty graphics command")?;
    }

    Ok(())
}

fn detect_kitty_graphics_support() -> bool {
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

fn detect_kitty_transfer_mode() -> KittyTransferMode {
    if let Ok(explicit) = std::env::var("RTSP_CLI_KITTY_TRANSFER_MODE") {
        match explicit.trim().to_ascii_lowercase().as_str() {
            "stream" => return KittyTransferMode::Stream,
            "file" => return KittyTransferMode::File,
            _ => {}
        }
    }

    // SSH sessions generally do not share a filesystem path namespace with kitty.
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        KittyTransferMode::Stream
    } else {
        KittyTransferMode::File
    }
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

fn color_text() -> Color {
    active_theme().text
}

fn color_muted() -> Color {
    active_theme().muted
}

fn color_border() -> Color {
    active_theme().border
}

fn color_border_active() -> Color {
    active_theme().border_active
}

fn color_accent() -> Color {
    active_theme().accent
}

fn color_success() -> Color {
    active_theme().success
}

fn color_warning() -> Color {
    active_theme().warning
}

fn color_error() -> Color {
    active_theme().error
}

fn panel_block<'a>(glyph: &'a str, title: &'a str, focused: bool) -> Block<'a> {
    let border_color = if focused {
        color_border_active()
    } else {
        color_border()
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(vec![
            Span::styled(
                format!(" {glyph} "),
                Style::default()
                    .fg(color_accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                title,
                Style::default()
                    .fg(color_text())
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
}

fn action_hint_spans(hints: &[(&'static str, &'static str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (idx, (key, label)) in hints.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled("  |  ", Style::default().fg(color_border())));
        }
        spans.push(Span::styled(
            format!("[{key}]"),
            Style::default()
                .fg(color_accent())
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(color_muted()),
        ));
    }
    spans
}

fn stream_state_color(status: &str) -> Color {
    let lower = status.to_ascii_lowercase();
    if lower.starts_with("stream") {
        color_success()
    } else if lower.starts_with("connect") || lower.starts_with("reconnect") {
        color_warning()
    } else if lower.starts_with("error") {
        color_error()
    } else {
        color_muted()
    }
}

fn status_code_color(status: u16) -> Color {
    match status {
        200..=299 => color_success(),
        300..=399 => color_accent(),
        400..=499 => color_warning(),
        500..=599 => color_error(),
        _ => color_muted(),
    }
}

fn status_message_style(status: &str) -> Style {
    let lower = status.to_ascii_lowercase();
    if lower.contains("fail") || lower.contains("error") || lower.contains("unauthorized") {
        Style::default().fg(color_error())
    } else if lower.contains("running") || lower.contains("discover") {
        Style::default().fg(color_accent())
    } else if lower.contains("saved")
        || lower.contains("added")
        || lower.contains("updated")
        || lower.contains("loaded")
    {
        Style::default().fg(color_success())
    } else {
        Style::default().fg(color_muted())
    }
}

fn checkbox(checked: bool) -> &'static str {
    if checked {
        GLYPH_CHECKED
    } else {
        GLYPH_UNCHECKED
    }
}

fn focus_marker(focused: bool) -> &'static str {
    if focused { GLYPH_ACTIVE } else { " " }
}

#[cfg(test)]
mod tests {
    use super::{parse_bool_value, term_or_program_indicates_kitty_graphics};

    #[test]
    fn kitty_term_hints_are_detected() {
        assert!(term_or_program_indicates_kitty_graphics(
            "xterm-kitty",
            "unknown"
        ));
        assert!(term_or_program_indicates_kitty_graphics("wezterm", "unknown"));
        assert!(term_or_program_indicates_kitty_graphics("xterm-ghostty", "unknown"));
        assert!(term_or_program_indicates_kitty_graphics("foot", "unknown"));
        assert!(term_or_program_indicates_kitty_graphics("foot-extra", "unknown"));
        assert!(term_or_program_indicates_kitty_graphics("konsole", "unknown"));
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
}
