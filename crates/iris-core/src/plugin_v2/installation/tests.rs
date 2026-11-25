use super::*;
use std::io::Cursor;

fn archive(extra: &str) -> Vec<u8> {
    archive_id("test.install", extra)
}
fn archive_id(id: &str, extra: &str) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let json = format!(
        r#"{{"id":"{id}","name":"test","version":"1","protocol":2,"abi":2,"entry":"plugin.wasm"{extra}}}"#
    );
    for (name, bytes) in [("plugin.json", json.as_bytes()), ("plugin.wasm", b"wasm")] {
        writer
            .start_file(
                name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn bundled_digest(bytes: &[u8]) -> [u8; 32] {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    for path in ["plugin.json", "plugin.wasm"] {
        let mut bytes = Vec::new();
        archive
            .by_name(path)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        hash.update(&(path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    hash.finish().as_ref().try_into().unwrap()
}

#[test]
fn bundled_names_require_the_host_index_at_install_and_reload() {
    let root = std::env::temp_dir().join(format!("iris-v2-bundle-{}", random_identity().unwrap()));
    let bytes = archive_id("iris.fixture", "");
    let digest = bundled_digest(&bytes);
    let mut installer = Installer::with_bundled(
        root.clone(),
        Ledger::default(),
        BTreeMap::from([("iris.fixture".into(), digest)]),
    );
    let approve = || Approval {
        digest,
        grants: BTreeSet::new(),
        network: NetworkDeclaration::default(),
        previous: None,
    };
    assert!(
        installer.install(&bytes, approve()).is_err(),
        "user path must not adopt bundled provenance"
    );
    let installed = installer.install_bundled(&bytes, approve()).unwrap();
    assert!(installer.load(&installed).is_ok());
    assert!(
        Installer::new(root.clone(), Ledger::default())
            .load(&installed)
            .is_err()
    );
    let changed = archive_id("iris.fixture", r#", "description":"changed""#);
    assert!(installer.install_bundled(&changed, approve()).is_err());
    assert_eq!(
        installer.record("iris.fixture").unwrap().unwrap(),
        installed
    );
    std::fs::write(root.join("installed/broken.plugin.json"), b"broken").unwrap();
    let entries = installer.entries().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .any(|entry| entry.plugin_id == "broken.plugin" && entry.record.is_err())
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.plugin_id == "iris.fixture" && entry.record.is_ok())
    );
    assert!(
        root.canonicalize()
            .unwrap()
            .starts_with(std::env::temp_dir().canonicalize().unwrap())
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn locked_installation_does_not_replace_existing_record() {
    let root = std::env::temp_dir().join(format!(
        "iris-v2-install-lock-{}",
        random_identity().unwrap()
    ));
    let mut installer = Installer::new(root.clone(), Ledger::default());
    let bytes = archive("");
    let installed = installer.install(&bytes, approval(&bytes)).unwrap();
    let lock = crate::fs_util::lock_file_with_timeout(
        &root.join("install.lock"),
        std::time::Duration::from_millis(10),
    )
    .unwrap();
    assert_eq!(
        installer.install(&bytes, approval(&bytes)),
        Err(ErrorCode::Backpressure)
    );
    assert_eq!(
        installer.record("test.install").unwrap().unwrap(),
        installed
    );
    drop(lock);
    assert!(
        root.canonicalize()
            .unwrap()
            .starts_with(std::env::temp_dir().canonicalize().unwrap())
    );
    std::fs::remove_dir_all(root).unwrap();
}

fn approval(bytes: &[u8]) -> Approval {
    Approval {
        digest: Package::from_zip(bytes, Provenance::User).unwrap().digest,
        grants: BTreeSet::new(),
        network: NetworkDeclaration::default(),
        previous: None,
    }
}

#[test]
fn failed_update_retains_old_record_and_reinstall_has_new_identity() {
    let root = std::env::temp_dir().join(format!("iris-v2-install-{}", random_identity().unwrap()));
    let mut installer = Installer::new(root.clone(), Ledger::default());
    let original = archive("");
    let first = installer.install(&original, approval(&original)).unwrap();
    let changed = archive(r#", "permissions":["net_http"]"#);
    assert!(installer.install(&changed, approval(&original)).is_err());
    assert_eq!(
        installer.record("test.install").unwrap().unwrap().digest,
        first.digest
    );
    let second = installer.install(&original, approval(&original)).unwrap();
    assert_ne!(first.installation_id, second.installation_id);
    let mut approve = approval(&changed);
    approve.previous = Some((second.installation_id.clone(), second.digest));
    let third = installer.install(&changed, approve).unwrap();
    assert_eq!(second.installation_id, third.installation_id);
    assert!(third.grants.is_empty()); // Declaring a capability does not grant it.
    assert!(installer.load(&first).is_err());
    assert!(installer.load(&third).is_ok());
    assert!(
        root.canonicalize()
            .unwrap()
            .starts_with(std::env::temp_dir().canonicalize().unwrap())
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn uninstall_removes_user_and_damaged_records_while_protecting_bundled_plugins() {
    let root =
        std::env::temp_dir().join(format!("iris-v2-uninstall-{}", random_identity().unwrap()));
    let bundled_bytes = archive_id("iris.fixture", "");
    let digest = bundled_digest(&bundled_bytes);
    let mut installer = Installer::with_bundled(
        root.clone(),
        Ledger::default(),
        BTreeMap::from([("iris.fixture".into(), digest)]),
    );
    installer
        .install_bundled(
            &bundled_bytes,
            Approval {
                digest,
                grants: BTreeSet::new(),
                network: NetworkDeclaration::default(),
                previous: None,
            },
        )
        .unwrap();
    let user_bytes = archive_id("community.removable", "");
    let user = installer
        .install(&user_bytes, approval(&user_bytes))
        .unwrap();
    let object_path = root.join("objects").join(&user.object);
    assert!(object_path.exists());

    // Bundled plugins cannot be uninstalled.
    assert_eq!(
        installer.uninstall("iris.fixture", false),
        Err(ErrorCode::Denied)
    );
    assert_eq!(
        installer.uninstall("iris.fixture", true),
        Err(ErrorCode::Denied)
    );
    assert!(installer.record("iris.fixture").unwrap().is_some());

    // Lock contention returns Backpressure without deleting.
    let lock = crate::fs_util::lock_file_with_timeout(
        &root.join("install.lock"),
        std::time::Duration::from_millis(10),
    )
    .unwrap();
    assert_eq!(
        installer.uninstall("community.removable", false),
        Err(ErrorCode::Backpressure)
    );
    drop(lock);

    // Uninstalling with keep_identity=true archives identity while removing active record/object.
    assert_eq!(
        installer.uninstall("community.removable", true).unwrap(),
        Some(user.clone())
    );
    assert!(installer.record("community.removable").unwrap().is_none());
    assert!(!object_path.exists());
    let archived = installer.archived("community.removable").unwrap().unwrap();
    assert_eq!(archived.installation_id, user.installation_id);
    assert_eq!(archived.digest, user.digest);

    // Explicit approval matching the archived identity restores it and clears the marker.
    let mut restore = approval(&user_bytes);
    restore.previous = Some((archived.installation_id.clone(), archived.digest));
    let restored = installer.install(&user_bytes, restore).unwrap();
    assert_eq!(restored.installation_id, user.installation_id);
    assert!(installer.archived("community.removable").unwrap().is_none());

    // Uninstalling with keep_identity=false leaves no archived identity behind.
    assert_eq!(
        installer.uninstall("community.removable", false).unwrap(),
        Some(restored)
    );
    assert!(installer.archived("community.removable").unwrap().is_none());

    // Damaged user records can still be removed cleanly.
    std::fs::write(root.join("installed/broken.user.json"), b"not-json").unwrap();
    assert_eq!(installer.uninstall("broken.user", false).unwrap(), None);
    assert!(!root.join("installed/broken.user.json").exists());

    std::fs::remove_dir_all(root).unwrap();
}
