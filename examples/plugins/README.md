# Plugin v2 examples

`watch-stats` is a Rust/SDK Wasm plugin with independent window timers.
`virtual-audio` supplies deterministic PCM using a separate RandomAccess StreamStore.
`cinema-ui` is a declarative theme and needs no Wasm entry.

```text
rustup target add wasm32-unknown-unknown
python -B scripts/build-example-plugins.py
```

Install the generated `target/plugin-v2/examples/*.irisplugin` archives through
Settings → Plugins → Install. Installation displays the requested capabilities.
Copying a source folder into the old plugin directory does not execute it.
Protocol 1 Python/stdio plugins are no longer supported.

For asynchronous state, secrets, login confirmation, directory paging and remote
playback, see `crates/iris-plugin-guides` and `crates/iris-plugin-webdav`, which use
the same SDK. These examples do not claim to implement real cloud-provider login.
