use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;

use crate::library::Kind;
use crate::settings::{Hwdec, OnEof};

use super::{
    CommandReceipts, EnhancementView, FailedTransport, LoadedTransport, MediaIdentity,
    MediaSourceState, MpvOp, OpeningTransport, PlayerMachine, coalesce_updates,
};
use crate::player::model::{Command, PlayerActionError, SessionEvent, SessionUpdate};
use crate::player::scoped::{ScopedAttachment, ScopedPlayback, ScopedStream};

#[derive(Default)]
struct FakeScopedStream {
    stops: AtomicUsize,
}

impl ScopedStream for FakeScopedStream {
    fn root_url(&self) -> Result<String> {
        Ok("http://127.0.0.1:9/root.mp4".into())
    }
    fn is_cancelled(&self) -> bool {
        self.stops.load(Ordering::SeqCst) > 0
    }
    fn advance_seek(&self) -> Result<()> {
        Ok(())
    }
    fn stop(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
}

fn identity(uri: &str, kind: Kind) -> MediaIdentity {
    MediaIdentity {
        local_path: None,
        uri: uri.into(),
        title: Some("Clip".into()),
        private_identity: false,
        kind,
        photo: None,
    }
}

fn scoped_playback(
    stream: Arc<FakeScopedStream>,
    attachments: Vec<ScopedAttachment>,
) -> Arc<ScopedPlayback> {
    Arc::new(ScopedPlayback {
        url: "http://127.0.0.1:9/root.mp4".into(),
        profile: None,
        kind: Kind::Video,
        title: "Scoped Clip".into(),
        is_live: false,
        attachments,
        stream,
    })
}

#[test]
fn load_and_rapid_switch_fence_stale_events_and_promote_scoped_tracks() {
    let mut machine = PlayerMachine::new(CommandReceipts::new());
    let stream1 = Arc::new(FakeScopedStream::default());
    let pb1 = scoped_playback(
        stream1.clone(),
        vec![ScopedAttachment {
            id: "old-sub".into(),
            url: "http://127.0.0.1:9/old.vtt".into(),
            label: "Old".into(),
            lang: Some("en".into()),
            default: true,
            audio: false,
        }],
    );
    let init_ops = machine.begin_load(
        10,
        identity("iris-plugin://p/s/1", Kind::Video),
        false,
        OpeningTransport::PluginScoped {
            playback: pb1,
            pending_tracks: vec![ScopedAttachment {
                id: "old-sub".into(),
                url: "http://127.0.0.1:9/old.vtt".into(),
                label: "Old".into(),
                lang: Some("en".into()),
                default: true,
                audio: false,
            }],
        },
        false,
    );
    assert!(init_ops.is_empty());
    assert_eq!(stream1.stops.load(Ordering::SeqCst), 0);

    // Rapid switch to entry 11 before entry 10 finishes loading must stop stream1 once.
    let stream2 = Arc::new(FakeScopedStream::default());
    let tracks2 = vec![
        ScopedAttachment {
            id: "sub".into(),
            url: "http://127.0.0.1:9/new.vtt".into(),
            label: "English".into(),
            lang: Some("en".into()),
            default: true,
            audio: false,
        },
        ScopedAttachment {
            id: "aud".into(),
            url: "http://127.0.0.1:9/new.m4a".into(),
            label: "Commentary".into(),
            lang: Some("ja".into()),
            default: false,
            audio: true,
        },
    ];
    let pb2 = scoped_playback(stream2.clone(), tracks2.clone());
    machine.begin_load(
        11,
        identity("iris-plugin://p/s/2", Kind::Video),
        false,
        OpeningTransport::PluginScoped {
            playback: pb2,
            pending_tracks: tracks2,
        },
        false,
    );
    assert_eq!(stream1.stops.load(Ordering::SeqCst), 1);
    assert_eq!(stream2.stops.load(Ordering::SeqCst), 0);

    // Late events for superseded entry 10 are fenced out completely.
    for stale in [
        SessionEvent::Loaded,
        SessionEvent::Ended(true),
        SessionEvent::Error(-13),
    ] {
        assert!(
            machine
                .reduce_update(
                    &SessionUpdate {
                        playlist_entry_id: Some(10),
                        event: stale,
                    },
                    EnhancementView::default(),
                )
                .is_none()
        );
    }
    assert!(!machine.state.loaded);
    assert!(!machine.state.ended);

    // Entry 11 Loaded promotes Opening -> Loaded and emits RestoreAttachmentPolicy + 2 tracked adds.
    let (ops, reattach) = machine
        .reduce_update(
            &SessionUpdate {
                playlist_entry_id: Some(11),
                event: SessionEvent::Loaded,
            },
            EnhancementView::default(),
        )
        .expect("entry 11 accepted");
    assert!(reattach.is_empty());
    assert!(machine.state.loaded);
    assert!(ops.contains(&MpvOp::RestoreAttachmentPolicy));
    assert!(ops.contains(&MpvOp::cmd_reply(
        1,
        &[
            "sub-add",
            "http://127.0.0.1:9/new.vtt",
            "select",
            "English",
            "en",
        ],
    )));
    assert!(ops.contains(&MpvOp::cmd_reply(
        2,
        &[
            "audio-add",
            "http://127.0.0.1:9/new.m4a",
            "auto",
            "Commentary",
            "ja",
        ],
    )));
    assert!(machine.source.has_in_flight_scoped_adds());
}

#[test]
fn eof_stop_toggle_and_subsequent_open_unpause_without_libmpv() {
    let mut machine = PlayerMachine::new(CommandReceipts::new());
    machine.state.paused = true;
    machine.begin_load(
        1,
        identity("file:///a.mp4", Kind::Video),
        false,
        OpeningTransport::Local,
        false,
    );
    machine.reduce_event(&SessionEvent::Loaded, EnhancementView::default());
    machine.reduce_event(&SessionEvent::Ended(true), EnhancementView::default());
    assert!(machine.state.ended);
    assert!(machine.source.eof_paused());

    // TogglePlayback at EOF seeks to 0 and unpauses.
    let toggle_ops = machine
        .reduce_pure_command(&Command::TogglePlayback)
        .unwrap()
        .unwrap();
    assert_eq!(
        toggle_ops,
        vec![
            MpvOp::cmd_async(&["seek", "0", "absolute+exact"]),
            MpvOp::cmd_async(&["set", "pause", "no"]),
        ]
    );

    // Explicit stop at EOF retains eof_paused in MediaSourceState::Idle so the next load unpauses.
    machine.transition_to_idle(false);
    assert!(matches!(
        machine.source,
        MediaSourceState::Idle { eof_paused: true }
    ));

    let reopen_ops = machine.begin_load(
        2,
        identity("file:///b.mp4", Kind::Video),
        false,
        OpeningTransport::Local,
        machine.source.eof_paused(),
    );
    assert!(!machine.state.paused);
    assert!(!machine.source.eof_paused());
    assert_eq!(
        reopen_ops,
        vec![MpvOp::SetPropertyString {
            name: "pause".into(),
            value: "no".into(),
        }]
    );
}

#[cfg(unix)]
fn non_unicode_path() -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(vec![
        b'/', b't', b'm', b'p', b'/', 0xff, b'.', b's', b'r', b't',
    ]))
}

#[cfg(windows)]
fn non_unicode_path() -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    PathBuf::from(OsString::from_wide(&[
        b'C' as u16,
        b':' as u16,
        b'\\' as u16,
        0xD800,
        b'.' as u16,
        b's' as u16,
        b'r' as u16,
        b't' as u16,
    ]))
}

#[test]
fn deferred_local_subtitles_reject_non_unicode_synchronously_and_flush_after_last_reply() {
    let mut machine = PlayerMachine::new(CommandReceipts::new());
    let stream = Arc::new(FakeScopedStream::default());
    let tracks = vec![ScopedAttachment {
        id: "s1".into(),
        url: "http://127.0.0.1:9/s1.vtt".into(),
        label: "Scoped 1".into(),
        lang: None,
        default: true,
        audio: false,
    }];
    let pb = scoped_playback(stream, tracks.clone());
    machine.begin_load(
        5,
        identity("iris-plugin://p/s/5", Kind::Video),
        false,
        OpeningTransport::PluginScoped {
            playback: pb,
            pending_tracks: tracks,
        },
        false,
    );
    machine.reduce_event(&SessionEvent::Loaded, EnhancementView::default());
    assert!(machine.source.has_in_flight_scoped_adds());

    // Non-Unicode subtitle paths must fail immediately with NonUnicodePath even while
    // in-flight scoped additions would otherwise defer execution (PR #20 follow-up #4).
    let bad_err = machine
        .queue_or_emit_local_subtitle(&non_unicode_path(), false, false)
        .unwrap_err();
    assert_eq!(
        bad_err.downcast_ref::<PlayerActionError>(),
        Some(&PlayerActionError::NonUnicodePath)
    );
    assert!(!machine.source.has_deferred_local_subs());

    // Valid UTF-8 path defers while receipt 1 is in flight.
    let deferred = machine
        .queue_or_emit_local_subtitle(Path::new("/subs/user.srt"), false, false)
        .unwrap();
    assert!(deferred.is_empty());
    assert!(machine.source.has_deferred_local_subs());

    // Unrelated receipt 999 does not flush.
    let (unrelated_ops, _) = machine.reduce_event(
        &SessionEvent::CommandReply { id: 999, error: 0 },
        EnhancementView::default(),
    );
    assert!(unrelated_ops.is_empty());
    assert!(machine.source.has_deferred_local_subs());

    // Completing receipt 1 flushes deferred_local_subs and restores scoped attachment policy.
    let (flush_ops, _) = machine.reduce_event(
        &SessionEvent::CommandReply { id: 1, error: 0 },
        EnhancementView::default(),
    );
    assert!(!machine.source.has_deferred_local_subs());
    assert_eq!(
        flush_ops,
        vec![
            MpvOp::DisableHlsPolicy,
            MpvOp::cmd_sync(&["sub-add", "/subs/user.srt", "select"]),
            MpvOp::RestoreAttachmentPolicy,
            MpvOp::cmd_async(&["set", "sub-visibility", "yes"]),
        ]
    );
}

#[test]
fn ab_loop_failure_snapshot_and_event_batch_coalescing_preserve_invariants() {
    let mut machine = PlayerMachine::new(CommandReceipts::new());
    machine.user_hwdec = Hwdec::AutoSafe;
    machine.on_eof = OnEof::Loop;
    assert_eq!(machine.loop_value(false), "inf");
    assert_eq!(machine.loop_value(true), "no");

    // Unloaded A-B loop is rejected with InvalidAbRange.
    let err = machine
        .reduce_pure_command(&Command::SetAbLoop {
            start: 1.0,
            end: 5.0,
        })
        .unwrap()
        .unwrap_err();
    assert_eq!(
        err.downcast_ref::<PlayerActionError>(),
        Some(&PlayerActionError::InvalidAbRange)
    );

    let stream = Arc::new(FakeScopedStream::default());
    let pb = scoped_playback(stream, Vec::new());
    machine.begin_load(
        9,
        identity("iris-plugin://p/s/9", Kind::Video),
        false,
        OpeningTransport::PluginScoped {
            playback: pb,
            pending_tracks: Vec::new(),
        },
        false,
    );
    machine.reduce_event(&SessionEvent::Loaded, EnhancementView::default());
    machine.reduce_event(&SessionEvent::Duration(10.0), EnhancementView::default());

    let ab_ops = machine
        .reduce_pure_command(&Command::SetAbLoop {
            start: 2.0,
            end: 6.0,
        })
        .unwrap()
        .unwrap();
    assert_eq!(ab_ops.len(), 4);
    assert!(matches!(
        &machine.source,
        MediaSourceState::Loaded {
            transport: LoadedTransport::PluginScoped { .. },
            ..
        }
    ));

    // Refresh ops clear dirty flags atomically.
    let refresh = machine.take_deferred_refresh_ops();
    assert_eq!(refresh, vec![MpvOp::RefreshTracks, MpvOp::RefreshChapters]);
    assert!(machine.take_deferred_refresh_ops().is_empty());

    // Error preserves live PluginScoped stream inside MediaSourceState::Failed.
    machine.transition_to_failed(Default::default());
    assert!(!machine.state.loaded);
    assert!(matches!(
        &machine.source,
        MediaSourceState::Failed {
            transport: FailedTransport::PluginScoped { .. },
            ..
        }
    ));

    // Coalescing collapses intermediate Position updates within a segment while keeping Seek barrier order.
    let coalesced = coalesce_updates(vec![
        SessionUpdate {
            playlist_entry_id: Some(9),
            event: SessionEvent::Position(1.0),
        },
        SessionUpdate {
            playlist_entry_id: Some(9),
            event: SessionEvent::Duration(10.0),
        },
        SessionUpdate {
            playlist_entry_id: Some(9),
            event: SessionEvent::Position(1.2),
        },
        SessionUpdate {
            playlist_entry_id: Some(9),
            event: SessionEvent::Seek,
        },
        SessionUpdate {
            playlist_entry_id: Some(9),
            event: SessionEvent::Position(5.0),
        },
        SessionUpdate {
            playlist_entry_id: Some(9),
            event: SessionEvent::Position(5.1),
        },
    ]);
    assert_eq!(coalesced.len(), 4);
    assert!(matches!(coalesced[0].event, SessionEvent::Duration(10.0)));
    assert!(matches!(coalesced[1].event, SessionEvent::Position(1.2)));
    assert!(matches!(coalesced[2].event, SessionEvent::Seek));
    assert!(matches!(coalesced[3].event, SessionEvent::Position(5.1)));
}
