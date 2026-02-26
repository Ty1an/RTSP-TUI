#![allow(
    clippy::assigning_clones,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unnecessary_wraps
)]

#[path = "tui_helpers.rs"]
mod helpers;
#[path = "tui_live.rs"]
mod live;

use self::helpers::{
    apply_rtsp_credentials, camera_auth_key, camera_ip_key, dedupe_discovered_streams_by_url,
    dedupe_selected_streams_by_url, inherited_camera_profile, sanitize_discovered,
};
use self::live::{
    EmbeddedTileState, KittyTransferMode, KittyUploadState, RenderGeometry, UploadPrepJob,
    UploadPrepKind, UploadPrepSendOutcome, adapt_render_geometry_for_transfer_mode,
    build_grid_rects_for_aspect, compute_grid_dimensions, compute_render_geometry,
    detect_kitty_graphics_support, detect_kitty_transfer_mode, display_stream_label, inner_cell,
    live_target_fps_for_stream_count, push_kitty_chunked_bytes, queue_upload_prep_job,
    run_embedded_stream_worker, target_video_aspect_in_cells,
};
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
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::io::{self, Seek, SeekFrom, Write};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc as tokio_mpsc, watch};
use tokio::task::JoinHandle;

const UI_IDLE_SLEEP: Duration = Duration::from_millis(16);
const DISCOVERY_RESCAN_INTERVAL: Duration = Duration::from_secs(30);
const DISCOVERY_RETRY_INTERVAL: Duration = Duration::from_secs(6);

const GLYPH_ACTIVE: &str = "▸";
const GLYPH_CHECKED: &str = "◉";
const GLYPH_UNCHECKED: &str = "○";
const GLYPH_BULLET: &str = "•";

static THEME: OnceLock<ThemePalette> = OnceLock::new();
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
    let mut force_ui_draw = true;
    let mut last_ui_signature = None;

    while running {
        app.sync_discovery_lifecycle();
        app.poll_discovery_events();
        app.poll_discovery_result().await;
        app.sync_live_viewer_workers();

        let current_ui_signature = app.ui_state_signature();
        let should_draw_ui =
            force_ui_draw || last_ui_signature.is_none_or(|prev| prev != current_ui_signature);
        if should_draw_ui {
            terminal
                .draw(|frame| app.draw(frame))
                .context("failed drawing TUI frame")?;
            last_ui_signature = Some(current_ui_signature);
            force_ui_draw = false;
        }

        let live_graphics_active = app.kitty_graphics_enabled
            && app.screen == Screen::ViewStreams
            && !app.live_tiles.is_empty();
        let should_render_graphics = if live_graphics_active {
            true
        } else {
            should_draw_ui
        };
        if should_render_graphics {
            if let Err(err) = app.render_kitty_graphics(terminal) {
                app.status = format!("graphics render failed: {err:#}");
                app.kitty_graphics_enabled = false;
                force_ui_draw = true;
            }
        }

        while event::poll(Duration::ZERO).context("failed to poll input")? {
            match event::read().context("failed reading input")? {
                Event::Key(key) => {
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
                    force_ui_draw = true;
                }
                Event::Resize(_, _) => {
                    // Terminal geometry changed; force a full redraw of TUI chrome.
                    force_ui_draw = true;
                    last_ui_signature = None;
                }
                _ => {}
            }
        }

        if !running {
            break;
        }

        let live_graphics_active = app.kitty_graphics_enabled
            && app.screen == Screen::ViewStreams
            && !app.live_tiles.is_empty();
        if live_graphics_active {
            tokio::task::yield_now().await;
            continue;
        }
        tokio::time::sleep(UI_IDLE_SLEEP).await;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Screen {
    ViewStreams,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
        } else {
            discovery_lines.push(Line::from(Span::styled(
                "Waiting for next scan cycle...",
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
        let target_fps = live_target_fps_for_stream_count(desired.len());

        for stream in desired {
            let tile = Arc::new(EmbeddedTileState::new(stream.label.clone()));
            let worker_tile = tile.clone();
            let worker_geometry_rx = geometry_rx.clone();
            self.live_workers.push(tokio::spawn(async move {
                run_embedded_stream_worker(
                    stream.url,
                    TransportMode::Tcp,
                    target_fps,
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

    fn ui_state_signature(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.screen.hash(&mut hasher);
        self.focus.hash(&mut hasher);
        self.status.hash(&mut hasher);
        self.auth_notice.hash(&mut hasher);
        self.discovery_enabled.hash(&mut hasher);
        self.discovery_progress.completed.hash(&mut hasher);
        self.discovery_progress.total.hash(&mut hasher);
        self.pending_discovery.is_some().hash(&mut hasher);
        self.pending_discovery_events_rx.is_some().hash(&mut hasher);
        self.discovered_cursor.hash(&mut hasher);
        self.discovered.len().hash(&mut hasher);
        for camera in &self.discovered {
            camera.base_url.hash(&mut hasher);
            camera.status.hash(&mut hasher);
            camera.label.hash(&mut hasher);
        }
        self.selected_streams.len().hash(&mut hasher);
        for stream in &self.selected_streams {
            stream.base_url.hash(&mut hasher);
            stream.enabled.hash(&mut hasher);
            stream.username.hash(&mut hasher);
            stream.password.hash(&mut hasher);
            stream.display_name.hash(&mut hasher);
        }
        self.live_tiles.len().hash(&mut hasher);
        for tile in &self.live_tiles {
            let snapshot = tile.inner.read();
            snapshot.stream_label.hash(&mut hasher);
            snapshot.status.hash(&mut hasher);
        }
        self.kitty_graphics_enabled.hash(&mut hasher);
        self.kitty_images_drawn.hash(&mut hasher);
        self.kitty_upload_states.len().hash(&mut hasher);
        std::mem::discriminant(&self.kitty_transfer_mode).hash(&mut hasher);
        hasher.finish()
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
            let geometry = adapt_render_geometry_for_transfer_mode(
                compute_render_geometry(inner_cell(first), view_count),
                self.kitty_transfer_mode,
                view_count,
            );
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
        let upload_fps = live_target_fps_for_stream_count(view_count);
        let upload_interval = if upload_fps == 0 {
            None
        } else {
            Some(Duration::from_millis(1_000_u64 / u64::from(upload_fps)))
        };
        let stream_count = view_count.max(1);
        let mut wrote_graphics_command = false;
        let mut graphics_batch = Vec::with_capacity(16 * 1024);
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

            while let Ok(prepared) = upload_state.upload_worker.rx.try_recv() {
                let should_replace = upload_state
                    .prepared
                    .as_ref()
                    .is_none_or(|current| prepared.frame_seq >= current.frame_seq);
                if should_replace {
                    upload_state.prepared = Some(prepared);
                }
            }

            let mut incoming_frame_rgb = std::mem::take(&mut upload_state.source_frame_scratch);
            if let Some((frame_seq, frame_width, frame_height)) =
                tile.take_latest_frame(&mut incoming_frame_rgb)
            {
                let next_frame_rgb = Arc::new(incoming_frame_rgb);
                if let Some(previous_frame_rgb) =
                    upload_state.source_frame_rgb.replace(next_frame_rgb)
                    && let Ok(mut reusable_rgb) = Arc::try_unwrap(previous_frame_rgb)
                {
                    reusable_rgb.clear();
                    upload_state.source_frame_scratch = reusable_rgb;
                }
                upload_state.source_frame_seq = frame_seq;
                upload_state.source_frame_width = frame_width;
                upload_state.source_frame_height = frame_height;
            } else {
                upload_state.source_frame_scratch = incoming_frame_rgb;
            }

            let Some(source_frame_rgb) = upload_state.source_frame_rgb.as_ref().cloned() else {
                continue;
            };
            if source_frame_rgb.is_empty()
                || upload_state.source_frame_width == 0
                || upload_state.source_frame_height == 0
            {
                continue;
            }

            let frame_seq = upload_state.source_frame_seq;
            let frame_width = upload_state.source_frame_width;
            let frame_height = upload_state.source_frame_height;
            let frame_changed = upload_state.encoded_seq != frame_seq;
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

            if let Some(upload_interval) = upload_interval
                && frame_changed
                && !placement_changed
            {
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

            if self.kitty_transfer_mode == KittyTransferMode::Stream
                && frame_changed
                && !placement_changed
                && upload_state.queued_seq != frame_seq
            {
                match queue_upload_prep_job(
                    &upload_state.upload_worker.tx,
                    UploadPrepJob {
                        frame_seq,
                        frame_width,
                        frame_height,
                        frame_rgb: source_frame_rgb.clone(),
                    },
                ) {
                    UploadPrepSendOutcome::Queued => {
                        upload_state.queued_seq = frame_seq;
                    }
                    UploadPrepSendOutcome::Dropped => {
                        tile.inc_dropped_frame();
                    }
                    UploadPrepSendOutcome::Disconnected => {
                        tile.inc_decode_error();
                        tile.set_status("upload prep worker disconnected");
                        continue;
                    }
                }
            }

            let mut continuation_control = None;
            let mut uploaded_frame_seq = frame_seq;
            let payload: &str = if self.kitty_transfer_mode == KittyTransferMode::File {
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
                    .write_all(source_frame_rgb.as_slice())
                    .context("failed writing kitty transfer file")?;
                let frame_len = source_frame_rgb.len();
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

                upload_state.control_buf.clear();
                let _ = write!(
                    &mut upload_state.control_buf,
                    "a=T,f=24,s={},v={},i={},p=1,c={},r={},C=1,z=-1,q=2,t=f,S={},O=0",
                    frame_width,
                    frame_height,
                    idx + 1,
                    cell_cols,
                    cell_rows,
                    transfer.size
                );
                transfer.encoded_path.as_str()
            } else if placement_changed {
                upload_state.encoded_payload.clear();
                BASE64_ENGINE.encode_string(
                    source_frame_rgb.as_slice(),
                    &mut upload_state.encoded_payload,
                );
                upload_state.control_buf.clear();
                let _ = write!(
                    &mut upload_state.control_buf,
                    "a=T,f=24,s={},v={},i={},p=1,c={},r={},C=1,z=-1,q=2",
                    frame_width,
                    frame_height,
                    idx + 1,
                    cell_cols,
                    cell_rows
                );
                upload_state.encoded_payload.as_str()
            } else {
                let Some(prepared) = upload_state.prepared.take() else {
                    continue;
                };
                if prepared.frame_seq > frame_seq {
                    upload_state.prepared = Some(prepared);
                    continue;
                }
                uploaded_frame_seq = prepared.frame_seq;

                match prepared.kind {
                    UploadPrepKind::Unchanged => {
                        upload_state.encoded_seq = prepared.frame_seq;
                        upload_state.frame_width = prepared.frame_width;
                        upload_state.frame_height = prepared.frame_height;
                        upload_state.row = row;
                        upload_state.col = col;
                        upload_state.cell_cols = cell_cols;
                        upload_state.cell_rows = cell_rows;
                        continue;
                    }
                    UploadPrepKind::Full => {
                        upload_state.control_buf.clear();
                        let _ = write!(
                            &mut upload_state.control_buf,
                            "a=T,f=24,s={},v={},i={},p=1,c={},r={},C=1,z=-1,q=2",
                            prepared.frame_width,
                            prepared.frame_height,
                            idx + 1,
                            cell_cols,
                            cell_rows
                        );
                    }
                    UploadPrepKind::Delta { rect } => {
                        continuation_control = Some("a=f");
                        upload_state.control_buf.clear();
                        let _ = write!(
                            &mut upload_state.control_buf,
                            "a=f,f=24,i={},r=1,x={},y={},s={},v={},q=2",
                            idx + 1,
                            rect.x,
                            rect.y,
                            rect.width,
                            rect.height
                        );
                    }
                }
                upload_state.encoded_payload = prepared.encoded_payload;
                upload_state.encoded_payload.as_str()
            };

            let _ = write!(&mut graphics_batch, "\x1b[{row};{col}H");
            push_kitty_chunked_bytes(
                &mut graphics_batch,
                &upload_state.control_buf,
                payload,
                continuation_control,
            );
            wrote_graphics_command = true;
            upload_state.encoded_seq = uploaded_frame_seq;
            upload_state.row = row;
            upload_state.col = col;
            upload_state.cell_cols = cell_cols;
            upload_state.cell_rows = cell_rows;
            upload_state.frame_width = frame_width;
            upload_state.frame_height = frame_height;
            if let Some(upload_interval) = upload_interval {
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
        if wrote_graphics_command {
            backend
                .write_all(&graphics_batch)
                .context("failed writing batched kitty graphics")?;
            backend.flush().context("failed flushing kitty graphics")?;
            self.kitty_images_drawn = true;
        }
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
                username: inherited
                    .as_ref()
                    .and_then(|stream| stream.username.clone()),
                password: inherited
                    .as_ref()
                    .and_then(|stream| stream.password.clone()),
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
