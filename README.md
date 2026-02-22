# RTSP-TUI

RTSP-TUI is a terminal-first RTSP camera viewer built in Rust.

It provides:
- A live multi-camera TUI with Kitty graphics rendering.
- Built-in network discovery for likely RTSP endpoints.
- Per-camera auth + display names from the settings screen.
- Real-time tile metrics (`fps` and `net kbps`).
- Performance toggles like delta updates and duplicate-view stress testing.

## What It Is

The main workflow is:
1. Start the TUI.
2. Let discovery find camera endpoints.
3. Select streams and set per-camera auth/display names.
4. Watch live tiles in the viewer.

RTSP-TUI also includes non-interactive CLI subcommands (`discover`, `streams list`, `view`) for scripting and one-off tasks.

## Requirements

- Rust toolchain (edition 2024 project; current `rust-version` is 1.86).
- A terminal with Kitty graphics protocol support (Kitty or Ghostty).
- Reachable RTSP cameras/streams.

## Install

### From source

```bash
git clone https://github.com/Ty1an/RTSP-TUI.git
cd RTSP-TUI
cargo install --path .
```

### Dev run

```bash
cargo run --release -- tui
```

## Quick Start (TUI)

```bash
rtsp-tui
```

Inside the app:
1. Press `Ctrl+S` to open **Settings**.
2. In **Select Cameras**, use `Up/Down` and `Ctrl+Space` to toggle streams.
3. Press `Tab` to switch to **Camera Auth** and set:
   - Display Name
   - Auth Username
   - Auth Password
4. Press `Ctrl+B` to return to live viewer.

## Keybindings

### Live Viewer
- `Ctrl+S`: Open Settings
- `Ctrl+D`: Toggle delta updates for all streams
- `Ctrl+U`: Toggle duplicate views (duplicates current visible stream set)
- `Ctrl+Q`: Quit

### Settings
- `Up/Down`: Move selection (stream list or auth field focus)
- `Ctrl+Space`: Toggle selected stream on/off
- `Tab`: Switch between stream list and auth panel
- `Ctrl+B`: Back to viewer
- `Ctrl+Q`: Quit

## CLI Commands

### Launch TUI

```bash
rtsp-tui tui
```

(`rtsp-tui` with no subcommand also launches TUI.)

### Discover streams

```bash
rtsp-tui discover
```

Custom scan example:

```bash
rtsp-tui discover --cidr 192.168.1.0/24 --onvif
```

### List cached streams

```bash
rtsp-tui streams list
```

JSON output:

```bash
rtsp-tui streams list --json
```

### Direct grid viewer mode (non-TUI)

```bash
rtsp-tui view --stream rtsp://user:pass@192.168.1.10:554/Streaming/Channels/101
```

Multiple streams:

```bash
rtsp-tui view --stream rtsp://cam1/... --stream rtsp://cam2/... --rows 1 --cols 2
```

## Performance Notes

- The TUI currently targets a capped render/upload pace (20 FPS target in the live viewer pipeline).
- Running with `--release` is strongly recommended for smooth playback.
- Delta updates can reduce bandwidth/upload pressure in mostly-static scenes.
- Duplicate view mode is useful for stress-testing rendering throughput.

## Data Storage

RTSP-TUI stores discovery, stream selection, and theme data under your local data directory in the `rtsp-tui` app folder.

## Screenshots

Screenshots and demos will be added later.
