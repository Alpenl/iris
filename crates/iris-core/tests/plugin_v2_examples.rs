//! Executable authoring examples are exercised as untrusted user packages.
use iris_api::v2::Generation;
use iris_core::{
    plugin_v2::{
        hub::{Config, Hub, Notice},
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
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
#[track_caller]
fn wait(mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
#[ignore = "build real SDK examples with scripts/test-plugin-v2-examples.py"]
fn user_packages_provide_theme_timer_and_real_random_access_pcm() {
    let input =
        PathBuf::from(std::env::var_os("IRIS_V2_EXAMPLES").expect("example archive directory"));
    for name in [
        "cinema-ui",
        "watch-stats",
        "virtual-audio",
        "bilibili",
        "baidupan",
    ] {
        let id = match name {
            "bilibili" => "community.bilibili".to_owned(),
            "baidupan" => "community.baidupan".to_owned(),
            _ => format!("example.{name}"),
        };
        let root =
            std::env::temp_dir().join(format!("iris-example-{}-{}", std::process::id(), name));
        assert!(!root.exists(), "fixture root must be new");
        let resources = Resources::new().unwrap();
        let bytes = std::fs::read(input.join(format!("{id}.irisplugin"))).unwrap();
        let package = Package::from_zip(&bytes, Provenance::User).unwrap();
        let mut installer = Installer::new(root.join("packages"), resources.ledger.clone());
        installer
            .install(
                &bytes,
                Approval {
                    digest: package.digest,
                    grants: package.manifest.permissions.into_iter().collect(),
                    network: package.manifest.network,
                    previous: None,
                },
            )
            .unwrap();
        let hub = Hub::new(Config {
            installer,
            bootstrap: None,
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
        let (client, _) = hub.open_window(Generation::default(), None).unwrap();
        let mut manager = PluginManager::new(client, Some(id.clone()));
        wait(|| {
            manager.poll();
            manager.client().is_ready(&id)
        });
        if name == "cinema-ui" {
            assert_eq!(resources.ledger.used(None, Resource::ControlStore), 0);
            assert!(manager.active_ui_overrides().0.is_some());
        } else if name == "watch-stats" {
            wait(|| {
                manager.poll();
                !manager.status_badges().is_empty()
            });
        } else if name == "bilibili" {
            wait(|| {
                manager.poll();
                !manager.sidebar_panels().is_empty() && !manager.status_badges().is_empty()
            });
            assert_eq!(manager.media_sources().len(), 7);
            assert!(
                !manager
                    .url_resolvers(Some("https://www.bilibili.com/video/BV1uv411q7Mv"))
                    .is_empty()
            );
            assert!(
                !manager
                    .url_resolvers(Some("https://live.bilibili.com/6"))
                    .is_empty()
            );
        } else if name == "baidupan" {
            wait(|| {
                manager.poll();
                !manager.sidebar_panels().is_empty() && !manager.status_badges().is_empty()
            });
            assert_eq!(manager.media_sources().len(), 4);
        } else {
            let request = manager
                .client()
                .resolve(&id, "audio", None, "tone", None)
                .unwrap();
            let mut playback = None;
            wait(|| {
                for notice in manager.client().poll() {
                    match notice {
                        Notice::Playback {
                            ticket,
                            playback: p,
                            ..
                        } => {
                            assert_eq!(ticket.id, request);
                            playback = Some(p);
                        }
                        Notice::Error { code, .. } => panic!("example playback: {code:?}"),
                        _ => {}
                    }
                }
                playback.is_some()
            });
            let playback = playback.unwrap();
            let url = url::Url::parse(&playback.url).unwrap();
            let authority = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
            let mut socket = TcpStream::connect(&authority).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            write!(
                socket,
                "GET {} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n",
                url.path()
            )
            .unwrap();
            let mut response = Vec::new();
            socket.take(1 << 20).read_to_end(&mut response).unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200 "));
            let start = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap() + 4;
            let bytes = &response[start..];
            assert_eq!(bytes.len(), 48044);
            assert_eq!(&bytes[..4], b"RIFF");
            assert_eq!(&bytes[8..12], b"WAVE");
            assert!(bytes[44..].iter().any(|b| *b != 0));
            drop(playback);
        }
        drop((manager, hub));
        wait(|| resources.ledger.used(None, Resource::HostBytes) == 0);
        assert_eq!(resources.ledger.used(None, Resource::WasmBytes), 0);
        assert!(root.starts_with(std::env::temp_dir()));
        std::fs::remove_dir_all(root).unwrap();
    }
}
