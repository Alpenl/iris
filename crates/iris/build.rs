fn windows_resource_compiler() -> std::ffi::OsString {
    if let Some(rc) = std::env::var_os("RC") {
        return rc;
    }
    // Developer shells already put their selected SDK on PATH.
    if std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join("rc.exe").is_file())
    }) {
        return "rc.exe".into();
    }
    // Cargo discovers MSVC in a regular Windows shell. Find its standard SDK here too.
    if let Some(programs) = std::env::var_os("ProgramFiles(x86)") {
        let bin = std::path::PathBuf::from(programs).join("Windows Kits/10/bin");
        if let Ok(entries) = std::fs::read_dir(bin)
            && let Some(rc) = entries
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("10."))
                .map(|entry| entry.path().join("x64").join("rc.exe"))
                .filter(|rc| rc.is_file())
                .max()
        {
            return rc.into_os_string();
        }
    }
    "rc.exe".into()
}

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    let patch = version
        .strip_prefix("0.0.")
        .and_then(|patch| patch.parse::<u16>().ok())
        .expect("Only 0.0.x Iris builds are authorized");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let target = std::env::var("TARGET").expect("Cargo sets TARGET");
        assert_eq!(
            target, "x86_64-pc-windows-msvc",
            "Iris supports Windows x64 MSVC only"
        );
        let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
        let manifest = out.join("app.manifest");
        let contents = include_str!("app.manifest").replace("@VERSION@", version);
        std::fs::write(&manifest, contents).expect("write Windows application manifest");
        std::fs::write(out.join("app.ico"), include_bytes!("app.ico")).expect("write Windows icon");
        let resource = format!(
            r#"1 ICON "app.ico"
1 VERSIONINFO
FILEVERSION 0,0,{patch},0
PRODUCTVERSION 0,0,{patch},0
FILEOS 0x40004
FILETYPE 1
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName", "Alpenl\0"
            VALUE "FileDescription", "Iris media player\0"
            VALUE "FileVersion", "{version}\0"
            VALUE "OriginalFilename", "iris.exe\0"
            VALUE "ProductName", "Iris\0"
            VALUE "ProductVersion", "{version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
        );
        std::fs::write(out.join("app.rc"), resource).expect("write Windows resource script");
        let rc = windows_resource_compiler();
        let result = std::process::Command::new(rc)
            .current_dir(&out)
            .args(["/nologo", "/fo", "app.res", "app.rc"])
            .status()
            .expect("run Windows SDK rc.exe; use a VS Developer shell or set RC");
        assert!(
            result.success(),
            "Windows resource compiler failed: {result}"
        );
        println!("cargo:rerun-if-env-changed=RC");
        println!("cargo:rerun-if-changed=app.manifest");
        println!("cargo:rerun-if-changed=app.ico");
        println!("cargo:rustc-link-arg-bin=iris=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=iris=/MANIFESTINPUT:{}",
            manifest.display()
        );
        println!(
            "cargo:rustc-link-arg-bin=iris={}",
            out.join("app.res").display()
        );
    }
}
