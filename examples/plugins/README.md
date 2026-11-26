# Plugin v2 examples

- `watch-stats` (`example.watch-stats`): Rust/SDK Wasm plugin with independent per-window playback timers.
- `virtual-audio` (`example.virtual-audio`): Supplies deterministic PCM using a separate `RandomAccess` `StreamStore`.
- `cinema-ui` (`example.cinema-ui`): Declarative theme and chrome preset requiring no Wasm entry.
- `bilibili` (`community.bilibili`): Community Wasm media source, URL resolver, QR/Cookie login, split-track DASH playback, danmaku timed overlays, and WebVTT subtitle artifacts.
- `baidupan` (`community.baidupan`): Community Wasm cloud-drive media source with QR/Cookie/OAuth login, local/remote directory paging, direct PCS/OpenAPI links, `VirtualHls` transcoding, and sibling subtitle attachment.

```text
rustup target add wasm32-unknown-unknown
python -B scripts/build-example-plugins.py
```

Install the generated `target/plugin-v2/examples/*.irisplugin` archives through
Settings → Plugins → Install. Installation displays the requested capabilities.
Copying a source folder into the old plugin directory does not execute it.
Protocol 1 Python/stdio plugins are no longer supported.

Bundled official reference plugins (`crates/iris-plugin-guides` and
`crates/iris-plugin-webdav`) use the exact same public `iris-plugin-sdk` ABI v2
surface.
