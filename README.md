<div align="center">

# Iris

**A fast, cross-platform photo, video and audio player built on mpv.**

[![Checks](https://github.com/Alpenl/iris/actions/workflows/check.yml/badge.svg?branch=main)](https://github.com/Alpenl/iris/actions/workflows/check.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.100](https://img.shields.io/badge/rust-1.100-orange.svg)](rust-toolchain.toml)
![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Windows-lightgrey.svg)

**English** · [简体中文](README.zh-CN.md)

</div>

Iris is a desktop media player for macOS and Windows. Playback is handled by [libmpv](https://mpv.io), the UI is written in Rust with [GPUI](https://www.gpui.rs), and decoding and color management stay in a native video layer. A sandboxed WebAssembly plugin system adds remote storage, UI customization and viewport overlays without touching the core.

> [!WARNING]
> Iris is in early development (**v0.0.2**). Expect breaking changes. No signed or notarized release builds are published yet.

## Table of contents

- [Features](#features)
- [Platform support](#platform-support)
- [Building from source](#building-from-source)
- [Usage](#usage)
- [Plugins](#plugins)
- [Data and privacy](#data-and-privacy)
- [Project layout](#project-layout)
- [Documentation](#documentation)
- [Contributing](#contributing)
- [License](#license)

## Features

### Playback

- Hardware decoding through libmpv (VideoToolbox on macOS, D3D11VA on Windows), with a software fallback.
- Precise and coarse seeking, frame stepping, chapters (MKV/MP4), 0.25×–4.00× speed and end-of-file policies.
- Resume position for local and remote media alike, keyed by a single `MediaUri`.
- Network streams with buffered-range display, live mode, and quality and mirror switching.
- Aspect ratio and crop-to-fill, temporary rotation and mirroring, and a picture equalizer (brightness, contrast, saturation, gamma).
- Live audio output device switching, frame screenshots and sleep prevention during playback.
- Integration with macOS Now Playing and Windows System Media Transport Controls.

### Photos

- Wide-gamut photo viewing with embedded ICC profiles, EXIF metadata and HEIC orientation.
- Cursor-anchored zoom (scroll wheel or trackpad pinch), panning, fit-to-window, 1:1 and a thumbnail strip.
- Remote photos load lazily: Iris reads the first 256 KiB, then uses HTTP Range requests for trailing metadata. Each photo is capped at 16 requests and 5 MiB in total.

### Library and remote sources

- One sidebar with four views: **Catalog**, **Sources**, **Queue** and **Plugins**.
- The play queue is separate from browsing, so you can queue items across folders and remote sources.
- Location bookmarks, plus a fast scanner for slow SMB/UNC shares that avoids `stat` storms.
- Built-in WebDAV source (`iris.webdav`): multiple servers, no OS mount needed, external subtitles, and credentials kept in the system keychain.
- HTTP(S) streams go through a host-side session forwarder that sends credentials only to the source they belong to.

### Customization

- **Workspace presets.** Five built-in presets (Default, Cinema, Review, Listening, Mini Player) and user-defined ones. Switching presets never overwrites your personal baseline in `settings.conf`, and each switch reverts the previous preset's changes.
- **Keymaps.** Three conflict-free built-in schemes (Iris, mpv-style, PotPlayer-style), plus per-action key recording. Single-key shortcuts are suppressed while you type in text fields.
- **Themes.** Four built-in palettes and UI scaling from 85% to 135%.

## Platform support

| Platform | Architecture | Video output | CI |
|---|---|---|---|
| macOS | Apple Silicon, Intel | `CAOpenGLLayer` | `macos-15`, `macos-15-intel` |
| Windows | x86_64 (MSVC) | D3D11 + DirectComposition | `windows-2025` |

Linux is not supported at this time.

## Building from source

### Prerequisites

- Rust **1.100.0**, pinned in [`rust-toolchain.toml`](rust-toolchain.toml). `rustup` installs it automatically.
- libmpv:
  - **macOS:** `brew install mpv` (for development only, see the note below).
  - **Windows (x64 MSVC):** set `MPV_LIB_DIR`, or place a development `mpv.lib` in `target/deps/mpv/`. The directory that holds the mpv DLL must be on `PATH`.

### Build and run

```bash
cargo build --release
```

```bash
./target/release/iris path/to/media
```

> [!NOTE]
> Builds linked against Homebrew's libmpv are for local development only and must not be redistributed. Release builds link a source-built LGPL media SDK. See [`.github/workflows/release-build.yml`](.github/workflows/release-build.yml) and the [licensing decision](docs/DECISIONS.md).

## Usage

```text
iris [PATH]          Open a file or folder
iris -- -clip.mp4    Open a path that starts with "-"
iris --help          Show usage
iris --version       Show version
```

### Default keyboard shortcuts

The list below is the default Iris scheme. You can switch to the mpv or PotPlayer scheme, or rebind any action, under **Settings → Shortcuts**.

<details>
<summary>Show all shortcuts</summary>

| Key | Action |
|---|---|
| `⌘P` / `Ctrl+P` | Command palette (actions, presets, themes, sources, plugin commands) |
| `⌘O` / `Ctrl+O` | Open local file or folder |
| `⌘U` / `Ctrl+U` | Open network stream |
| `⌘F` / `Ctrl+F` | Search the current catalog |
| `⌘R` / `Ctrl+R` | Refresh the current catalog or remote source |
| `⌘,` / `Ctrl+,` | Settings |
| `Alt+W` | Cycle workspace presets |
| `Tab` | Toggle sidebar |
| `F` / double-click | Toggle fullscreen |
| `T` | Toggle always on top |
| `I` / `⌘I` | Media info, decoder stats and EXIF |
| `S` | Favorite / unfavorite |
| `Space` | Play / pause (video and audio); toggle 1:1 (photos) |
| `←` / `→` | Seek, or `Shift` for 6× larger steps (video); previous / next (photos) |
| `,` / `.` | Step one frame back / forward |
| `↑` / `↓` / `M` | Volume up / down / mute |
| `[` / `]` / `Backspace` | Speed down / up / reset to 1.00× |
| `V` / `J` / `Z` / `X` | Subtitle visibility / track / delay adjustment |
| `Shift+,` / `Shift+.` | Previous / next subtitle line (keeps the paused state) |
| `A` / `C` / `R` / `H` | Audio track / aspect ratio / rotate 90° / mirror |
| `G` | Photo thumbnail strip |
| `0` | Fit photo to window |
| `⌘S` / `Ctrl+S` | Save screenshot of the current frame |
| `⌘⌥⇧R` / `Ctrl+Alt+Shift+R` | Restore default appearance (cannot be rebound) |
| `Esc` | Close the topmost layer (recorder, dialog, menu, palette, settings, overlay, search), then exit fullscreen |

</details>

## Plugins

Plugins are WebAssembly modules that run in a [Wasmtime](https://wasmtime.dev) sandbox against the headless `iris-api` protocol (`protocol: 2`, `ABI: 2`). Within the permissions a user grants, a plugin can:

- Add multi-instance data sources and remote search.
- Restyle the UI (colors, corner radius, bar heights) or hide the title bar for a cinema-style window. The host always keeps recovery paths available.
- Mount declarative widgets in the sidebar or in dialogs.
- Draw interactive vector HUDs over the viewport. There are 7 overlay node types.
- Write structured logs, which the host rate-limits and redacts.

Official plugins: `iris.webdav` (WebDAV source) and `iris.guides` (composition guides). The [`examples/plugins/`](examples/plugins) directory has more examples, including a declarative theme with no Wasm code, watch-time stats, a virtual audio stream, and Bilibili and Baidu Netdisk sources.

Start with the [Plugin Development Guide](docs/PLUGINS.md).

## Data and privacy

Iris never writes to your local or remote media. All personal data lives in the platform's application data directory:

| | Data | Credentials | Log |
|---|---|---|---|
| macOS | `~/Library/Application Support/Iris/` | Keychain | `~/Library/Logs/Iris/iris.log` |
| Windows | `%APPDATA%\Iris\` | Credential Manager | `%LOCALAPPDATA%\Iris\iris.log` |

<details>
<summary>Files in the data directory</summary>

| Path | Contents |
|---|---|
| `settings.conf` | Personal baseline preferences and the active preset ID |
| `presets.json` | User-defined workspace presets |
| `keymap.json` | Keymap scheme and custom bindings |
| `locations.json` | Location bookmarks |
| `records.tsv`, `recent.tsv` | Favorites, resume positions and recent items. HTTP(S) URLs that carry user info, a query or a fragment are kept for the current session only. |
| `credential_index.json` | Scopes and key names of stored credentials. Secrets themselves stay in the system credential store. |
| `plugins-v2/` | Approved Wasm packages and install grants (override with `IRIS_PLUGINS_V2` during development) |
| `plugin-v2-state/` | Non-sensitive plugin configuration, per install identity |
| `plugin-v2-temp/` | Quota-limited temporary plugin artifacts |

</details>

## Project layout

```text
crates/
├── iris/                  GPUI desktop application: UI, native video compositing, platform integration
├── iris-plugin-host/      Wasm plugin host (Wasmtime): plugin lifecycle, capability grants,
│                          bounded network and file access, stream relay, media enhancements
├── iris-core/             Headless core (no GPUI): player state machine, catalog and play queue,
│                          presets, keymap, photo parsing and color matching, credential vault, logging
├── iris-net/              Network and concurrency base: HTTP(S) client, DNS/DoH, loopback HTTP server,
│                          WebSocket/TCP framing, managed threads and cancellation
├── iris-mpv/              Hand-written libmpv FFI (client API, mpv_node, render handle)
├── iris-api/              Headless plugin protocol (serde only)
├── iris-plugin-sdk/       Guest-side SDK for writing Wasm plugins
├── iris-plugin-webdav/    Official WebDAV source plugin (iris.webdav)
└── iris-plugin-guides/    Official composition guides plugin (iris.guides)
examples/plugins/          Example plugins
docs/                      Architecture, decisions and plugin specifications
scripts/                   Project checks and build tooling
```

Dependencies point one way: `iris` → `iris-plugin-host` → `iris-core` → `iris-net`, plus `iris-core` → `iris-mpv`. Only `iris` uses GPUI, and no host crate depends on the guest SDK. `scripts/check-project.py` checks the dependency graph in CI. See [ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full rules.

## Documentation

> [!NOTE]
> The design documents are currently written in Chinese.

| Document | Description |
|---|---|
| [Architecture](docs/ARCHITECTURE.md) | Crate boundaries, rendering stack, data flow |
| [Plugin Development Guide](docs/PLUGINS.md) | Writing and packaging plugins |
| [Plugin V2 Architecture](docs/PLUGIN_V2_ARCHITECTURE.md) | Full plugin contract, quotas and acceptance criteria |
| [Plugin V2 Decisions](docs/PLUGIN_V2_DECISIONS.md) | Design decisions behind the plugin system |
| [Decisions & Roadmap](docs/DECISIONS.md) | Technical direction, licensing and open questions |
| [Discussions](docs/DISCUSSIONS.md) | Record of design discussions |
| [Testing](docs/TESTING.md) | Test strategy and manual verification records |

## Contributing

Issues and pull requests are welcome. Before you submit a change, run the same checks as CI (the project scripts need Python 3.11 or later):

```bash
python3 scripts/check-project.py
```

```bash
cargo fmt --all -- --check
```

```bash
cargo clippy --locked --workspace --all-targets -- -D warnings
```

```bash
cargo test --locked --workspace
```

```bash
python3 scripts/run-plugin-v2-ci.py
```

`check-project.py` enforces architecture boundaries, module size limits, the dependency allowlist and icon and plugin manifest integrity.

## License

The Iris application is dual-licensed under either of the following, at your option:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

Distributed builds dynamically link source-built LGPL media libraries. The corresponding source, build configuration and third-party licenses ship with each artifact. See [Decisions § Licensing](docs/DECISIONS.md).

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this project, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.
