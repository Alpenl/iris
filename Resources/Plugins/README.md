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
compilation. `--verify-embedded` rebuilds into `target/` and compares distribution
hashes with this directory. Run native host tests with the required mpv SDK.
Do not edit the generated archives or hashes by hand.

`iris --check-plugin-runtime` compiles both embedded guests, verifies an in-memory
cache hit, instantiates them and executes a callback in the actual host process.
It emits JSON timings and checks that Store/handle accounting returns to zero.
`scripts/benchmark-plugin-v2.py` records repeated cold-process/hot-cache samples,
host identity and sampled peak RSS. These are JIT measurements, not 4K/UI results.

The locked Wasmtime 51 Unix allocator uses mmap/mprotect without MAP_JIT.
`Resources/Iris.entitlements` therefore grants only
`com.apple.security.cs.allow-unsigned-executable-memory`; it does not disable
library validation. See [Apple's entitlement reference](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.cs.allow-unsigned-executable-memory).
Review this exception when upgrading the allocator. The release packager checks
the signed entitlement dictionary and runs the actual staged host after signing.
macOS Intel/ARM results require those hosts; Windows tests cannot establish them.
