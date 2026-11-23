use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=MPV_LIB_DIR");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let dir = std::env::var_os("MPV_LIB_DIR").map(PathBuf::from);
    let dir = match target_os.as_str() {
        "windows" => {
            let target = std::env::var("TARGET").expect("Cargo sets TARGET");
            assert_eq!(
                target, "x86_64-pc-windows-msvc",
                "Iris supports Windows x64 MSVC only"
            );
            let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(Path::parent)
                .expect("iris-mpv is inside the workspace's crates directory");
            let dir = dir.unwrap_or_else(|| workspace.join("target/deps/mpv"));
            let import_lib = dir.join("mpv.lib");
            assert!(
                import_lib.is_file(),
                "{} not found; set MPV_LIB_DIR to a Windows x64 MSVC libmpv SDK",
                import_lib.display()
            );
            println!("cargo:rerun-if-changed={}", import_lib.display());
            Some(dir)
        }
        "macos" => dir.or_else(|| {
            let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
            let homebrew = if target_arch == "aarch64" {
                "/opt/homebrew/lib"
            } else {
                "/usr/local/lib"
            };
            Path::new(homebrew)
                .join("libmpv.dylib")
                .is_file()
                .then(|| PathBuf::from(homebrew))
        }),
        _ => dir,
    };
    if let Some(dir) = dir {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
    println!("cargo:rustc-link-lib=dylib=mpv");
}
