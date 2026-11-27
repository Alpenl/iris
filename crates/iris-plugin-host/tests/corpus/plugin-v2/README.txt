Seeded host-parser mutation regression. Run cargo test --locked -p iris-plugin-host --lib tests::fuzz:: -- --nocapture.
IRIS_FUZZ_SEED is a nonzero u64; IRIS_FUZZ_ITERATIONS defaults to 2000 (maximum 20000000).
Failed bytes are saved to the system temporary directory with seed and iteration in the filename.
Preserve any minimized failure here before changing the parser. Do not include credentials or user media.
This bounded mutator is not coverage-guided, and does not fuzz native decoders or Wasmtime itself.
