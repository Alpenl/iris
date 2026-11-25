use super::*;
use iris_core::plugin_v2::{official::Bootstrap, storage::Storage};
use iris_core::vfs::HttpCancel;

#[test]
#[ignore = "official embedded Wasm lifecycle; scripts/test-plugin-v2-official.py"]
fn embedded_distribution_bootstraps_before_window_start_and_respects_disabled_plugins() {
    let root = std::env::temp_dir().join(format!(
        "iris-bootstrap-runtime-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let legacy = root.join("plugin_state");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("iris.guides.json"), br#"{"mode":"thirds"}"#).unwrap();
    let servers = serde_json::json!([{"id":"original-source-id","name":"Migrated NAS","endpoint":"https://nas.example/dav/"}]).to_string();
    std::fs::write(
        legacy.join("iris.webdav.json"),
        serde_json::to_vec(&serde_json::json!({"servers":servers})).unwrap(),
    )
    .unwrap();
    let resources = Resources::new().unwrap();
    let bootstrap = Bootstrap::embedded(root.clone()).unwrap();
    let installer = Installer::with_bundled(
        root.join("packages"),
        resources.ledger.clone(),
        bootstrap.pins(),
    );
    let vault = CredentialVault::in_memory();
    let setup = Setup {
        resources: resources.clone(),
        state_directory: root.join("state"),
        temporary_directory: root.join("temp"),
        vault: vault.clone(),
        tools: BTreeMap::new(),
        models: BTreeMap::new(),
        tool_catalog: Default::default(),
        wake: Arc::new(|| {}),
    };
    let hub = Hub::new(Config {
        installer: installer.clone(),
        bootstrap: Some(bootstrap),
        runtime: setup,
        disabled: BTreeSet::from(["iris.webdav".into()]),
    })
    .unwrap();
    let (client, _) = hub.open_window(Generation::default(), None).unwrap();
    let mut ui = PluginManager::new(client, None);
    wait(|| {
        ui.poll();
        ui.overlay_nodes()
            .any(|(_, n)| matches!(n, OverlayNode::Grid { cols: 3, .. }))
    });
    let catalog = ui.client().catalog();
    assert!(
        catalog
            .plugins
            .iter()
            .any(|p| p.id == "iris.webdav" && !p.enabled && !ui.client().is_ready("iris.webdav"))
    );
    assert_eq!(resources.ledger.used(None, Resource::ControlStore), 1);
    let installed = installer.record("iris.webdav").unwrap().unwrap();
    let state = Storage::new(
        root.join("state"),
        installed.installation_id,
        vault,
        resources.ledger.clone(),
    )
    .unwrap();
    assert!(
        state
            .read_state("servers", &HttpCancel::default())
            .unwrap()
            .0
            .unwrap()
            .contains("original-source-id")
    );
    ui.client().set_enabled("iris.webdav", true).unwrap();
    wait(|| {
        ui.poll();
        ui.media_sources()
            .iter()
            .any(|(p, _, source)| *p == "iris.webdav" && source.id == "original-source-id")
    });
    assert_eq!(resources.ledger.used(None, Resource::ControlStore), 2);
    drop((catalog, ui, hub, state, installer));
    wait(|| resources.ledger.used(None, Resource::HostBytes) == 0);
    assert!(root.starts_with(std::env::temp_dir()));
    std::fs::remove_dir_all(root).unwrap();
}
