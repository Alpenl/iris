use std::path::Path;

// Links libmpv.
// macOS: Homebrew's libmpv on Apple Silicon or Intel (override with MPV_LIB_DIR).
// Windows: libmpv-2.dll + mpv.lib from a mpv-dev build (set MPV_LIB_DIR).
fn main() {
    println!("cargo:rerun-if-env-changed=MPV_LIB_DIR");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let dir = std::env::var("MPV_LIB_DIR")
        .ok()
        .or_else(|| match target_os.as_str() {
            "macos" => ["/opt/homebrew/lib", "/usr/local/lib"]
                .into_iter()
                .find(|p| Path::new(p).exists())
                .map(str::to_owned),
            _ => None,
        });
    if let Some(dir) = dir {
        println!("cargo:rustc-link-search=native={dir}");
    }
    println!("cargo:rustc-link-lib=dylib=mpv");
}
