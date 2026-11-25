use super::*;
use crate::{
    plugin_v2::{ledger::Resource, runtime::Resources, storage::Storage},
    vault::CredentialVault,
    vfs::HttpCancel,
};
use serde_json::{Value, json};
use std::sync::Arc;

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
            tools: BTreeMap::new(),
            models: BTreeMap::new(),
            tool_catalog: Default::default(),
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
    // Correcting a bad record can retry without deleting the immutable first backup.
    let fixed = json!([{"id":"ok", "name":"OK", "endpoint":"https://user:password@nas.example/"}])
        .to_string();
    fixture.legacy("iris.webdav", json!({"servers":fixed}));
    assert!(fixture.entries().iter().all(|e| e.record.is_ok()));
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
