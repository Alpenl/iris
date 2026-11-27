//! Executable authoring examples are exercised as untrusted user packages
//! strictly through the public [`PluginHost`] and [`WindowSession`] facade.
use iris_core::vault::CredentialVault;
use iris_plugin_host::{Notice, PluginHost};
use std::{
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
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

struct TestDir(PathBuf);

impl Drop for TestDir {
    fn drop(&mut self) {
        if self.0.starts_with(std::env::temp_dir()) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
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
        let root = TestDir(std::env::temp_dir().join(format!(
            "iris-example-{}-{}",
            std::process::id(),
            name
        )));
        assert!(!root.0.exists(), "fixture root must be new");
        std::fs::create_dir_all(&root.0).unwrap();

        let host = PluginHost::open_in(&root.0, CredentialVault::in_memory(), &[], false).unwrap();
        let (mut manager, _) = host.open_window(&[], &format!("plugin:{id}"));
        let pkg_path = input.join(format!("{id}.irisplugin"));
        let review_id = manager.review_package(pkg_path).unwrap();
        let mut summary = None;
        wait(|| {
            for notice in manager.poll().notices {
                match notice {
                    Notice::Review(r) if r.id == review_id => summary = Some(r),
                    Notice::Error { code, .. } => panic!("example review failed: {code:?}"),
                    _ => {}
                }
            }
            summary.is_some()
        });
        let grants = summary
            .unwrap()
            .manifest
            .permissions
            .iter()
            .copied()
            .collect();
        manager.approve_review(review_id, grants, false).unwrap();
        wait(|| {
            let ready = manager.is_ready(&id);
            manager.poll();
            ready
                && manager
                    .list()
                    .iter()
                    .any(|p| p.id == id && p.status == iris_plugin_host::PluginStatus::Ready)
        });

        if name == "cinema-ui" {
            assert_eq!(host.control_stores_used(), 0);
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
            let request = manager.resolve(&id, "audio", None, "tone", None).unwrap();
            let mut playback = None;
            wait(|| {
                for notice in manager.poll().notices {
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
        manager.shutdown();
        drop(manager);
        host.shutdown();
        wait(|| host.host_bytes_used() == 0);
        assert_eq!(host.wasm_bytes_used(), 0);
    }
}
