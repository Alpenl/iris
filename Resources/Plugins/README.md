# Official Wasm distribution

These two `.irisplugin` archives and `index.json` are generated from
`crates/iris-plugin-guides` and `crates/iris-plugin-webdav` through the public SDK.
They are embedded into the host executable. The host trusts the compiled-in
content digests; a user-writable manifest or index cannot claim an `iris.*` identity.

Rebuild after changes to the official sources, API, SDK or Rust toolchain:

```text
rustup target add wasm32-unknown-unknown
python -B scripts/build-official-plugins.py --output Resources/Plugins
python -B scripts/build-official-plugins.py --check --output Resources/Plugins
python -B scripts/build-official-plugins.py --verify-embedded
python -B scripts/test-plugin-v2-official.py
```

`--check` validates archive/content hashes, manifests and the source stamp without
compilation. `--verify-embedded` rebuilds into `target/` and enforces byte-for-byte
equality with this directory on all platforms (Windows, macOS, and Linux). Run native
host tests with the required mpv SDK. Do not edit the generated archives or hashes by hand.

`scripts/build-official-plugins.py` uses `x86_64-unknown-linux-gnu` (`Rust 1.100.0`) as
the single canonical build host: on Linux `x86_64` it builds natively, on macOS and
Windows it automatically delegates to the SHA-256-pinned `linux/amd64` container
(`rust:1.100.0-slim-bookworm@sha256:5d3170f8660a07782d5ff42abc55601d2df51d49e5397a8f22247341a658e128`)
via Docker/Podman (or WSL2 `x86_64` on Windows), and normalizes workspace, Cargo
registry, and `rust-src` paths to POSIX prefixes (`/cargo/deps/...`, `/rustc/<commit>/...`).
CI rebuilds inside the pinned `linux/amd64` container and enforces byte-for-byte
`--verify-embedded` archive equality across Linux, macOS, and Windows.

`iris --check-plugin-runtime` compiles both embedded guests to `pulley64` bytecode,
verifies an in-memory cache hit, instantiates them and executes a callback in the
actual host process. It emits JSON timings and checks that Store/handle accounting
returns to zero. `scripts/benchmark-plugin-v2.py` records repeated cold-process/hot-cache
samples, host identity and sampled peak RSS.

Wasmtime compiles plugin modules to `pulley64` interpreter bytecode stored in read-only
memory (`PROT_READ` / `PAGE_READONLY`), never allocating executable pages (`PROT_EXEC`
or `MAP_JIT`). `Resources/Iris.entitlements` is therefore an empty `<dict/>` with zero
JIT or unsigned-executable-memory exceptions. The release packager verifies that signed
entitlements match `<dict/>` and runs the staged host binary after signing.
