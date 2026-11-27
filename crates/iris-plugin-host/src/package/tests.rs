use super::*;
use std::io::Write;
use zip::write::SimpleFileOptions;

fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        zip.start_file(
            *name,
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

const MANIFEST: &[u8] = br#"{"id":"test.example","name":"test","version":"1","protocol":2,"abi":2,"entry":"plugin.wasm"}"#;

#[test]
fn validates_whole_package_and_rejects_ambiguous_paths() {
    let bytes = archive(&[("plugin.json", MANIFEST), ("plugin.wasm", b"wasm")]);
    assert!(Package::from_zip(&bytes, Provenance::User).is_ok());
    for name in [
        "../evil",
        "/absolute",
        "C:/evil",
        "assets/../evil",
        "assets\\evil",
        "assets/CON.txt",
        "assets/foo.",
    ] {
        assert!(validate_path(name).is_err(), "{name}");
    }
    for name in ["PLUGIN.JSON", "assets/../plugin.json"] {
        let bytes = archive(&[
            ("plugin.json", MANIFEST),
            ("plugin.wasm", b"x"),
            (name, b"x"),
        ]);
        assert!(Package::from_zip(&bytes, Provenance::User).is_err());
    }
    let bytes = archive(&[
        ("plugin.json", MANIFEST),
        ("plugin.wasm", b"x"),
        ("assets/a", b"x"),
        ("assets/a/b", b"x"),
    ]);
    assert!(Package::from_zip(&bytes, Provenance::User).is_err());
}

#[test]
fn reserved_identity_requires_the_host_digest() {
    let official = String::from_utf8(MANIFEST.to_vec())
        .unwrap()
        .replace("test.example", "iris.guides");
    let bytes = archive(&[("plugin.json", official.as_bytes()), ("plugin.wasm", b"x")]);
    assert!(matches!(
        Package::from_zip(&bytes, Provenance::User),
        Err(ErrorCode::Denied)
    ));
    assert!(Package::from_zip(&bytes, Provenance::Bundled(&BTreeMap::new())).is_err());
}

#[test]
fn rejects_size_bombs_and_duplicate_manifest_keys_before_execution() {
    let bad = br#"{"id":"first","id":"second"}"#;
    let bytes = archive(&[("plugin.json", bad), ("plugin.wasm", b"x")]);
    assert!(Package::from_zip(&bytes, Provenance::User).is_err());
    let bytes = archive(&[
        ("plugin.json", &vec![b' '; MESSAGE_LIMIT + 1]),
        ("plugin.wasm", b"x"),
    ]);
    assert!(matches!(
        Package::from_zip(&bytes, Provenance::User),
        Err(ErrorCode::QuotaExceeded)
    ));
}
