<p align="center">
  <img src="icons/lumen-icon-128.png" alt="Lumen logo" width="96">
</p>

<h1 align="center">Lumen</h1>

<p align="center">
  <strong>A Windows-first, GPU-accelerated terminal for people who want command-line work to feel fast, visual, and controllable across devices.</strong>
</p>

<p align="center">
  <strong>English</strong> · <a href="README.zh-CN.md">简体中文</a> ·
  <a href="https://github.com/jimhy/lumen/releases">Download</a> ·
  <a href="#build-from-source">Build from source</a> ·
  <a href="server/deploy/README.md">Self-host remote server</a>
</p>

<p align="center">
  <img alt="Windows 10 1809+" src="https://img.shields.io/badge/Windows-10%201809%2B-0078D4?logo=windows">
  <img alt="Rust 1.92+" src="https://img.shields.io/badge/Rust-1.92%2B-f74c00?logo=rust">
  <img alt="License Apache 2.0" src="https://img.shields.io/badge/license-Apache--2.0-blue">
  <img alt="GPU rendering" src="https://img.shields.io/badge/rendering-wgpu%20%2B%20glyphon-7c3aed">
</p>

<p align="center">
  <img src="docs/demo.gif" alt="Lumen screen recording: file tree, PowerShell, command output, and multi-pane splits" width="920">
</p>

Lumen combines a native Windows terminal, an editor-style command input, command blocks,
file browsing, themes, and self-hosted remote control in one Rust app. It is built for
developers who spend the day in PowerShell, switch between projects often, and want a
terminal that helps them see what happened instead of forcing them to scroll through a
wall of text.

> Current status: the local terminal, modern input editor, file tree, multi-pane layout,
> auto-update, account/server plumbing, remote device control, remote file operations,
> and QUIC-assisted P2P data path are implemented and actively polished.

## Why Lumen

| You want to... | Use this | Why it helps |
|---|---|---|
| Write longer commands without fear | Multi-line footer input, PowerShell highlighting, continuation detection | Treat commands more like code: edit first, run when ready. |
| Find and reuse commands quickly | `Ctrl+R` fuzzy history, `↑/↓` history, ghost text, `Tab` completion | Less retyping, fewer context switches, faster repeat work. |
| Understand output at a glance | Command blocks, exit-code badges, elapsed time, block selection | Each command becomes a readable unit you can jump to, copy, and debug. |
| Work in several folders at once | Up to 6 panes, draggable ratios, pane maximize, title-bar swapping | Keep build, logs, server, and scratch commands visible together. |
| Browse files without leaving the terminal | `Ctrl+B` file tree, right-click actions, drag file to insert path | Move through a project, create files, copy paths, and `cd` faster. |
| Control your own machines | Self-host `lumen-server`, sign in on devices, pair with a 9-digit code | Mirror/control a remote Lumen instance and move files between machines. |
| Make the terminal fit your setup | 11 built-in themes, OS light/dark sync, background images, Chinese/English UI | A comfortable terminal is one you keep open all day. |

## Screenshots

| Local workspace | Command workflow | Six-pane split |
|---|---|---|
| <img src="docs/media/lumen-overview.png" alt="Lumen local terminal with file tree" width="300"> | <img src="docs/media/lumen-workflow.png" alt="Lumen command output and split panes" width="300"> | <img src="docs/media/lumen-splits.png" alt="Lumen six-pane terminal layout" width="300"> |

## Quick Start

### Install

Download the latest Windows build from
[GitHub Releases](https://github.com/jimhy/lumen/releases), then run `lumen.exe`.

Requirements:

- Windows 10 1809+ for ConPTY.
- PowerShell: Lumen prefers `pwsh` and falls back to Windows PowerShell.

### First 3 Minutes

| Action | Shortcut or place |
|---|---|
| Open a new session | `Ctrl+T` |
| Add a pane | `Ctrl+Shift+D` |
| Toggle the file tree | `Ctrl+B` |
| Open settings | `Ctrl+,` |
| Search command history | `Ctrl+R` |
| Add a newline before running | `Shift+Enter` |
| Accept ghost text | `→` or `End` |
| Open a URL or file path from output | `Ctrl+Click` |

## Feature Highlights

### Editor-Style Command Input

Lumen gives commands their own input area instead of forcing you to edit at the prompt.
You get multi-line editing, syntax highlighting, smart continuation handling, history
search, command/file completion, ghost text, draft recovery, Unicode grapheme-aware
cursor movement, and a one-key fallback to classic passthrough mode.

### Command Blocks

Shell integration captures command boundaries through OSC 133. Finished commands show
success/failure state and elapsed time, and block navigation lets you jump through output
without hunting for prompts manually.

### GPU Terminal Core

The renderer uses `wgpu` + `glyphon`, with a custom rectangle pipeline for cell
backgrounds, cursors, and underlines. The terminal core handles ANSI/VT sequences,
alternate screen apps such as `vim`/`less`, bracketed paste, synchronized updates,
10k-line scrollback, CJK IME preedit, true color, and clickable links.

### Project-Aware UI

The app shell includes sessions, panes, a file tree, custom title bar, snap layout
support, persisted layout widths, system toasts, and built-in themes including Lumen,
Tokyo Night, Dracula, Nord, Gruvbox, Solarized, Catppuccin, and One Dark.

### Self-Hosted Remote Control

Run `lumen-server` on your own machine or VPS, enter its address in Lumen settings, then
sign in on two devices. The remote tab shows online devices; double-click to connect,
enter the 9-digit pairing code shown on the controlled device, and start working.

Remote sessions support terminal mirroring/control, remote tabs and panes, a remote file
tree, file upload/download, folder copy via virtual file clipboard, relay fallback, and a
QUIC P2P data path when direct connectivity is available.

Start here:

- [Server overview](server/lumen-server/README.md)
- [Production deployment guide](server/deploy/README.md)

## Build From Source

```powershell
# Clone and enter the repo first, then:
cargo run -p lumen-app

# Release build
cargo build -p lumen-app --release
.\target\release\lumen.exe
```

The modern input editor is enabled by default through the `input-editor` feature.
To build a classic byte-stream terminal:

```powershell
cargo run -p lumen-app --no-default-features
```

To run the self-hosted server locally:

```powershell
cargo run -p lumen-server
```

## Keyboard Shortcuts

| Shortcut | Action |
|---|---|
| `Ctrl+T` | New session |
| `Ctrl+W` | Close current session |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous session |
| `Ctrl+B` | Toggle file tree |
| `Ctrl+,` | Open / close settings |
| `Ctrl+↑` / `Ctrl+↓` | Jump between command blocks |
| `Ctrl+C` | Copy selection or selected block output; interrupt when nothing is selected |
| `Ctrl+V` / `Shift+Insert` | Paste |
| `Shift+PgUp` / `Shift+PgDn` | Scroll up / down |
| `Esc` | Close settings or overlay |
| `Ctrl+Shift+D` | Add pane |
| `Ctrl+Shift+W` | Close pane |
| `Ctrl+Shift+Enter` | Maximize / restore pane |
| `Ctrl+R` | Fuzzy history search |
| `Tab` | Completion |
| `Shift+Enter` | Insert newline |
| `Ctrl+Shift+E` | Toggle classic passthrough mode |
| `Ctrl+Click` | Open terminal link or file path |

## Architecture

```text
crates/
├── lumen-pty/       # PTY abstraction: Windows ConPTY / portable-pty
├── lumen-term/      # VT parser, grid, scrollback, command blocks
├── lumen-editor/    # Pure command-editor state machine
├── lumen-renderer/  # wgpu + glyphon renderer
├── lumen-protocol/  # Remote-control protocol shared by client/server
└── lumen-app/       # winit + egui app shell, sessions, panes, settings, remote UI

server/
└── lumen-server/    # Axum server: auth, devices, sync, WebSocket relay, STUN helper
```

Data flow: PTY bytes -> `lumen-term` -> grid/block model -> `lumen-renderer`.
Keyboard, mouse, IME, file tree, and remote events are routed through `lumen-app`; editor
mode uses `lumen-editor` before sending finalized bytes back to the PTY.

Deep dives:

- [Architecture design](docs/架构设计.md)
- [Input editor design](docs/输入编辑器设计.md)
- [Remote control design](docs/M5远程控制设计.md)
- [P2P direct connection design](docs/M6-P2P直连-QUIC打洞-设计-2026-06-23.md)

## Roadmap

- Local terminal core: ConPTY, ANSI/VT, GPU rendering, blocks
- App shell: custom title bar, panes, file tree, settings, themes, i18n
- Modern editor: multi-line input, highlighting, history, completion, links
- Updates: GitHub Release auto-update and proxy support
- Remote: accounts, devices, pairing, terminal control, file transfer, relay/P2P path
- Next: AI-assisted command generation and error explanation
- Next: broader sync polish and cross-device workflow refinements

## Contributing

Try Lumen on your own workflow, open issues with real terminal scenarios, share short
screen recordings, or help test remote control across different NAT/network setups.
Small, reproducible reports are especially valuable.

## License

[Apache-2.0](LICENSE) © jimhy
