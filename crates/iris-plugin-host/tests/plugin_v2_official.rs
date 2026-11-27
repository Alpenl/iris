//! Distribution tests use freshly built official .irisplugin archives, not WAT
//! substitutes. Run scripts/test-plugin-v2-official.py to supply these fixtures.
use iris_api::{
    OverlayNode,
    v2::{ErrorCode, HostEvent, source::PlaybackSnapshot},
};
use iris_plugin_host::{Notice, PluginHost, WindowSession, open_host_with_bundled_package};
use std::{
    collections::BTreeMap,
    path::PathBuf,
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
    host: PluginHost,
}

impl Fixture {
    fn load(variable: &str) -> Self {
        let path = std::env::var_os(variable).expect("official package fixture environment");
        let bytes = std::fs::read(path).unwrap();
        let root = TestDir::new("iris-official");
        let plugin = match variable {
            "IRIS_V2_GUIDES_PACKAGE" => "iris.guides",
            "IRIS_V2_WEBDAV_PACKAGE" => "iris.webdav",
            _ => panic!("unknown official fixture"),
        };
        let host = open_host_with_bundled_package(root.path(), plugin, &bytes).unwrap();
        Self { root, host }
    }

    fn open_window(&self) -> WindowSession {
        self.host.open_window(&[], "dark").0
    }

    fn shutdown(self) {
        self.host.shutdown();
        wait(|| self.host.host_bytes_used() == 0);
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
    let mut one = fixture.open_window();
    let mut second = fixture.open_window();
    assert!(!second.is_connected());
    assert!(matches!(
        second.poll().notices.as_slice(),
        [Notice::Error {
            code: ErrorCode::Backpressure,
            ..
        }]
    ));
    drop(second);

    wait(|| {
        let ready = one.is_ready("iris.guides");
        one.poll();
        ready
            && one
                .list()
                .iter()
                .any(|p| p.id == "iris.guides" && p.status == iris_plugin_host::PluginStatus::Ready)
            && !one.sidebar_panels().is_empty()
    });
    assert_eq!(fixture.host.control_stores_used(), 1);
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
    wait(|| fixture.host.control_stores_used() == 0);
    let mut reopened = fixture.open_window();
    wait(|| {
        reopened.poll();
        reopened
            .overlay_nodes()
            .any(|(_, node)| matches!(node, OverlayNode::Crosshair { .. }))
    });
    reopened.set_enabled_with_snapshot("iris.guides", false, PlaybackSnapshot::default());
    wait(|| {
        reopened.poll();
        reopened.overlay_nodes().count() == 0
    });
    drop(reopened);
    fixture.shutdown();
}

#[test]
fn check_embedded_runtime_verifies_both_official_plugins() {
    let receipt = iris_plugin_host::check_embedded_runtime().unwrap();
    assert_eq!(receipt["status"], "ok");
    assert_eq!(receipt["protocol"], 2);
    assert_eq!(receipt["abi"], 2);
    let mut ids: Vec<&str> = receipt["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["plugin"].as_str())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["iris.guides", "iris.webdav"]);
}
