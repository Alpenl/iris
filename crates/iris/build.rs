fn main() {
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        version
            .strip_prefix("0.0.")
            .is_some_and(|patch| patch.parse::<u16>().is_ok()),
        "Only 0.0.x Iris builds are authorized"
    );
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // GPUI needs Common Controls v6 and physical per-monitor coordinates.
        let manifest =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("app.manifest");
        let contents = include_str!("app.manifest").replace("@VERSION@", version);
        std::fs::write(&manifest, contents).expect("write Windows application manifest");
        println!("cargo:rerun-if-changed=app.manifest");
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    }
}
