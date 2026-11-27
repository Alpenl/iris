use super::*;
use iris_core::vault::CredentialVault;

#[test]
#[ignore = "official embedded Wasm lifecycle; scripts/test-plugin-v2-official.py"]
fn embedded_distribution_bootstraps_before_window_start_and_respects_disabled_plugins() {
    let root = TestDir::new("iris-bootstrap-runtime");
    let legacy = root.path().join("plugin_state");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("iris.guides.json"), br#"{"mode":"thirds"}"#).unwrap();
    let servers = serde_json::json!([{"id":"original-source-id","name":"Migrated NAS","endpoint":"https://nas.example/dav/"}]).to_string();
    std::fs::write(
        legacy.join("iris.webdav.json"),
        serde_json::to_vec(&serde_json::json!({"servers":servers})).unwrap(),
    )
    .unwrap();

    let disabled = vec!["iris.webdav".to_owned()];
    let host =
        PluginHost::open_in(root.path(), CredentialVault::in_memory(), &disabled, true).unwrap();
    let (mut ui, _) = host.open_window(&disabled, "dark");
    wait(|| {
        ui.poll();
        ui.overlay_nodes()
            .any(|(_, n)| matches!(n, OverlayNode::Grid { cols: 3, .. }))
    });
    assert!(
        ui.list()
            .iter()
            .any(|p| p.id == "iris.webdav" && !p.enabled && !ui.is_ready("iris.webdav"))
    );
    assert_eq!(host.control_stores_used(), 1);

    // Migrated WebDAV state record is persisted before enabling `iris.webdav`.
    let migrated_found = std::fs::read_dir(root.path().join("plugin-v2-state"))
        .unwrap()
        .flatten()
        .any(|entry| {
            entry.path().extension().is_some_and(|ext| ext == "json")
                && std::fs::read_to_string(entry.path())
                    .ok()
                    .is_some_and(|text| text.contains("original-source-id"))
        });
    assert!(migrated_found);

    ui.set_enabled_with_snapshot("iris.webdav", true, PlaybackSnapshot::default());
    wait(|| {
        ui.poll();
        ui.media_sources()
            .iter()
            .any(|(p, _, source)| *p == "iris.webdav" && source.id == "original-source-id")
    });
    assert_eq!(host.control_stores_used(), 2);
    drop(ui);
    host.shutdown();
    wait(|| host.host_bytes_used() == 0);
}
