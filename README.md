# RTSP-TUI

RTSP-TUI is a terminal-first RTSP camera viewer built in Rust.

A live multi-camera TUI with Kitty graphics rendering.

## Screenshot

![RTSP-TUI live multi-camera view](<screenshots/RTSP-TUI EXAMPLE 1.png>)
![RTSP-TUI live multi-camera view (alt)](<screenshots/Screenshot 2RTSP-TUI EXAMPLE 2.png>)

<sub><em>AI image replacement for the individual feeds, since I am not sharing real camera footage of my house lol.</em></sub>

## What It Is

The main workflow is:
1. Start the TUI.
2. Let discovery find camera endpoints.
3. Select streams and set per-camera auth/display names.
4. Watch live tiles in the viewer.

## Possible Additions

1. Maybe make it like a somewhat lightweight "NVR" with motion detection that saves the last frame or something idk
2. Make it more accecible for an AI agent (SKILL.MD) like openclaw to automate something based on camera information (Claude watches your house now lol 🙃)

## Requirements

- Rust toolchain
- A terminal with Kitty graphics protocol support (Kitty or Ghostty).
- Reachable RTSP cameras/streams.
- Tested Personally on MacOS (M3 Mac) will likely work on Linux or Windows WSL but no guarantee

## Install

### From crates.io

```bash
cargo install rtsp-tui
```

### From source

```bash
git clone https://github.com/Ty1an/RTSP-TUI.git
cd RTSP-TUI
cargo install --path .
```

### Dev run

```bash
cargo run --release
```

## Quick Start (TUI)

```bash
rtsp-tui
```

Inside the app:
1. Press `Ctrl+S` to open **Settings**.
2. In **Select Cameras**, use `Up/Down` and `Enter` to toggle streams.
3. Press `Tab` to switch to **Camera Auth** and set:
   - Display Name
   - Auth Username
   - Auth Password
4. Press `Ctrl+B` to return to live viewer.

## Keybindings

### Live Viewer
- `Ctrl+S`: Open Settings
- `Ctrl+Q`: Quit

### Settings
- `Up/Down`: Move selection (stream list or auth field focus)
- `Enter`: Toggle selected stream on/off
- `Tab`: Switch between stream list and auth panel
- `Ctrl+B`: Back to viewer
- `Ctrl+Q`: Quit
