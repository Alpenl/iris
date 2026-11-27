use super::*;
use crate::ledger::Resource;
use crate::runtime::Resources;
use crate::storage::Storage;
use iris_api::v2::Capability;
use iris_base::net::http_client::HttpCancel;
use iris_core::vault::CredentialVault;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{Cursor, Read, Write},
    sync::Arc,
};

impl super::Bootstrap {
    pub(crate) fn entries(
        &self,
        installer: Installer,
        setup: &Setup,
    ) -> Result<Vec<InstallationEntry>, ErrorCode> {
        self.scan(installer, setup, &CancelToken::new())
            .map(|scan| scan.entries)
    }

    pub(crate) fn custom(
        legacy_root: PathBuf,
        packages: Vec<(String, String, Vec<u8>)>,
    ) -> Result<Self, ErrorCode> {
        let mut pins = BTreeMap::new();
        let mut entries = BTreeMap::new();
        for (id, version, bytes) in packages {
            let sha256: [u8; 32] = ring::digest::digest(&ring::digest::SHA256, &bytes)
                .as_ref()
                .try_into()
                .map_err(|_| ErrorCode::Internal)?;
            let mut archive = zip::ZipArchive::new(Cursor::new(&bytes))
                .map_err(|_| ErrorCode::InvalidArgument)?;
            let mut names = Vec::new();
            for i in 0..archive.len() {
                let f = archive
                    .by_index(i)
                    .map_err(|_| ErrorCode::InvalidArgument)?;
                names.push(
                    f.name()
                        .map_err(|_| ErrorCode::InvalidArgument)?
                        .into_owned(),
                );
            }
            names.sort();
            let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
            for name in names {
                let mut buf = Vec::new();
                archive
                    .by_name(&name)
                    .map_err(|_| ErrorCode::InvalidArgument)?
                    .read_to_end(&mut buf)
                    .map_err(|_| ErrorCode::IoError)?;
                hash.update(&(name.len() as u64).to_le_bytes());
                hash.update(name.as_bytes());
                hash.update(&(buf.len() as u64).to_le_bytes());
                hash.update(&buf);
            }
            let content_sha256: [u8; 32] = hash
                .finish()
                .as_ref()
                .try_into()
                .map_err(|_| ErrorCode::Internal)?;
            let single = BTreeMap::from([(id.clone(), content_sha256)]);
            Package::from_zip(&bytes, Provenance::Bundled(&single))?;
            pins.insert(id.clone(), content_sha256);
            entries.insert(
                id,
                Pin {
                    sha256,
                    content_sha256,
                    version,
                    bytes: Arc::from(bytes),
                },
            );
        }
        Ok(Self {
            legacy_root,
            pins,
            entries,
        })
    }
}

fn official_package(id: &str, version: &str, permissions: &[Capability]) -> Vec<u8> {
    let wasm = wat::parse_str(
        r#"(module
        (memory (export "memory") 1 4)
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 1024)
        (func (export "iris_dealloc") (param i32 i32))
        (func (export "iris_on_event") (param i32 i32) (result i64) i64.const 0))"#,
    )
    .unwrap();
    let manifest = json!({
        "id": id,
        "name": "Official Test",
        "version": version,
        "protocol": 2,
        "abi": 2,
        "entry": "plugin.wasm",
        "permissions": permissions,
    });
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in [
        ("plugin.json", serde_json::to_vec(&manifest).unwrap()),
        ("plugin.wasm", wasm),
    ] {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

struct Fixture {
    root: PathBuf,
    setup: Setup,
    bootstrap: Bootstrap,
    installer: Installer,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "iris-official-migrate-{}-{}",
            std::process::id(),
            super::super::instance::next_id().unwrap()
        ));
        let setup = Setup {
            resources: Resources::new().unwrap(),
            state_directory: root.join("state"),
            temporary_directory: root.join("temp"),
            vault: CredentialVault::in_memory(),
            wake: Arc::new(|| {}),
        };
        let bootstrap = Bootstrap::embedded(root.clone()).unwrap();
        let installer = Installer::with_bundled(
            root.join("packages"),
            setup.resources.ledger.clone(),
            bootstrap.pins(),
        );
        std::fs::create_dir_all(root.join("plugin_state")).unwrap();
        Self {
            root,
            setup,
            bootstrap,
            installer,
        }
    }
    fn state(&self, plugin: &str) -> Storage {
        let record = self.installer.record(plugin).unwrap().unwrap();
        Storage::new(
            self.setup.state_directory.clone(),
            record.installation_id,
            self.setup.vault.clone(),
            self.setup.resources.ledger.clone(),
        )
        .unwrap()
    }
    fn legacy(&self, plugin: &str, value: Value) -> Vec<u8> {
        let bytes = serde_json::to_vec(&value).unwrap();
        std::fs::write(
            self.root
                .join("plugin_state")
                .join(format!("{plugin}.json")),
            &bytes,
        )
        .unwrap();
        bytes
    }
    fn entries(&self) -> Vec<InstallationEntry> {
        self.bootstrap
            .entries(self.installer.clone(), &self.setup)
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.root.starts_with(std::env::temp_dir()));
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
#[test]
fn embedded_bootstrap_preserves_identity_and_migrates_only_official_legacy_data() {
    let fixture = Fixture::new();
    let guides = fixture.legacy("iris.guides", json!({"mode":"safe"}));
    let servers = json!([
        {"id":"nas", "name":"NAS", "endpoint":"http://192.168.1.50:5244/dav/", "auth":{"kind":"basic","username":"user"}, "allow_insecure_auth":true},
        {"id":"embedded", "name":"Legacy URL", "endpoint":"https://alice:embedded-password@nas.example/dav/"}
    ]).to_string();
    let webdav = fixture.legacy("iris.webdav", json!({"servers":servers}));
    fixture
        .setup
        .vault
        .set_secret("builtin.iris.webdav", "secret.nas", Some("legacy-password"))
        .unwrap();
    for file in ["store.json", "locations.json", "settings.json"] {
        std::fs::write(fixture.root.join(file), b"existing-user-data").unwrap();
    }
    let entries = fixture.entries();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.record.is_ok()));
    let record = fixture.installer.record("iris.webdav").unwrap().unwrap();
    let scope = format!("plugin-v2.{}", record.installation_id);
    assert_eq!(
        fixture
            .setup
            .vault
            .get_secret(&scope, "p.secret.nas")
            .as_deref(),
        Some("legacy-password")
    );
    assert_eq!(
        fixture
            .setup
            .vault
            .get_secret(&scope, "p.secret.embedded")
            .as_deref(),
        Some("embedded-password")
    );
    assert_eq!(
        fixture
            .setup
            .vault
            .get_secret("builtin.iris.webdav", "secret.nas")
            .as_deref(),
        Some("legacy-password")
    );
    let read = fixture
        .state("iris.webdav")
        .read_state("servers", &HttpCancel::default())
        .unwrap()
        .0
        .unwrap();
    assert!(!read.contains("embedded-password"));
    let decoded: Value = serde_json::from_str(&read).unwrap();
    assert_eq!(decoded[0]["id"], "nas");
    assert_eq!(decoded[0]["allow_insecure_auth"], false);
    assert_eq!(decoded[1]["endpoint"], "https://nas.example/dav/");
    assert_eq!(
        fixture
            .state("iris.guides")
            .read_state("mode", &HttpCancel::default())
            .unwrap()
            .0
            .as_deref(),
        Some("safe")
    );
    assert_eq!(
        std::fs::read(fixture.root.join("plugin_state/iris.guides.json")).unwrap(),
        guides
    );
    assert_eq!(
        std::fs::read(fixture.root.join("plugin_state/iris.webdav.json")).unwrap(),
        webdav
    );
    assert!(
        !fixture
            .setup
            .state_directory
            .join(format!("{}.origins.json", record.installation_id))
            .exists()
    );
    // A second bootstrap must retain user changes and the same credential scope.
    fixture
        .state("iris.guides")
        .write_state("mode", Some("off"), &HttpCancel::default())
        .unwrap();
    fixture.legacy("iris.guides", json!({"mode":"golden"}));
    assert!(fixture.entries().iter().all(|e| e.record.is_ok()));
    assert_eq!(
        fixture.installer.record("iris.webdav").unwrap().unwrap(),
        record
    );
    assert_eq!(
        fixture
            .state("iris.guides")
            .read_state("mode", &HttpCancel::default())
            .unwrap()
            .0
            .as_deref(),
        Some("off")
    );
    // Missing distribution objects can be reconstructed without replacing identity.
    std::fs::remove_file(fixture.root.join("packages/objects").join(&record.object)).unwrap();
    assert!(fixture.entries().iter().all(|e| e.record.is_ok()));
    assert_eq!(
        fixture.installer.record("iris.webdav").unwrap().unwrap(),
        record
    );
    for file in ["store.json", "locations.json", "settings.json"] {
        assert_eq!(
            std::fs::read(fixture.root.join(file)).unwrap(),
            b"existing-user-data"
        );
    }
    assert_eq!(
        fixture
            .setup
            .resources
            .ledger
            .used(None, Resource::HostBytes),
        0
    );
}
#[test]
fn invalid_legacy_data_is_retryable_without_publishing_partial_config_or_secrets() {
    let fixture = Fixture::new();
    let bad = json!([
        {"id":"ok", "name":"OK", "endpoint":"https://user:password@nas.example/"},
        {"id":"invalid", "name":"Bad", "endpoint":"file:///private"}
    ])
    .to_string();
    fixture.legacy("iris.webdav", json!({"servers":bad}));
    let entries = fixture.entries();
    assert!(
        entries
            .iter()
            .find(|e| e.plugin_id == "iris.guides")
            .unwrap()
            .record
            .is_ok()
    );
    assert!(
        entries
            .iter()
            .find(|e| e.plugin_id == "iris.webdav")
            .unwrap()
            .record
            .is_err()
    );
    let record = fixture.installer.record("iris.webdav").unwrap().unwrap();
    let scope = format!("plugin-v2.{}", record.installation_id);
    assert!(
        fixture
            .setup
            .vault
            .get_secret(&scope, "p.secret.ok")
            .is_none()
    );
    assert!(
        fixture
            .state("iris.webdav")
            .read_state("servers", &HttpCancel::default())
            .unwrap()
            .0
            .is_none()
    );
    assert!(
        !fixture
            .root
            .join("plugin-v2-migration")
            .join(&record.installation_id)
            .join("complete.json")
            .exists()
    );
    // Second distinct bad payload creates a second backup before recovery.
    let bad2 = json!([
        {"id":"invalid2", "name":"Bad2", "endpoint":"file:///private2"}
    ])
    .to_string();
    fixture.legacy("iris.webdav", json!({"servers":bad2}));
    assert!(
        fixture
            .entries()
            .iter()
            .find(|e| e.plugin_id == "iris.webdav")
            .unwrap()
            .record
            .is_err()
    );
    // Correcting a bad record succeeds and prunes stale failed-attempt backups.
    let fixed = json!([{"id":"ok", "name":"OK", "endpoint":"https://user:password@nas.example/"}])
        .to_string();
    fixture.legacy("iris.webdav", json!({"servers":fixed}));
    assert!(fixture.entries().iter().all(|e| e.record.is_ok()));
    let mig_dir = fixture
        .root
        .join("plugin-v2-migration")
        .join(&record.installation_id);
    let backups: Vec<_> = std::fs::read_dir(&mig_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("v1-"))
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "superseded failed-attempt migration backups must be pruned after completion"
    );
    assert_eq!(
        fixture
            .setup
            .vault
            .get_secret(&scope, "p.secret.ok")
            .as_deref(),
        Some("password")
    );
    // A user package can never import this legacy vault bucket through Bootstrap.
    let mut impostor = record;
    impostor.plugin_id = "user.webdav".into();
    assert_eq!(
        migration::migrate(&fixture.root, &impostor, &fixture.setup),
        Err(ErrorCode::Denied)
    );
}

#[test]
fn expanded_official_upgrade_surfaces_review_and_upgrades_in_place() {
    let fixture = Fixture::new();
    let v1_bytes = official_package("iris.expand", "1", &[Capability::UiOverlay]);
    let b1 = Bootstrap::custom(
        fixture.root.clone(),
        vec![("iris.expand".into(), "1".into(), v1_bytes)],
    )
    .unwrap();
    let i1 = Installer::with_bundled(
        fixture.root.join("packages"),
        fixture.setup.resources.ledger.clone(),
        b1.pins(),
    );
    let scan1 = b1
        .scan(i1.clone(), &fixture.setup, &CancelToken::new())
        .unwrap();
    assert!(scan1.pending_reviews.is_empty());
    let installed_v1 = i1.record("iris.expand").unwrap().unwrap();
    let v1_obj = fixture
        .root
        .join("packages/objects")
        .join(&installed_v1.object);
    assert!(v1_obj.exists());

    // Upgrade with expanded capabilities must NOT silently enlarge grants or silently brick.
    let v2_bytes = official_package(
        "iris.expand",
        "2",
        &[Capability::UiOverlay, Capability::Notifications],
    );
    let b2 = Bootstrap::custom(
        fixture.root.clone(),
        vec![("iris.expand".into(), "2".into(), v2_bytes)],
    )
    .unwrap();
    let mut i2 = Installer::with_bundled(
        fixture.root.join("packages"),
        fixture.setup.resources.ledger.clone(),
        b2.pins(),
    );
    let mut scan2 = b2
        .scan(i2.clone(), &fixture.setup, &CancelToken::new())
        .unwrap();
    assert_eq!(scan2.pending_reviews.len(), 1);
    assert!(
        scan2
            .entries
            .iter()
            .any(|e| e.plugin_id == "iris.expand" && e.record == Err(ErrorCode::Denied))
    );
    let upgrade = scan2.pending_reviews.pop().unwrap();
    assert_eq!(upgrade.plugin_id, "iris.expand");
    assert!(upgrade.review.is_bundled());
    assert_eq!(
        upgrade.review.previous().unwrap().installation_id,
        installed_v1.installation_id
    );

    // Explicit approval upgrades in-place preserving installation_id and pruning old object.
    let (installed_v2, superseded) = upgrade
        .review
        .approve_with_cleanup(
            &mut i2,
            BTreeSet::from([Capability::UiOverlay, Capability::Notifications]),
            true,
        )
        .unwrap();
    assert_eq!(installed_v2.installation_id, installed_v1.installation_id);
    assert!(superseded.is_empty());
    assert!(!v1_obj.exists());

    let scan3 = b2.scan(i2, &fixture.setup, &CancelToken::new()).unwrap();
    assert!(scan3.pending_reviews.is_empty());
    assert!(
        scan3
            .entries
            .iter()
            .any(|e| e.plugin_id == "iris.expand" && e.record.is_ok())
    );
}

#[test]
fn legacy_overlong_webdav_ids_remap_deterministically_across_servers_secrets_and_uris() {
    use iris_core::bookmarks::LocationStore;
    use iris_core::media_uri::MediaUri;
    use iris_core::store::{AnnotationEdit, Store};

    let fixture = Fixture::new();
    let long_60 = "legacy-nas-connection-identifier-exceeding-forty-eight-bytes";
    let long_72 = "legacy-secondary-connection-identifier-up-to-ninety-six-bytes-limit-test";
    assert_eq!(long_60.len(), 60);
    assert_eq!(long_72.len(), 72);

    let servers = json!([
        {
            "id": long_60,
            "name": "Primary NAS",
            "endpoint": "https://nas1.example/dav/",
            "auth": {"kind": "basic", "username": "alice"}
        },
        {
            "id": long_72,
            "name": "Archive NAS",
            "endpoint": "https://bob:archive-pass@nas2.example/dav/"
        }
    ])
    .to_string();
    fixture.legacy("iris.webdav", json!({"servers": servers}));
    fixture
        .setup
        .vault
        .set_secret(
            "builtin.iris.webdav",
            &format!("secret.{long_60}"),
            Some("primary-pass"),
        )
        .unwrap();

    // Seed legacy bookmark, favorite/resume record, and rating annotation under long_60.
    std::fs::write(
        fixture.root.join("locations.json"),
        serde_json::to_vec(&json!({
            "bookmarks": [
                {"id": "bm-1", "name": "Movies", "uri": format!("webdav://{long_60}/Movies")}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let legacy_film = MediaUri::Plugin {
        plugin_id: "iris.webdav".into(),
        source_id: long_60.into(),
        entry_id: "/Movies/Film.mkv".into(),
    };
    {
        let mut store = Store::load_from_dir(&fixture.root);
        assert!(store.toggle_favorite(&legacy_film));
        assert!(store.remember(&legacy_film, 42.0, 120.0, false));
        store.save();
        store
            .annotation_target()
            .unwrap()
            .transact(
                Some((
                    std::slice::from_ref(&legacy_film),
                    &AnnotationEdit::Rating { rating: 5 },
                )),
                "mig",
                |_, _| Ok(Arc::new(())),
                || Ok(()),
            )
            .unwrap();
    }

    let entries = fixture.entries();
    assert!(entries.iter().all(|e| e.record.is_ok()));

    let read = fixture
        .state("iris.webdav")
        .read_state("servers", &HttpCancel::default())
        .unwrap()
        .0
        .unwrap();
    let decoded: Value = serde_json::from_str(&read).unwrap();
    let remapped_60 = decoded[0]["id"].as_str().unwrap();
    let remapped_72 = decoded[1]["id"].as_str().unwrap();
    for id in [remapped_60, remapped_72] {
        assert!(id.len() <= 48, "{id}");
        assert!(iris_api::is_valid_id(id), "{id}");
        assert!(iris_api::is_valid_id(&format!("secret.{id}")), "{id}");
    }
    assert_ne!(remapped_60, remapped_72);

    let record = fixture.installer.record("iris.webdav").unwrap().unwrap();
    let scope = format!("plugin-v2.{}", record.installation_id);
    assert_eq!(
        fixture
            .setup
            .vault
            .get_secret(&scope, &format!("p.secret.{remapped_60}"))
            .as_deref(),
        Some("primary-pass")
    );
    assert_eq!(
        fixture
            .setup
            .vault
            .get_secret(&scope, &format!("p.secret.{remapped_72}"))
            .as_deref(),
        Some("archive-pass")
    );

    let locations = LocationStore::load_from(&fixture.root.join("locations.json"));
    assert_eq!(
        locations.bookmarks[0].uri,
        MediaUri::Plugin {
            plugin_id: "iris.webdav".into(),
            source_id: remapped_60.into(),
            entry_id: "/Movies".into(),
        }
    );

    let migrated_film = MediaUri::Plugin {
        plugin_id: "iris.webdav".into(),
        source_id: remapped_60.into(),
        entry_id: "/Movies/Film.mkv".into(),
    };
    let store = Store::load_from_dir(&fixture.root);
    assert!(store.is_favorite(&migrated_film));
    assert!((store.resume(&migrated_film) - 42.0).abs() < 0.01);
    assert_eq!(store.annotations(&migrated_film).unwrap().rating, 5);

    // Deterministic across independent installations.
    let fixture2 = Fixture::new();
    fixture2.legacy(
        "iris.webdav",
        json!({"servers": json!([{"id": long_60, "name": "Primary NAS", "endpoint": "https://nas1.example/dav/"}]).to_string()}),
    );
    assert!(fixture2.entries().iter().all(|e| e.record.is_ok()));
    let decoded2: Value = serde_json::from_str(
        &fixture2
            .state("iris.webdav")
            .read_state("servers", &HttpCancel::default())
            .unwrap()
            .0
            .unwrap(),
    )
    .unwrap();
    assert_eq!(decoded2[0]["id"].as_str().unwrap(), remapped_60);
}

#[test]
fn migration_retry_after_interrupted_reference_remap_recovers_and_preserves_canonical_annotations()
{
    use iris_core::bookmarks::LocationStore;
    use iris_core::media_uri::MediaUri;
    use iris_core::store::Store;

    let fixture = Fixture::new();
    let long_60 = "legacy-nas-connection-identifier-exceeding-forty-eight-bytes";
    let servers_legacy = json!([
        {
            "id": long_60,
            "name": "Primary NAS",
            "endpoint": "https://nas1.example/dav/"
        }
    ])
    .to_string();
    let legacy_bytes = fixture.legacy("iris.webdav", json!({"servers": servers_legacy}));

    // Install official packages first with no legacy files needing lock contention, then
    // simulate an interrupted first migration where `servers` and `v1-<digest>.json` were
    // written before `remap_persisted_webdav_references` and `complete.json` could finish.
    assert!(fixture.entries().iter().all(|e| e.record.is_ok()));
    let record = fixture.installer.record("iris.webdav").unwrap().unwrap();
    let mig_dir = fixture
        .root
        .join("plugin-v2-migration")
        .join(&record.installation_id);
    std::fs::remove_file(mig_dir.join("complete.json")).unwrap();

    let read = fixture
        .state("iris.webdav")
        .read_state("servers", &HttpCancel::default())
        .unwrap()
        .0
        .unwrap();
    let decoded: Value = serde_json::from_str(&read).unwrap();
    let remapped_60 = decoded[0]["id"].as_str().unwrap().to_owned();

    // Seed un-remapped bookmark, progress record, and both legacy + canonical annotations.
    std::fs::write(
        fixture.root.join("locations.json"),
        serde_json::to_vec(&json!({
            "bookmarks": [
                {"id": "bm-retry", "name": "Retry Movies", "uri": format!("webdav://{long_60}/Movies")}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let legacy_film = MediaUri::Plugin {
        plugin_id: "iris.webdav".into(),
        source_id: long_60.into(),
        entry_id: "/Movies/Retry.mkv".into(),
    };
    let canonical_film = MediaUri::Plugin {
        plugin_id: "iris.webdav".into(),
        source_id: remapped_60.clone(),
        entry_id: "/Movies/Retry.mkv".into(),
    };
    {
        let mut store = Store::load_from_dir(&fixture.root);
        assert!(store.remember(&legacy_film, 77.0, 200.0, false));
        store.save();
    }
    let annotations_path = fixture.root.join("annotations.json");
    std::fs::write(
        &annotations_path,
        serde_json::to_vec(&json!({
            legacy_film.to_uri_string(): {"rating": 2, "color": null, "markers": {}},
            canonical_film.to_uri_string(): {"rating": 5, "color": null, "markers": {}},
            format!("webdav://{long_60}/Movies/OnlyLegacy.mkv"): {"rating": 4, "color": null, "markers": {}}
        }))
        .unwrap(),
    )
    .unwrap();

    // Even if `plugin_state/iris.webdav.json` was removed before retry, `v1-<digest>.json` recovers `remapped_ids`.
    std::fs::remove_file(fixture.root.join("plugin_state/iris.webdav.json")).unwrap();
    assert!(fixture.entries().iter().all(|e| e.record.is_ok()));
    assert!(mig_dir.join("complete.json").exists());
    let backups: Vec<_> = std::fs::read_dir(&mig_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("v1-"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(std::fs::read(backups[0].path()).unwrap(), legacy_bytes);

    let locations = LocationStore::load_from(&fixture.root.join("locations.json"));
    assert_eq!(
        locations.bookmarks[0].uri,
        MediaUri::Plugin {
            plugin_id: "iris.webdav".into(),
            source_id: remapped_60.clone(),
            entry_id: "/Movies".into(),
        }
    );

    let store = Store::load_from_dir(&fixture.root);
    assert!((store.resume(&canonical_film) - 77.0).abs() < 0.01);
    assert_eq!(
        store.annotations(&canonical_film).unwrap().rating,
        5,
        "canonical plugin:// annotation must not be overwritten by legacy entry"
    );
    let only_legacy_migrated = MediaUri::Plugin {
        plugin_id: "iris.webdav".into(),
        source_id: remapped_60,
        entry_id: "/Movies/OnlyLegacy.mkv".into(),
    };
    assert_eq!(store.annotations(&only_legacy_migrated).unwrap().rating, 4);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&annotations_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
