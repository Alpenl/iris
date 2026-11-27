use std::any::Any;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::{ScopedAttachment, ScopedPlayback, ScopedStream, empty_scoped};
use crate::library::Kind;
use crate::media_uri::MediaUri;
use crate::player::machine::{
    FailedTransport, LoadedTransport, MediaIdentity, MediaSourceState, OpeningTransport,
};
use crate::player::{
    Chapter, Command, ExternalTrack, RemoteMediaRequest, Session, SessionEvent, TrackKind, prepare,
};
use crate::settings::Settings;
use crate::test_util::TestDir;

#[derive(Default)]
struct TestStream {
    url: String,
    stops: AtomicUsize,
    seeks: AtomicUsize,
}

impl TestStream {
    fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            stops: AtomicUsize::new(0),
            seeks: AtomicUsize::new(0),
        }
    }
}

impl ScopedStream for TestStream {
    fn root_url(&self) -> Result<String> {
        Ok(if self.url.is_empty() {
            "http://127.0.0.1/fixture".into()
        } else {
            self.url.clone()
        })
    }
    fn is_cancelled(&self) -> bool {
        self.stops.load(Ordering::SeqCst) > 0
    }
    fn advance_seek(&self) -> Result<()> {
        self.seeks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn stop(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

struct StubExternalSub {
    url: String,
}

impl ExternalTrack for StubExternalSub {
    fn owner(&self) -> &str {
        "stub-plugin"
    }
    fn id(&self) -> &str {
        "sub-1"
    }
    fn label(&self) -> &str {
        "Stub Plugin Sub"
    }
    fn lang(&self) -> Option<&str> {
        Some("en")
    }
    fn is_audio(&self) -> bool {
        false
    }
    fn url(&self) -> &str {
        &self.url
    }
    fn is_live(&self) -> bool {
        true
    }
}

struct LoopbackAssetServer {
    url: String,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl LoopbackAssetServer {
    fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&stopped);
        let worker = thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let _ = socket.set_nonblocking(false);
                let _ = socket.set_read_timeout(Some(Duration::from_secs(2)));
                let mut req = Vec::new();
                let mut byte = [0u8; 1];
                while !req.ends_with(b"\r\n\r\n") && req.len() < 8192 {
                    if socket.read_exact(&mut byte).is_err() {
                        break;
                    }
                    req.push(byte[0]);
                }
                let _ = write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(&body);
            }
        });
        Self {
            url: format!("http://{addr}/clip.y4m"),
            stopped,
            worker: Some(worker),
        }
    }
}

impl Drop for LoopbackAssetServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[test]
fn release_scoped_preserves_local_and_idle_sources() {
    let mut session = Session::new().unwrap();
    session.release_scoped();
    assert!(matches!(
        session.machine.source,
        MediaSourceState::Idle { .. }
    ));
    session.machine.begin_load(
        1,
        MediaIdentity {
            local_path: None,
            uri: "file:///local.mp4".into(),
            title: None,
            private_identity: false,
            kind: Kind::Video,
            photo: None,
        },
        false,
        OpeningTransport::Local,
        false,
    );
    session.release_scoped();
    session.release_scoped();
    assert!(matches!(
        session.machine.source,
        MediaSourceState::Opening {
            transport: OpeningTransport::Local,
            ..
        }
    ));
    session.shutdown();
}

#[test]
fn release_scoped_stops_plugin_stream_once_and_detaches_event_pump() {
    let mut session = Session::new().unwrap();
    let stream = Arc::new(TestStream::default());
    let playback = Arc::new(ScopedPlayback {
        url: stream.root_url().unwrap(),
        profile: None,
        kind: Kind::Video,
        title: "Fixture".into(),
        is_live: false,
        attachments: Vec::new(),
        stream: stream.clone(),
    });
    session
        .event_pump
        .set_scoped(Arc::downgrade(&playback.stream));
    session.machine.begin_load(
        1,
        MediaIdentity {
            local_path: None,
            uri: "iris-plugin://stub/src/1".into(),
            title: Some("Fixture".into()),
            private_identity: false,
            kind: Kind::Video,
            photo: None,
        },
        false,
        OpeningTransport::PluginScoped {
            playback,
            pending_tracks: vec![ScopedAttachment {
                id: "subtitle".into(),
                url: "memory://WEBVTT\n".into(),
                label: "Subtitle".into(),
                lang: None,
                default: true,
                audio: false,
            }],
        },
        false,
    );
    session.release_scoped();
    assert!(matches!(
        session.machine.source,
        MediaSourceState::Idle { .. }
    ));
    assert!(session.scoped_stream().is_none());
    assert!(
        session
            .event_pump
            .set_scoped(empty_scoped())
            .upgrade()
            .is_none()
    );
    session.release_scoped();
    assert_eq!(stream.stops.load(Ordering::SeqCst), 1);
    session.shutdown();
}

#[test]
fn load_subtitle_works_under_plugin_scoped_and_plugin_track_restores_policy() {
    let video = format!(
        "YUV4MPEG2 W2 H2 F10:1 Ip A1:1 C420jpeg\n{}",
        "FRAME\nxxxxxx".repeat(60)
    )
    .into_bytes();
    let video_server = LoopbackAssetServer::start(video);
    let sub_server =
        LoopbackAssetServer::start(b"WEBVTT\n\n00:00.000 --> 00:03.000\nPlugin Sub\n".to_vec());

    let dir = TestDir::new("scoped-local-sub");
    let local_srt = dir.join("local.srt");
    std::fs::write(
        &local_srt,
        "1\n00:00:00,000 --> 00:00:05,000\nLocal Scoped Sub\n",
    )
    .unwrap();

    let stream = Arc::new(TestStream::new(video_server.url.clone()));
    let playback = Arc::new(ScopedPlayback {
        url: video_server.url.clone(),
        profile: None,
        kind: Kind::Video,
        title: "Scoped Local Sub Fixture".into(),
        is_live: false,
        attachments: Vec::new(),
        stream,
    });
    let mut session = Session::new_headless(true).unwrap();
    let mut events = session.events().unwrap();
    assert!(
        session
            .execute(Command::LoadSubtitle(local_srt.clone()))
            .is_err(),
        "unloaded session must reject LoadSubtitle before touching per-file options"
    );
    session
        .open_scoped(
            playback,
            MediaUri::Plugin {
                plugin_id: "stub".into(),
                source_id: "src".into(),
                entry_id: "vid".into(),
            },
            &Settings::default(),
            0.,
        )
        .unwrap();
    assert!(
        session
            .execute(Command::LoadSubtitle(local_srt.clone()))
            .is_err(),
        "pre-load PluginScoped session must reject LoadSubtitle until media is loaded"
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    while !session.state().loaded {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        assert!(Instant::now() < deadline, "scoped video never loaded");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(session.mpv.get_property_string("demuxer").unwrap(), "lavf");

    session
        .execute(Command::LoadSubtitle(local_srt))
        .expect("local subtitle command must succeed in PluginScoped mode");
    // Scoped lavf policy must already be restored immediately after LoadSubtitle,
    // without relying on a later attach_plugin_track call.
    assert_eq!(
        session
            .mpv
            .get_property_string("file-local-options/demuxer")
            .unwrap(),
        "lavf"
    );
    let immediate_lavf_o = session
        .mpv
        .get_property_string("file-local-options/demuxer-lavf-o")
        .unwrap();
    assert!(
        immediate_lavf_o.contains("protocol_whitelist=")
            && immediate_lavf_o.contains("http,tcp")
            && !immediate_lavf_o.contains("file"),
        "expected LoadSubtitle to immediately restore protocol_whitelist without file, got {immediate_lavf_o:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        session.flush_deferred();
        if !session.tracks_of(TrackKind::Sub).is_empty()
            && session.mpv.get_property_string("sub-text").ok().as_deref()
                == Some("Local Scoped Sub")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "local subtitle did not load under PluginScoped: tracks={:?}, sub-text={:?}",
            session.tracks_of(TrackKind::Sub),
            session.mpv.get_property_string("sub-text").ok()
        );
        thread::sleep(Duration::from_millis(10));
    }

    // Subsequent plugin attachment restores the restricted lavf policy.
    session
        .attach_plugin_track(Arc::new(StubExternalSub {
            url: sub_server.url.clone(),
        }))
        .unwrap();
    assert_eq!(
        session
            .mpv
            .get_property_string("file-local-options/demuxer")
            .unwrap(),
        "lavf"
    );
    let lavf_o = session
        .mpv
        .get_property_string("file-local-options/demuxer-lavf-o")
        .unwrap();
    assert!(
        lavf_o.contains("protocol_whitelist=")
            && lavf_o.contains("http,tcp")
            && !lavf_o.contains("file"),
        "expected restricted protocol_whitelist without file, got {lavf_o:?}"
    );
    session.shutdown();
}

#[test]
fn gpu_reset_reopen_of_plugin_scoped_does_not_advance_seek_and_clears_cancelled() {
    let stream = Arc::new(TestStream::new("http://127.0.0.1:9/unloaded.mp4"));
    let playback = Arc::new(ScopedPlayback {
        url: stream.root_url().unwrap(),
        profile: None,
        kind: Kind::Video,
        title: "GPU Reset Fixture".into(),
        is_live: false,
        attachments: Vec::new(),
        stream: stream.clone(),
    });
    let uri = MediaUri::Plugin {
        plugin_id: "stub".into(),
        source_id: "src".into(),
        entry_id: "gpu".into(),
    };

    let mut session = Session::new_headless(true).unwrap();
    let settings = Settings::default();
    session
        .open_scoped(Arc::clone(&playback), uri.clone(), &settings, 0.)
        .unwrap();
    assert_eq!(stream.seeks.load(Ordering::SeqCst), 0);

    // Explicit user open_scoped on the same stream advances seek.
    session
        .open_scoped(Arc::clone(&playback), uri, &settings, 5.)
        .unwrap();
    assert_eq!(stream.seeks.load(Ordering::SeqCst), 1);

    // Simulate unloaded mpv state after a GPU device reset.
    session.mpv.command(&["stop"]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while session
        .mpv
        .get_property_string("path")
        .is_ok_and(|p| !p.is_empty())
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }

    session.recover_unloaded_after_gpu_reset(&settings, 0., [0., 0.]);
    assert_eq!(
        stream.seeks.load(Ordering::SeqCst),
        1,
        "GPU-reset reopen must not spuriously call advance_seek()"
    );
    assert!(session.machine.source.is_plugin_scoped());

    // When the scoped stream has expired/cancelled, GPU-reset recovery discards it cleanly.
    stream.stop();
    session.mpv.command(&["stop"]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while session
        .mpv
        .get_property_string("path")
        .is_ok_and(|p| !p.is_empty())
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    session.recover_unloaded_after_gpu_reset(&settings, 0., [0., 0.]);
    assert!(matches!(
        session.machine.source,
        MediaSourceState::Idle { .. }
    ));
    assert!(!session.state().loaded);
    assert_eq!(stream.seeks.load(Ordering::SeqCst), 1);
    session.shutdown();
}

#[test]
fn idle_pause_updates_state_and_post_eof_open_resumes_playback() {
    let mut session = Session::new_headless(false).unwrap();
    let mut events = session.events().unwrap();
    assert!(!session.state().paused);

    session.mpv.set_property_string("pause", "yes").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !session.state().paused {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        assert!(Instant::now() < deadline, "idle pause=yes did not sync");
        thread::sleep(Duration::from_millis(5));
    }

    session.mpv.set_property_string("pause", "no").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while session.state().paused {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        assert!(Instant::now() < deadline, "idle pause=no did not sync");
        thread::sleep(Duration::from_millis(5));
    }

    let dir = TestDir::new("post-eof-unpause");
    let clip1 = dir.join("clip1.y4m");
    let clip2 = dir.join("clip2.y4m");
    let short_y4m = format!(
        "YUV4MPEG2 W2 H2 F10:1 Ip A1:1 C420jpeg\n{}",
        "FRAME\nxxxxxx".repeat(3)
    )
    .into_bytes();
    let long_y4m = format!(
        "YUV4MPEG2 W2 H2 F10:1 Ip A1:1 C420jpeg\n{}",
        "FRAME\nxxxxxx".repeat(45)
    )
    .into_bytes();
    std::fs::write(&clip1, &short_y4m).unwrap();
    std::fs::write(&clip2, &long_y4m).unwrap();

    session.open(prepare(&clip1, None).unwrap(), 0.).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while !(session.state().ended && session.state().paused) {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        assert!(
            Instant::now() < deadline,
            "clip1 never reached keep-open EOF: ended={} paused={}",
            session.state().ended,
            session.state().paused
        );
        thread::sleep(Duration::from_millis(10));
    }

    session.open(prepare(&clip2, None).unwrap(), 0.).unwrap();
    assert!(!session.state().paused);
    let deadline = Instant::now() + Duration::from_secs(8);
    while !(session.state().loaded && session.state().time_pos >= 0.15 && !session.state().paused) {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        assert!(
            Instant::now() < deadline,
            "clip2 froze after EOF open: loaded={} paused={} pos={}",
            session.state().loaded,
            session.state().paused,
            session.state().time_pos
        );
        thread::sleep(Duration::from_millis(10));
    }

    // Also verify open_scoped after natural EOF starts playing instead of freezing paused.
    session.open(prepare(&clip1, None).unwrap(), 0.).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while !(session.state().ended && session.state().paused) {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        assert!(Instant::now() < deadline, "clip1 second EOF timed out");
        thread::sleep(Duration::from_millis(10));
    }

    let scoped_server = LoopbackAssetServer::start(long_y4m);
    let stream = Arc::new(TestStream::new(scoped_server.url.clone()));
    let playback = Arc::new(ScopedPlayback {
        url: scoped_server.url.clone(),
        profile: None,
        kind: Kind::Video,
        title: "Post-EOF Scoped".into(),
        is_live: false,
        attachments: Vec::new(),
        stream,
    });
    session
        .open_scoped(
            playback,
            MediaUri::Plugin {
                plugin_id: "stub".into(),
                source_id: "src".into(),
                entry_id: "after-eof".into(),
            },
            &Settings::default(),
            0.,
        )
        .unwrap();
    assert!(!session.state().paused);
    let deadline = Instant::now() + Duration::from_secs(8);
    while !(session.state().loaded && session.state().time_pos >= 0.15 && !session.state().paused) {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        assert!(
            Instant::now() < deadline,
            "scoped clip froze after EOF: loaded={} paused={} pos={}",
            session.state().loaded,
            session.state().paused,
            session.state().time_pos
        );
        thread::sleep(Duration::from_millis(10));
    }
    session.shutdown();
}

#[test]
fn deferred_local_subtitle_waits_for_scoped_track_preserves_policy_and_wins_selection() {
    let video = format!(
        "YUV4MPEG2 W2 H2 F10:1 Ip A1:1 C420jpeg\n{}",
        "FRAME\nxxxxxx".repeat(60)
    )
    .into_bytes();
    let video_server = LoopbackAssetServer::start(video.clone());
    let scoped_sub_server = LoopbackAssetServer::start(
        b"WEBVTT\n\n00:00.000 --> 00:05.000\nDefault Scoped Sub\n".to_vec(),
    );
    let plugin_sub_server = LoopbackAssetServer::start(
        b"WEBVTT\n\n00:00.000 --> 00:05.000\nPlugin External Sub\n".to_vec(),
    );

    let dir = TestDir::new("deferred-local-sub");
    let local_srt = dir.join("user.srt");
    std::fs::write(
        &local_srt,
        "1\n00:00:00,000 --> 00:00:05,000\nUser Local Sub\n",
    )
    .unwrap();
    let local_video = dir.join("local.y4m");
    std::fs::write(&local_video, &video).unwrap();

    let stream = Arc::new(TestStream::new(video_server.url.clone()));
    let playback = Arc::new(ScopedPlayback {
        url: video_server.url.clone(),
        profile: None,
        kind: Kind::Video,
        title: "Deferred Sub Fixture".into(),
        is_live: false,
        attachments: vec![ScopedAttachment {
            id: "default-sub".into(),
            url: scoped_sub_server.url.clone(),
            label: "Scoped Default".into(),
            lang: Some("en".into()),
            default: true,
            audio: false,
        }],
        stream,
    });

    let mut session = Session::new_headless(true).unwrap();
    let mut events = session.events().unwrap();
    session
        .open_scoped(
            playback,
            MediaUri::Plugin {
                plugin_id: "stub".into(),
                source_id: "src".into(),
                entry_id: "vid".into(),
            },
            &Settings::default(),
            0.,
        )
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(8);
    while !session.state().loaded {
        if let Ok(update) = events.try_recv() {
            session.apply_update(&update);
            if session.state().loaded {
                break;
            }
        } else {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(Instant::now() < deadline, "scoped video never loaded");
    }

    // Queue LoadSubtitle immediately after Loaded while scoped `sub-add` is still in flight.
    session
        .execute(Command::LoadSubtitle(local_srt.clone()))
        .unwrap();
    let lavf_during = session
        .mpv
        .get_property_string("file-local-options/demuxer-lavf-o")
        .unwrap();
    assert!(
        lavf_during.contains("protocol_whitelist=")
            && lavf_during.contains("http,tcp")
            && !lavf_during.contains("file"),
        "in-flight scoped sub-add must keep restricted protocol_whitelist, got {lavf_during:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        session.flush_deferred();
        if session.tracks_of(TrackKind::Sub).len() == 2
            && session.mpv.get_property_string("sub-text").ok().as_deref() == Some("User Local Sub")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expected both subs loaded and user local sub selected, tracks={:?}, sub-text={:?}",
            session.tracks_of(TrackKind::Sub),
            session.mpv.get_property_string("sub-text").ok()
        );
        thread::sleep(Duration::from_millis(10));
    }
    let lavf_after = session
        .mpv
        .get_property_string("file-local-options/demuxer-lavf-o")
        .unwrap();
    assert!(
        lavf_after.contains("protocol_whitelist=")
            && lavf_after.contains("http,tcp")
            && !lavf_after.contains("file")
    );

    // Local media with an external plugin track must also preserve restricted attachment policy.
    session
        .open(prepare(&local_video, None).unwrap(), 0.)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while !session.state().loaded {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        assert!(Instant::now() < deadline, "local video never loaded");
        thread::sleep(Duration::from_millis(5));
    }
    session
        .attach_plugin_track(Arc::new(StubExternalSub {
            url: plugin_sub_server.url.clone(),
        }))
        .unwrap();
    session.execute(Command::LoadSubtitle(local_srt)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        session.flush_deferred();
        if session.tracks_of(TrackKind::Sub).len() == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "local+plugin subs did not both load: {:?}",
            session.tracks_of(TrackKind::Sub)
        );
        thread::sleep(Duration::from_millis(10));
    }
    let local_lavf = session
        .mpv
        .get_property_string("file-local-options/demuxer-lavf-o")
        .unwrap();
    assert!(
        local_lavf.contains("protocol_whitelist=")
            && local_lavf.contains("http,tcp")
            && !local_lavf.contains("file"),
        "LoadSubtitle on Local media with plugin tracks must restore policy, got {local_lavf:?}"
    );
    session.shutdown();
}

#[test]
fn toggle_playback_at_eof_advances_scoped_seek_and_error_preserves_gpu_reset_recovery() {
    let video = format!(
        "YUV4MPEG2 W2 H2 F10:1 Ip A1:1 C420jpeg\n{}",
        "FRAME\nxxxxxx".repeat(40)
    )
    .into_bytes();
    let video_server = LoopbackAssetServer::start(video);
    let sub_server =
        LoopbackAssetServer::start(b"WEBVTT\n\n00:00.000 --> 00:03.000\nRecovered Sub\n".to_vec());

    let stream = Arc::new(TestStream::new(video_server.url.clone()));
    let playback = Arc::new(ScopedPlayback {
        url: video_server.url.clone(),
        profile: None,
        kind: Kind::Video,
        title: "Recovery Fixture".into(),
        is_live: false,
        attachments: Vec::new(),
        stream: stream.clone(),
    });
    let uri = MediaUri::Plugin {
        plugin_id: "stub".into(),
        source_id: "src".into(),
        entry_id: "rec".into(),
    };
    let settings = Settings::default();
    let mut session = Session::new_headless(true).unwrap();
    let mut events = session.events().unwrap();
    session.open_scoped(playback, uri, &settings, 0.).unwrap();

    let deadline = Instant::now() + Duration::from_secs(8);
    while !session.state().loaded {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        assert!(Instant::now() < deadline, "scoped video never loaded");
        thread::sleep(Duration::from_millis(5));
    }

    // TogglePlayback from EOF must advance ScopedStream seek generation before seeking to 0.
    session.apply(&SessionEvent::Ended(true));
    assert_eq!(stream.seeks.load(Ordering::SeqCst), 0);
    session.execute(Command::TogglePlayback).unwrap();
    assert_eq!(stream.seeks.load(Ordering::SeqCst), 1);

    let held_track: Arc<dyn ExternalTrack> = Arc::new(StubExternalSub {
        url: sub_server.url.clone(),
    });
    session
        .attach_plugin_track(Arc::clone(&held_track))
        .unwrap();
    session
        .set_plugin_timeline(
            "stub-plugin".into(),
            &[
                Chapter {
                    title: "Intro".into(),
                    time: 0.,
                },
                Chapter {
                    title: "Main".into(),
                    time: 2.,
                },
            ],
        )
        .unwrap();

    // Simulate GPU-reset EndReason::Error arriving before recover_unloaded_after_gpu_reset.
    session.mpv.command(&["stop"]).unwrap();
    session.apply(&SessionEvent::Error(-13));
    assert!(!session.state().loaded);
    assert!(session.state().chapters.is_empty());
    assert!(matches!(
        session.machine.source,
        MediaSourceState::Failed {
            transport: FailedTransport::PluginScoped { .. },
            ..
        }
    ));

    let deadline = Instant::now() + Duration::from_secs(3);
    while session
        .mpv
        .get_property_string("path")
        .is_ok_and(|p| !p.is_empty())
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }

    session.recover_unloaded_after_gpu_reset(&settings, 0., [0., 0.]);
    assert_eq!(
        stream.seeks.load(Ordering::SeqCst),
        1,
        "GPU-reset reload must not advance seek"
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        session.flush_deferred();
        if session.state().loaded && !session.tracks_of(TrackKind::Sub).is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "recovered scoped playback did not reload + reattach sub: loaded={}, subs={:?}",
            session.state().loaded,
            session.tracks_of(TrackKind::Sub)
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        session.enhancements.timeline_owner.as_deref(),
        Some("stub-plugin")
    );
    assert_eq!(session.state().chapters.len(), 2);
    drop(held_track);

    // Also verify DirectRemote transitions to StoppedRemote on Error and recovers on GPU reset.
    session
        .open_remote(
            RemoteMediaRequest {
                uri: MediaUri::HttpUrl(video_server.url.clone()),
                playback_url: video_server.url.clone(),
                title: "Remote Recovery".into(),
                kind: Kind::Video,
                headers: BTreeMap::new(),
                subtitles: Vec::new(),
                is_live: false,
                photo: None,
            },
            &settings,
            0.,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while !session.state().loaded {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        assert!(
            Instant::now() < deadline,
            "direct remote video never loaded"
        );
        thread::sleep(Duration::from_millis(5));
    }
    session.mpv.command(&["stop"]).unwrap();
    session.apply(&SessionEvent::Error(-13));
    assert!(matches!(
        session.machine.source,
        MediaSourceState::Failed {
            transport: FailedTransport::StoppedRemote { .. },
            ..
        }
    ));
    let deadline = Instant::now() + Duration::from_secs(3);
    while session
        .mpv
        .get_property_string("path")
        .is_ok_and(|p| !p.is_empty())
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    session.recover_unloaded_after_gpu_reset(&settings, 0., [0., 0.]);
    assert!(matches!(
        session.machine.source,
        MediaSourceState::Opening {
            transport: OpeningTransport::DirectRemote { .. },
            ..
        } | MediaSourceState::Loaded {
            transport: LoadedTransport::DirectRemote { .. },
            ..
        }
    ));
    let deadline = Instant::now() + Duration::from_secs(8);
    while !session.state().loaded {
        while let Ok(update) = events.try_recv() {
            session.apply_update(&update);
        }
        assert!(
            Instant::now() < deadline,
            "direct remote did not reload after GPU reset"
        );
        thread::sleep(Duration::from_millis(10));
    }
    session.shutdown();
}
