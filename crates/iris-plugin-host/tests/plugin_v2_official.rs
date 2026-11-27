//! Distribution tests use freshly built official .irisplugin archives, not WAT
//! substitutes. Run scripts/test-plugin-v2-official.py to supply these fixtures.
use iris_api::{
    OverlayNode,
    v2::{Generation, HostEvent},
};
use iris_core::vault::CredentialVault;
use iris_plugin_host::{
    hub::{Config, Hub},
    installation::{Approval, Installer},
    ledger::Resource,
    package::{Package, Provenance},
    runtime::{Resources, Setup},
    ui::PluginManager,
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
struct TestDir(PathBuf);
impl TestDir {
    fn new(prefix: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{id}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        if self.0.starts_with(std::env::temp_dir()) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

struct Fixture {
    root: TestDir,
    resources: Resources,
    hub: Option<Hub>,
}
impl Fixture {
    fn load(variable: &str) -> Self {
        let path = std::env::var_os(variable).expect("official package fixture environment");
        let bytes = std::fs::read(path).unwrap();
        let root = TestDir::new("iris-official");
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
        let mut installer = Installer::with_bundled(
            root.path().join("packages"),
            resources.ledger.clone(),
            index,
        );
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
                state_directory: root.path().join("state"),
                temporary_directory: root.path().join("temp"),
                vault: CredentialVault::in_memory(),
                wake: Arc::new(|| {}),
            },
        })
        .unwrap();
        Self {
            root,
            resources,
            hub: Some(hub),
        }
    }
    fn hub(&self) -> &Hub {
        self.hub.as_ref().expect("hub active")
    }
    fn shutdown(mut self) {
        drop(self.hub.take());
        wait(|| self.resources.ledger.used(None, Resource::HostBytes) == 0);
    }
}

#[path = "support/official_bootstrap.rs"]
mod bootstrap;
#[path = "support/official_webdav.rs"]
mod webdav;
#[path = "support/official_webdav_config.rs"]
mod webdav_config;
#[test]
#[ignore = "build official packages with scripts/test-plugin-v2-official.py"]
fn official_guides_wasm_persists_state_across_single_window_reopen() {
    let fixture = Fixture::load("IRIS_V2_GUIDES_PACKAGE");
    let (one, _) = fixture
        .hub()
        .open_window(Generation::default(), None)
        .unwrap();
    assert!(matches!(
        fixture.hub().open_window(Generation::default(), None),
        Err(iris_api::v2::ErrorCode::Backpressure)
    ));
    let mut one = PluginManager::new(one, None);
    wait(|| {
        one.poll();
        one.client().is_ready("iris.guides") && !one.sidebar_panels().is_empty()
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
    one.send_to(
        "iris.guides",
        HostEvent::Command {
            id: "cross".into(),
            snapshot: None,
        },
    );
    wait(|| {
        one.poll();
        one.overlay_nodes()
            .any(|(_, node)| matches!(node, OverlayNode::Crosshair { .. }))
    });
    // Observe an actual persisted state snapshot before destroying the control.
    wait(|| {
        std::fs::read_dir(fixture.root.path().join("state"))
            .ok()
            .is_some_and(|files| {
                files.flatten().any(|file| {
                    std::fs::read_to_string(file.path())
                        .ok()
                        .is_some_and(|bytes| bytes.contains("\"mode\":\"cross\""))
                })
            })
    });
    drop(one);
    wait(|| fixture.resources.ledger.used(None, Resource::ControlStore) == 0);
    let (reopened, _) = fixture
        .hub()
        .open_window(Generation::default(), None)
        .unwrap();
    let mut reopened = PluginManager::new(reopened, None);
    wait(|| {
        reopened.poll();
        reopened
            .overlay_nodes()
            .any(|(_, node)| matches!(node, OverlayNode::Crosshair { .. }))
    });
    reopened.client().set_enabled("iris.guides", false).unwrap();
    wait(|| {
        reopened.poll();
        reopened.overlay_nodes().count() == 0
    });
    drop(reopened);
    fixture.shutdown();
}
