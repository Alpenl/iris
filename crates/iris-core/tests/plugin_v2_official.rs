//! Distribution tests use freshly built official .irisplugin archives, not WAT
//! substitutes. Run scripts/test-plugin-v2-official.py to supply these fixtures.
use iris_api::{
    OverlayNode,
    v2::{Generation, HostEvent},
};
use iris_core::{
    plugin_v2::{
        hub::{Config, Hub},
        installation::{Approval, Installer},
        ledger::Resource,
        package::{Package, Provenance},
        runtime::{Resources, Setup},
        ui::PluginManager,
    },
    vault::CredentialVault,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

#[track_caller]
fn wait(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !check() {
        assert!(Instant::now() < deadline, "official Wasm fixture deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
}
struct Fixture {
    root: PathBuf,
    resources: Resources,
    hub: Hub,
}
impl Fixture {
    fn load(variable: &str) -> Self {
        let path = std::env::var_os(variable).expect("official package fixture environment");
        let bytes = std::fs::read(path).unwrap();
        let root = std::env::temp_dir().join(format!(
            "iris-official-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let resources = Resources::new().unwrap();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        use std::io::Read;
        for name in ["plugin.json", "plugin.wasm"] {
            let mut content = Vec::new();
            archive
                .by_name(name)
                .unwrap()
                .read_to_end(&mut content)
                .unwrap();
            hash.update(&(name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            hash.update(&(content.len() as u64).to_le_bytes());
            hash.update(&content);
        }
        let digest: [u8; 32] = hash.finish().as_ref().try_into().unwrap();
        let plugin = match variable {
            "IRIS_V2_GUIDES_PACKAGE" => "iris.guides",
            "IRIS_V2_WEBDAV_PACKAGE" => "iris.webdav",
            _ => panic!("unknown official fixture"),
        };
        let index = BTreeMap::from([(plugin.into(), digest)]);
        let package = Package::from_zip(&bytes, Provenance::Bundled(&index)).unwrap();
        let mut installer =
            Installer::with_bundled(root.join("packages"), resources.ledger.clone(), index);
        installer
            .install_bundled(
                &bytes,
                Approval {
                    digest,
                    grants: package.manifest.permissions.into_iter().collect(),
                    network: package.manifest.network,
                    previous: None,
                },
            )
            .unwrap();
        let hub = Hub::new(Config {
            bootstrap: None,
            installer,
            disabled: BTreeSet::new(),
            runtime: Setup {
                resources: resources.clone(),
                state_directory: root.join("state"),
                temporary_directory: root.join("temp"),
                vault: CredentialVault::in_memory(),
                tools: BTreeMap::new(),
                models: BTreeMap::new(),
                tool_catalog: Default::default(),
                wake: Arc::new(|| {}),
            },
        })
        .unwrap();
        Self {
            root,
            resources,
            hub,
        }
    }
}

#[path = "support/official_bootstrap.rs"]
mod bootstrap;
#[path = "support/official_webdav.rs"]
mod webdav;
#[test]
#[ignore = "build official packages with scripts/test-plugin-v2-official.py"]
fn official_guides_wasm_persists_and_isolates_window_presentations() {
    let fixture = Fixture::load("IRIS_V2_GUIDES_PACKAGE");
    let (one, _) = fixture
        .hub
        .open_window(Generation::default(), None)
        .unwrap();
    let (two, _) = fixture
        .hub
        .open_window(Generation::default(), None)
        .unwrap();
    let mut one = PluginManager::new(one, None);
    let mut two = PluginManager::new(two, None);
    wait(|| {
        one.poll();
        two.poll();
        one.client().is_ready("iris.guides")
            && two.client().is_ready("iris.guides")
            && !one.sidebar_panels().is_empty()
            && !two.sidebar_panels().is_empty()
    });
    assert_eq!(
        fixture.resources.ledger.used(None, Resource::ControlStore),
        1
    );
    one.send_to(
        "iris.guides",
        HostEvent::Command {
            id: "thirds".into(),
            snapshot: None,
        },
    );
    wait(|| {
        one.poll();
        two.poll();
        one.overlay_nodes().any(|(_, node)| {
            matches!(
                node,
                OverlayNode::Grid {
                    cols: 3,
                    rows: 3,
                    ..
                }
            )
        })
    });
    assert_eq!(two.overlay_nodes().count(), 0);
    two.send_to(
        "iris.guides",
        HostEvent::Command {
            id: "cross".into(),
            snapshot: None,
        },
    );
    wait(|| {
        one.poll();
        two.poll();
        two.overlay_nodes()
            .any(|(_, node)| matches!(node, OverlayNode::Crosshair { .. }))
    });
    assert!(
        one.overlay_nodes()
            .any(|(_, node)| matches!(node, OverlayNode::Grid { .. }))
    );
    // Observe an actual persisted state snapshot before destroying the control.
    wait(|| {
        std::fs::read_dir(fixture.root.join("state"))
            .ok()
            .is_some_and(|files| {
                files.flatten().any(|file| {
                    std::fs::read_to_string(file.path())
                        .ok()
                        .is_some_and(|bytes| bytes.contains("\"mode\":\"cross\""))
                })
            })
    });
    drop((one, two));
    wait(|| fixture.resources.ledger.used(None, Resource::ControlStore) == 0);
    let (three, _) = fixture
        .hub
        .open_window(Generation::default(), None)
        .unwrap();
    let mut three = PluginManager::new(three, None);
    wait(|| {
        three.poll();
        three
            .overlay_nodes()
            .any(|(_, node)| matches!(node, OverlayNode::Crosshair { .. }))
    });
    three.client().set_enabled("iris.guides", false).unwrap();
    wait(|| {
        three.poll();
        three.overlay_nodes().count() == 0
    });
    let Fixture {
        root,
        resources,
        hub,
    } = fixture;
    drop((three, hub));
    wait(|| resources.ledger.used(None, Resource::HostBytes) == 0);
    assert!(root.starts_with(std::env::temp_dir()));
    std::fs::remove_dir_all(root).unwrap();
}
