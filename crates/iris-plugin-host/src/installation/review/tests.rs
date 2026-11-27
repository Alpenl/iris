use super::*;
use crate::installation::random_identity;
use std::io::{Cursor, Write};
fn package(wasm: &[u8], name: &str) -> Vec<u8> {
    let manifest = serde_json::to_vec(&serde_json::json!({"id":"test.review","name":name,"version":"1","protocol":2,"abi":2,"entry":"plugin.wasm"})).unwrap();
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in [("plugin.json", manifest.as_slice()), ("plugin.wasm", wasm)] {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}
#[test]
fn review_compiles_without_executing_and_approval_uses_the_reviewed_bytes() {
    let root = std::env::temp_dir().join(format!("iris-v2-review-{}", random_identity().unwrap()));
    std::fs::create_dir_all(&root).unwrap();
    let resources = Resources::new().unwrap();
    let mut installer = Installer::new(root.join("packages"), resources.ledger.clone());
    let path = root.join("candidate.irisplugin");
    std::fs::write(&path, package(b"invalid-module", "Invalid")).unwrap();
    assert!(Review::read(&path, &installer, &resources, &CancelToken::new()).is_err());
    assert!(installer.entries().unwrap().is_empty());
    // The start trap proves review never instantiates or executes unapproved code.
    let wasm = wat::parse_str(r#"(module (memory (export "memory") 1) (func $start unreachable) (start $start) (func (export "iris_abi_version") (result i32) i32.const 2) (func (export "iris_alloc") (param i32) (result i32) i32.const 1) (func (export "iris_dealloc") (param i32 i32)) (func (export "iris_on_event") (param i32 i32) (result i64) i64.const 0))"#).unwrap();
    std::fs::write(&path, package(&wasm, "Reviewed")).unwrap();
    let review = Review::read(&path, &installer, &resources, &CancelToken::new()).unwrap();
    let digest = review.digest();
    std::fs::write(&path, package(&wasm, "Replaced after review")).unwrap();
    let (installed, _) = review
        .approve_with_cleanup(&mut installer, BTreeSet::new(), false)
        .unwrap();
    assert_eq!(installed.digest, digest);
    assert_eq!(
        installer.load(&installed).unwrap().manifest.name,
        "Reviewed"
    );
    let review = Review::read(&path, &installer, &resources, &CancelToken::new()).unwrap();
    let other = Review::read(&path, &installer, &resources, &CancelToken::new()).unwrap();
    other
        .approve_with_cleanup(&mut installer, BTreeSet::new(), false)
        .unwrap();
    assert_eq!(
        review.approve_with_cleanup(&mut installer, BTreeSet::new(), true),
        Err(ErrorCode::StaleHandle)
    );
    let mut expired = Review::read(&path, &installer, &resources, &CancelToken::new()).unwrap();
    expired.expires = Deadline::at(std::time::Instant::now() - Duration::from_secs(1));
    assert_eq!(
        expired.approve_with_cleanup(&mut installer, BTreeSet::new(), false),
        Err(ErrorCode::StaleHandle)
    );
    let mut refreshed = Review::read(&path, &installer, &resources, &CancelToken::new()).unwrap();
    refreshed.expires = Deadline::at(std::time::Instant::now() - Duration::from_secs(1));
    refreshed.refresh_expiry();
    assert!(
        refreshed
            .approve_with_cleanup(&mut installer, BTreeSet::new(), false)
            .is_ok()
    );
    assert_eq!(resources.ledger.used(None, Resource::HostBytes), 0);
    assert!(
        root.canonicalize()
            .unwrap()
            .starts_with(std::env::temp_dir().canonicalize().unwrap())
    );
    std::fs::remove_dir_all(root).unwrap();
}
