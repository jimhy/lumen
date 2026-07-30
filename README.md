<p align="center">
  <img src="icons/lumen-icon-128.png" alt="Lumen logo" width="96">
</p>

<h1 align="center">Lumen</h1>

<p align="center">
  <strong>Your command-line workspace—from local projects to SSH fleets and your own remote devices.</strong>
</p>

<p align="center">
  <strong>English</strong> · <a href="README.zh-CN.md">简体中文</a> ·
  <a href="https://github.com/jimhy/lumen/releases"><strong>Download</strong></a> ·
  <a href="#build-from-source">Build from source</a> ·
  <a href="server/deploy/README.md">Self-host remote access</a>
</p>

<p align="center">
  <a href="https://github.com/jimhy/lumen/releases"><img alt="GitHub release" src="https://img.shields.io/github/v/release/jimhy/lumen?color=7c3aed"></a>
  <a href="https://github.com/jimhy/lumen/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/jimhy/lumen/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Platforms" src="https://img.shields.io/badge/platforms-Windows%20%7C%20Linux%20%7C%20macOS-2563eb">
  <img alt="Rust 1.92+" src="https://img.shields.io/badge/Rust-1.92%2B-f74c00?logo=rust">
  <img alt="Apache 2.0" src="https://img.shields.io/badge/license-Apache--2.0-blue">
</p>

<p align="center">
  <img src="docs/demo.gif" alt="Lumen demo: editor-style commands, project files, command blocks, split panes, and fuzzy history" width="1080">
</p>

Lumen is a native, GPU-accelerated terminal workspace built in Rust. It combines an
editor-style command area, readable command blocks, persistent sessions, project files,
secure SSH/SFTP, server monitoring, and self-hosted device control in one focused app.

No Electron shell. No mandatory hosted account. No need to keep five separate tools open
just to run a command, inspect a file, watch a process, and reach another machine.

> **Project status:** the local terminal, SSH/SFTP workspace, built-in remote text editor,
> server monitor, app lock, self-hosted device control, relay fallback, and QUIC P2P path
> are implemented and actively polished. Embedded browser and AI-assisted workflows are
> roadmap items, not advertised here as finished features.

## Why developers reach for Lumen

| | |
|---|---|
| **Command with confidence**<br>Write multi-line commands with PowerShell highlighting, continuation detection, completion, ghost text, draft recovery, and fuzzy history before anything runs. | **Keep the whole project in view**<br>Use persistent sessions, up to six panes, a searchable file tree, draggable layouts, and command blocks instead of juggling windows. |
| **Operate servers without leaving the terminal**<br>Organize SSH hosts, open independent shells, browse SFTP, edit remote files, inspect metrics, search processes or ports, and stop a runaway process. | **Control devices on infrastructure you own**<br>Self-host `lumen-server`, pair devices with a short code, mirror sessions and panes, transfer files, and use direct QUIC when the network allows it. |

## See Lumen in action

### A project-aware local workspace

<p align="center">
  <img src="docs/media/lumen-workspace.png" alt="Lumen local workspace with project file tree, command blocks, and split panes" width="1080">
</p>

| SSH workspace | Make it yours |
|---|---|
| <img src="docs/media/lumen-ssh.png" alt="Lumen SSH server groups and connection form" width="530"> | <img src="docs/media/lumen-customize.png" alt="Lumen themes, fonts, language, and background settings" width="530"> |
| Group and search servers, choose password/private-key/agent authentication, open multiple shells, browse SFTP, edit files, and monitor the host. | Choose from 11 themes, follow the OS light/dark mode, select a font, add a background image, and switch between English, Simplified Chinese, and Traditional Chinese. |

All product media above was captured from the real English UI. The SSH names and hosts are
isolated demo data; no real credentials or infrastructure appear in the screenshots.

## Install

### Windows

Download `Lumen-Setup-*.exe` from [GitHub Releases](https://github.com/jimhy/lumen/releases)
and run the installer.

- Windows 10 version 1809 or newer is required for ConPTY.
- Lumen prefers PowerShell 7 (`pwsh`) and falls back to Windows PowerShell.
- Windows builds can check, download, and install updates from inside Lumen.

### Linux

Releases include an x86_64 tarball and a `.deb` package. On Debian or Ubuntu:

```bash
sudo apt install ./lumen-app_*_amd64.deb
```

The `.deb` package declares its desktop dependencies. The tarball is useful for portable
or non-Debian setups and requires a working Vulkan or OpenGL stack plus the documented
desktop libraries.

### macOS

The [macOS packaging workflow](https://github.com/jimhy/lumen/actions/workflows/package-macos.yml)
builds a universal `Lumen.app` for Apple Silicon and Intel. The current app bundle is
unsigned, so macOS may require explicit approval on first launch. Building from source is
also supported.

## Your first five minutes

1. Open a project directory in the local terminal; the file tree follows the shell's
   reported working directory.
2. Press `Ctrl+Shift+D` to split the workspace, then keep a build, server, log, or REPL in
   each pane.
3. Press `Ctrl+R` to search command history, or use `↑` / `↓`, completion, and ghost text
   to reuse previous work.
4. Open the **SSH** tab, create a server profile, verify its host-key fingerprint, and get
   a shell, SFTP tree, editor, and monitor in the same window.

Useful shortcuts:

| Action | Shortcut |
|---|---|
| New / close session | `Ctrl+T` / `Ctrl+W` |
| Next / previous session | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| Add / close pane | `Ctrl+Shift+D` / `Ctrl+Shift+W` |
| Maximize / restore pane | `Ctrl+Shift+Enter` |
| Toggle project file tree | `Ctrl+B` |
| Open settings | `Ctrl+,` |
| Search command history | `Ctrl+R` |
| Add a line without running | `Shift+Enter` |
| Jump between command blocks | `Ctrl+↑` / `Ctrl+↓` |
| Toggle classic passthrough mode | `Ctrl+Shift+E` |
| Open a URL or file path | `Ctrl+Click` |

## Everything included

### Modern terminal and command workflow

| Capability | What you get |
|---|---|
| Editor-style input | A dedicated multi-line command area with selection, cut/copy/paste, Unicode grapheme-aware cursor movement, undo/redo, draft recovery, and smart submit behavior. |
| PowerShell intelligence | Syntax highlighting, quote/pipe continuation detection, command and path completion, history navigation, ghost text, and `Ctrl+R` fuzzy search. |
| Command blocks | OSC 133/633-aware command boundaries, success/failure state, elapsed time, block selection, block output copy, and keyboard navigation. |
| Classic compatibility | One shortcut switches back to byte-for-byte passthrough for shells, REPLs, TUIs, or workflows that want traditional terminal behavior. |
| Terminal core | ANSI/VT, true color, alternate-screen apps such as Vim and `less`, bracketed paste, synchronized updates, mouse protocols, 10k-line scrollback, draggable scrollbar, and clickable URLs/file paths. |
| International input | CJK IME pre-edit, emoji/grapheme-aware editing, cross-platform font fallback, and English/Simplified Chinese/Traditional Chinese UI. |
| GPU rendering | `wgpu` + `glyphon`, with custom rendering for cell backgrounds, cursors, selections, and decorations. |

### Sessions, panes, and project files

| Capability | What you get |
|---|---|
| Persistent sessions | Session names, working directories, pane topology, pane ratios, and active state are restored across restarts. |
| Flexible panes | Up to six panes per session, draggable dividers, maximize/restore, reset layout, pane reordering, and custom pane names. |
| Project file tree | Recursive search, hidden-item toggle, directory-level refresh, create/rename, move to trash, reveal in the OS file manager, and copy absolute or relative paths. |
| Native file workflows | Copy/paste files and folders with the operating-system clipboard; drag an external file into the terminal to insert its path. |
| Desktop integration | Custom title bar, Windows 11 Snap Layouts, per-session process icons, system notifications, single-instance handoff, and remembered sidebar widths/visibility. |
| Personalization | 11 bundled light/dark themes, OS theme sync, custom terminal font and size, background images with opacity/dim controls, and persistent layout preferences. |
| Updates and networking | In-app update checks, Windows installer handoff, skip-version support, and optional HTTP/HTTPS/SOCKS5 proxy settings. |

### SSH, SFTP, remote editing, and server operations

| Capability | What you get |
|---|---|
| Server inventory | Searchable profiles, custom groups, stable ordering, initial directory, connection timeout, optional keepalive, reconnect, and account-scoped metadata sync. |
| Authentication | Password, private key with optional passphrase, and SSH Agent. Secrets and private-key paths stay on the current device and are never synced. |
| Host-key safety | First-use fingerprint confirmation and fail-closed blocking when a saved host key changes. |
| Independent shells | Multiple simultaneous shell sessions per SSH host, with their own terminal state and reconnect lifecycle. |
| SFTP workspace | Remote file tree, hidden files, create/rename/delete, upload/download, local↔remote clipboard workflows, and OS file-manager paste integration. |
| Built-in text editor | UTF-8 remote editing with syntax highlighting for common languages, local completion, snippets, find/replace, go-to-line, soft wrap, comments, undo/redo, save-conflict detection, and safe reload/overwrite choices. |
| Live server dashboard | CPU, per-core usage, memory, load averages, filesystem and disk I/O, network throughput/traffic, uptime, OS/kernel/timezone, and collapsible cards. |
| Process and port tools | Auto-refreshing process list, CPU/memory sorting, name search, `:port` lookup, process details, and confirmed terminate/force-kill actions. |

The editor recognizes JSON, YAML, TOML, Rust, Python, JavaScript/TypeScript, shell,
PowerShell, Go, Java/Kotlin, C/C++/C#, HTML/Vue/Svelte, XML/SVG, CSS, SQL, Markdown, and
Dockerfiles.

### Self-hosted remote device control

Run `lumen-server` on your own machine or VPS, point each Lumen client at it, and sign in
with the same account.

| Capability | What you get |
|---|---|
| Device presence | Online/offline device list, stable device identity, and explicit device removal. |
| Consent and pairing | Incoming control approval plus a 9-digit pairing code shown on the controlled device. |
| Full terminal workspace | Mirror and control remote terminals, sessions, pane layouts, titles, working directories, scrollback, links, and process icons. |
| Remote files | Browse the remote file tree, create/rename/delete, upload/download, copy folders recursively, and paste fetched files into the OS file manager. |
| Network resilience | Authenticated WebSocket signaling/relay, direct QUIC data path with mutual TLS and pinned fingerprints when possible, automatic relay fallback, reconnect, and session restore. |
| Your infrastructure | The Axum server, PostgreSQL storage, TLS reverse-proxy example, systemd unit, and deployment guide are included in this repository. |

Start here:

- [Server overview](server/lumen-server/README.md)
- [Production deployment guide](server/deploy/README.md)
- [Remote control design](docs/M5远程控制设计.md)
- [QUIC P2P design](docs/M6-P2P直连-QUIC打洞-设计-2026-06-23.md)

## Security model

- SSH passwords and private-key passphrases are stored locally; Windows uses Credential
  Manager and Unix builds use an encrypted local credential file. Credential material is
  excluded from account sync.
- SSH host-key changes are blocked until the user explicitly verifies and updates trust.
- App lock supports manual lock, lock on startup, lock after resume, and idle timeout.
  Password verification uses Argon2id and sensitive UI buffers are cleared after use.
- Remote control requires authenticated devices, explicit consent, and a short-lived
  pairing code. Direct QUIC peers authenticate with fingerprints exchanged over the
  authenticated signaling channel.
- The remote service is self-hostable. Put it behind TLS before exposing it to the public
  internet and follow the production guide's secret/database requirements.

## Build from source

Prerequisites:

- Rust 1.92 or newer for the desktop workspace.
- Platform build dependencies for `winit`, `wgpu`, and the native file dialog backend.
- On Windows, Visual Studio Build Tools with the MSVC toolchain.

```powershell
# Run the desktop app with the modern input editor
cargo run -p lumen-app

# Build an optimized desktop binary
cargo build -p lumen-app --release

# Build the classic byte-stream variant
cargo run -p lumen-app --no-default-features

# Run the self-hosted service locally
cargo run -p lumen-server
```

### Repository map

```text
crates/
├── lumen-pty/       # ConPTY / portable PTY abstraction
├── lumen-term/      # VT parser, grid, scrollback, links, command blocks
├── lumen-editor/    # Pure command-editor state machine
├── lumen-renderer/  # wgpu + glyphon terminal renderer
├── lumen-ssh/       # SSH transport, SFTP, metrics, process/port management
├── lumen-protocol/  # Remote-control and sync protocol
└── lumen-app/       # winit + egui desktop shell

server/
└── lumen-server/    # Auth, devices, SSH metadata sync, relay, STUN helper
```

More technical detail:

- [Architecture](docs/架构设计.md)
- [Modern input editor](docs/输入编辑器设计.md)
- [SSH mode product/design notes](docs/SSH模式-PRD-2026-07-23.md)

## Roadmap

- AI-assisted command generation and error explanation
- Embedded browser with an automation bridge
- More cross-device sync and remote-network edge-case polish
- Signed/notarized macOS distribution

## Contributing

Real workflows are the best test cases. Open an issue with a reproducible terminal
scenario, SSH/SFTP edge case, network topology, short recording, or focused proposal.
Pull requests and cross-platform testing are welcome.

## License

[Apache-2.0](LICENSE) © jimhy
