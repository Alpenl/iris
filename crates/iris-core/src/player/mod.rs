//! Viewport-owned playback session, structured `loadfile_with_options` dispatch,
//! and explicit `PlayerMachine` state transitions.

mod backend;
mod enhance;
mod events;
mod inspect;
mod machine;
mod model;
mod prepare;
mod recovery;
mod relay;
mod scoped;

use std::cell::RefCell;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use futures::channel::mpsc::UnboundedReceiver;
use iris_mpv::{Mpv, RenderHandle};
use machine::{
    FailedEnhancements, MediaIdentity, MediaSourceState, OpeningTransport, PlayerMachine,
};
use relay::PlaybackRelay;
use serde_json::json;

use crate::catalog::LocalDirectory;
use crate::library::Kind;
use crate::logging;
use crate::media_uri::MediaUri;
use crate::photo::Photo;
use crate::settings::Settings;
use iris_net::http_client::{origin_headers, validate_stream_url};

pub use enhance::ExternalTrack;
pub use machine::coalesce_updates;
pub use model::{
    AspectMode, AudioDevice, BackendDiagnostics, Chapter, Command, MAX_SPEED, MIN_SPEED,
    MediaStats, PlaybackState, PlayerActionError, RemoteMediaRequest, RemoteSubtitle, SPEED_STEPS,
    SessionEvent, SessionUpdate, Track, TrackKind, format_time, step_speed,
};
pub use prepare::{Prepared, PreparedQueue, prepare, prepare_queue};
pub use recovery::{ActiveVideoTrack, VideoRecoveryHandle};
#[doc(hidden)]
pub use relay::fuzz_hls;
pub use scoped::{ScopedAttachment, ScopedPlayback, ScopedStream, StreamProfile};

/// Validated CPU preview LUT filter provider (decoupled from plugin file/artifact internals).
pub trait MonitorLutFilter: Send + Sync {
    fn filter_spec(&self) -> Result<String>;
}

/// Session-scoped monotonic command receipt generator (no process-wide static).
#[derive(Clone)]
struct CommandReceipts(Arc<AtomicU64>);

impl CommandReceipts {
    fn new() -> Self {
        Self(Arc::new(AtomicU64::new(1)))
    }

    fn next(&self) -> Result<u64> {
        self.0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| anyhow!("exhausted mpv command receipt identifiers"))
    }
}

/// mpv only takes UTF-8 paths. A lossy conversion would name a different file, and
/// for `screenshot-to-file` it could overwrite one.
fn mpv_path(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| PlayerActionError::NonUnicodePath.into())
}

/// Active plugin-attached enhancements owned for the current media lifecycle.
struct PluginEnhancements {
    tracks: enhance::TrackState,
    timeline_owner: Option<String>,
    monitor_lut: Option<Arc<dyn MonitorLutFilter>>,
    retired_luts: Vec<(u64, Arc<dyn MonitorLutFilter>)>,
    loading_luts: Vec<Arc<dyn MonitorLutFilter>>,
}

impl PluginEnhancements {
    fn new(receipts: CommandReceipts) -> Self {
        Self {
            tracks: enhance::TrackState::new(receipts),
            timeline_owner: None,
            monitor_lut: None,
            retired_luts: Vec::new(),
            loading_luts: Vec::new(),
        }
    }
}

/// Visual orientation and subtitle safe-area state for the active viewport.
#[derive(Default)]
struct VisualTransform {
    extra_rotate: u16,
    source_rotation: u16,
    subtitle_inset: f64,
}

pub struct Session {
    mpv: Arc<Mpv>,
    event_pump: backend::EventPump,
    event_rx: RefCell<Option<UnboundedReceiver<SessionUpdate>>>,
    receipts: CommandReceipts,
    relay_budget: relay::RelayWorkerBudget,
    video_recovery: VideoRecoveryHandle,
    machine: PlayerMachine,
    enhancements: PluginEnhancements,
    visual: VisualTransform,
}

impl Session {
    pub fn new() -> Result<Self> {
        Self::from_mpv(backend::create()?)
    }

    /// Creates a headless playback session (`vo=null`, `ao=null`) for integration tests.
    pub fn new_headless(paused: bool) -> Result<Self> {
        let mut session = Self::from_mpv(backend::create_headless(paused)?)?;
        session.machine.state.paused = paused;
        Ok(session)
    }

    fn from_mpv(mpv: Arc<Mpv>) -> Result<Self> {
        let video_recovery = VideoRecoveryHandle::new(mpv.clone());
        let (event_pump, event_rx) =
            backend::spawn_event_pump(mpv.clone(), video_recovery.clone())?;
        let receipts = CommandReceipts::new();
        Ok(Self {
            mpv,
            event_pump,
            event_rx: RefCell::new(Some(event_rx)),
            machine: PlayerMachine::new(receipts.clone()),
            enhancements: PluginEnhancements::new(receipts.clone()),
            receipts,
            relay_budget: relay::RelayWorkerBudget::default(),
            video_recovery,
            visual: VisualTransform::default(),
        })
    }

    #[allow(clippy::disallowed_methods)]
    pub fn render_handle(&self) -> RenderHandle {
        RenderHandle::new(self.mpv.clone())
    }

    #[allow(clippy::disallowed_methods)]
    pub fn video_recovery(&self) -> VideoRecoveryHandle {
        self.video_recovery.clone()
    }

    /// Takes the session's single event receiver.
    pub fn events(&self) -> Option<UnboundedReceiver<SessionUpdate>> {
        self.event_rx.borrow_mut().take()
    }

    pub fn state(&self) -> &PlaybackState {
        &self.machine.state
    }

    pub fn current(&self) -> Option<&Path> {
        self.machine.current_path()
    }

    pub fn current_uri(&self) -> Option<&str> {
        self.machine.current_uri()
    }

    pub fn current_media_uri(&self) -> Option<MediaUri> {
        self.machine.current_media_uri()
    }

    pub fn current_title(&self) -> Option<&str> {
        self.machine.current_title()
    }

    pub fn set_private_identity(&mut self, private: bool) {
        self.machine.set_private_identity(private);
    }

    pub fn may_share_current_identity(&self) -> bool {
        !self.machine.private_identity()
            && self
                .current_media_uri()
                .is_some_and(|u| u.may_share_identity())
    }

    pub fn kind(&self) -> Option<Kind> {
        self.machine.kind()
    }

    pub fn is_actively_playing_video(&self) -> bool {
        self.kind() == Some(Kind::Video)
            && self.machine.state.loaded
            && !self.machine.state.paused
            && !self.machine.state.ended
    }

    pub fn stop(&mut self) -> Result<()> {
        let ended_now = self.machine.state.ended
            || self
                .mpv
                .get_property_string("eof-reached")
                .is_ok_and(|s| s == "yes");
        self.execute(Command::ClearAbLoop)?;
        let stopped_lut = self.enhancements.monitor_lut.take();
        let loading_luts = std::mem::take(&mut self.enhancements.loading_luts);
        if stopped_lut.is_some() || !loading_luts.is_empty() {
            if matches!(self.kind(), Some(Kind::Video | Kind::Photo)) {
                Self::log_mpv_err(
                    "restore hwdec",
                    self.mpv
                        .command_async(&["set", "hwdec", self.machine.user_hwdec.mpv_value()]),
                );
            }
            Self::log_mpv_err("reset vf", self.mpv.command_async(&["set", "vf", ""]));
        }
        self.enhancements.tracks.stopping();
        let need_stop_receipt = stopped_lut.is_some()
            || !loading_luts.is_empty()
            || self.enhancements.tracks.has_retired_or_pending();
        if need_stop_receipt && let Ok(receipt) = self.receipts.next() {
            self.mpv.command_with_reply(receipt, &["stop"])?;
            if let Some(lut) = stopped_lut {
                self.enhancements.retired_luts.push((receipt, lut));
            }
            for lut in loading_luts {
                self.enhancements.retired_luts.push((receipt, lut));
            }
            if self.enhancements.tracks.has_retired_or_pending() {
                self.enhancements.tracks.track_stop(receipt);
            }
        } else {
            self.mpv.command_async(&["stop"])?;
            self.enhancements.tracks.clear();
        }
        self.machine.transition_to_idle(ended_now);
        self.event_pump.set_relay(Default::default());
        self.event_pump.set_scoped(scoped::empty_scoped());
        self.enhancements.timeline_owner = None;
        self.visual.extra_rotate = 0;
        self.visual.source_rotation = 0;
        Ok(())
    }

    fn should_unpause_after_eof(&self) -> bool {
        self.machine.source.eof_paused()
            || self.machine.state.ended
            || self
                .mpv
                .get_property_string("eof-reached")
                .is_ok_and(|s| s == "yes")
    }

    fn prepare_enhancements_for_load(&mut self) {
        if let Some(old) = self.enhancements.monitor_lut.take() {
            self.enhancements.loading_luts.push(old);
        }
        self.enhancements.tracks.loading();
        self.enhancements.timeline_owner = None;
        self.visual.extra_rotate = 0;
        self.visual.source_rotation = 0;
    }

    fn snapshot_failed_enhancements(&mut self) -> FailedEnhancements {
        let lut = self.enhancements.monitor_lut.as_ref().map(Arc::downgrade);
        let tracks = self.enhancements.tracks.snapshot_wanted_weak();
        let timeline = self.enhancements.timeline_owner.take().map(|owner| {
            let chapters = std::mem::take(&mut self.machine.state.chapters);
            (owner, chapters)
        });
        FailedEnhancements {
            lut,
            tracks,
            timeline,
        }
    }

    pub fn open(&mut self, prepared: Prepared, resume: f64) -> Result<LocalDirectory> {
        let Prepared {
            folder,
            entries,
            subfolders,
            target,
            kind,
            photo,
        } = prepared;
        let dir = LocalDirectory {
            folder: Some(folder),
            entries,
            subfolders,
        };
        match target {
            Some(path) => self.load(&path, kind, photo, resume)?,
            None => self.stop()?,
        }
        Ok(dir)
    }

    pub fn open_prepared_queue(
        &mut self,
        prepared: PreparedQueue,
        resume: f64,
    ) -> Result<LocalDirectory> {
        let PreparedQueue {
            folder,
            entries,
            photo,
        } = prepared;
        let first = entries.first().map(|e| (e.path.clone(), e.kind));
        let dir = LocalDirectory {
            folder: Some(folder),
            entries,
            subfolders: Vec::new(),
        };
        if let Some((first_path, first_kind)) = first {
            self.load(&first_path, Some(first_kind), photo, resume)?;
        }
        Ok(dir)
    }

    /// Opens HTTP(S) media through a cancellable, source-scoped relay.
    pub fn open_remote(
        &mut self,
        req: RemoteMediaRequest,
        settings: &Settings,
        resume: f64,
    ) -> Result<()> {
        self.load_remote(
            req,
            settings,
            if settings.resume_playback { resume } else { 0. },
        )
    }

    pub(super) fn load_remote(
        &mut self,
        req: RemoteMediaRequest,
        settings: &Settings,
        resume: f64,
    ) -> Result<()> {
        let saved_remote = req.clone();
        let main_url = validate_stream_url(&req.playback_url)?;
        let clean_headers = origin_headers(&main_url, &req.headers)?;
        let relay =
            PlaybackRelay::start_with_budget(main_url.clone(), clean_headers, &self.relay_budget)?;
        let playback_url = relay.url(&main_url)?;
        let mut owned_opts: Vec<(&'static str, String)> = Vec::new();
        owned_opts.push(("autoload-files", "no".into()));
        owned_opts.push(("stream-lavf-o", "reconnect_on_http_error=503".into()));
        if let Some(photo) = &req.photo {
            owned_opts.extend(photo.mpv_options());
        } else if !req.is_live
            && matches!(req.kind, Kind::Video | Kind::Audio)
            && resume.is_finite()
            && resume > 0.
        {
            owned_opts.push(("start", format!("+{resume:.3}")));
        }

        let mut pending_subs = Vec::new();
        for sub in req.subtitles {
            match sub {
                RemoteSubtitle::Url(raw_sub) => {
                    let sub_url = validate_stream_url(&raw_sub)?;
                    pending_subs.push(relay.subtitle_url(&sub_url)?);
                }
                RemoteSubtitle::InlineText(text) => {
                    pending_subs.push(format!("memory://{}", model::subtitle_text(&text)?));
                }
            }
        }

        let unpause_after_eof = self.should_unpause_after_eof();
        self.reset_backend_for_load()?;
        self.machine.on_eof = settings.on_eof;
        owned_opts.push(("loop-file", self.machine.loop_value(req.is_live).into()));
        let opt_refs: Vec<_> = owned_opts.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let previous = self.event_pump.set_relay(Arc::downgrade(&relay.service()));
        let previous_scoped = self.event_pump.set_scoped(scoped::empty_scoped());
        let entry_id = match self
            .mpv
            .loadfile_with_options(&playback_url, "replace", &opt_refs)
        {
            Ok(id) => id,
            Err(err) => {
                self.event_pump.set_relay(previous);
                self.event_pump.set_scoped(previous_scoped);
                return Err(err.into());
            }
        };
        self.prepare_enhancements_for_load();
        let ops = self.machine.begin_load(
            entry_id,
            MediaIdentity {
                local_path: None,
                uri: req.uri.to_uri_string(),
                title: Some(req.title),
                private_identity: false,
                kind: req.kind,
                photo: req.photo,
            },
            req.is_live,
            OpeningTransport::DirectRemote {
                relay,
                last_remote: Box::new(saved_remote),
                pending_subs,
            },
            unpause_after_eof,
        );
        let _ = self.run_ops(ops);
        logging::event(
            logging::EventLevel::Info,
            "player",
            "media_open",
            json!({
                "source": "remote",
                "kind": format!("{:?}", req.kind),
                "host": logging::url_host(main_url.as_str()),
                "is_live": req.is_live,
                "has_resume": resume > 0.,
            }),
        );
        Ok(())
    }

    pub(super) fn load(
        &mut self,
        path: &Path,
        kind: Option<Kind>,
        photo: Option<Photo>,
        resume: f64,
    ) -> Result<()> {
        let mpv_path = mpv_path(path)?;
        let canonical_uri = MediaUri::Local(path.to_path_buf()).to_uri_string();
        let kind = kind.unwrap_or(Kind::Video);
        let mut owned_opts: Vec<(&'static str, String)> = Vec::new();
        if let Some(p) = &photo {
            owned_opts.extend(p.mpv_options());
        } else if matches!(kind, Kind::Video | Kind::Audio) && resume.is_finite() && resume > 0. {
            owned_opts.push(("start", format!("{resume:.3}")));
        }

        let unpause_after_eof = self.should_unpause_after_eof();
        self.reset_backend_for_load()?;
        owned_opts.push(("loop-file", self.machine.loop_value(false).into()));
        let opt_refs: Vec<(&str, &str)> =
            owned_opts.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let entry_id = self
            .mpv
            .loadfile_with_options(mpv_path, "replace", &opt_refs)?;
        self.event_pump.set_relay(Default::default());
        self.event_pump.set_scoped(scoped::empty_scoped());
        self.prepare_enhancements_for_load();
        let ops = self.machine.begin_load(
            entry_id,
            MediaIdentity {
                local_path: Some(path.to_path_buf()),
                uri: canonical_uri,
                title: None,
                private_identity: false,
                kind,
                photo,
            },
            false,
            OpeningTransport::Local,
            unpause_after_eof,
        );
        let _ = self.run_ops(ops);
        logging::event(
            logging::EventLevel::Info,
            "player",
            "media_open",
            json!({
                "source": "local",
                "kind": format!("{kind:?}"),
                "is_live": false,
                "has_resume": resume > 0.,
            }),
        );
        Ok(())
    }

    pub fn progress(&self) -> Option<(&Path, f64, f64, bool)> {
        let path = self.current()?;
        (matches!(self.kind(), Some(Kind::Video | Kind::Audio)) && self.machine.state.loaded)
            .then_some((
                path,
                self.machine.state.time_pos,
                self.machine.state.duration,
                self.machine.state.ended,
            ))
    }

    pub fn uri_progress(&self) -> Option<(&str, f64, f64, bool)> {
        let uri = self.current_uri()?;
        (!self.machine.private_identity()
            && !self.machine.state.is_live
            && matches!(self.kind(), Some(Kind::Video | Kind::Audio))
            && self.machine.state.loaded)
            .then_some((
                uri,
                self.machine.state.time_pos,
                self.machine.state.duration,
                self.machine.state.ended,
            ))
    }

    /// Applies current-playback and session-wide events, returning the event
    /// only when the UI should act on it.
    pub fn apply_update<'a>(&mut self, update: &'a SessionUpdate) -> Option<&'a SessionEvent> {
        if let Some(entry_id) = update.playlist_entry_id
            && self.machine.source.entry_id() != Some(entry_id)
        {
            return None;
        }
        self.apply(&update.event);
        Some(&update.event)
    }

    pub fn flush_deferred(&mut self) {
        let ops = self.machine.take_deferred_refresh_ops();
        let _ = self.run_ops(ops);
    }

    #[cfg(any(debug_assertions, feature = "devtools"))]
    pub fn debug_property(&self, name: &str) -> Result<String> {
        Ok(self.mpv.get_property_string(name)?)
    }

    #[cfg(any(debug_assertions, feature = "devtools"))]
    pub fn debug_command(&self, args: &[&str]) -> Result<()> {
        Ok(self.mpv.command(args)?)
    }

    pub fn shutdown(&self) {
        self.machine.source.stop_active_transports();
        self.event_pump.stop(&self.mpv);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::library::Kind;
    use crate::media_uri::MediaUri;

    use super::machine::{LoadedTransport, MediaIdentity, OpeningTransport};
    use super::{
        MediaSourceState, PlaybackRelay, RemoteMediaRequest, Session, SessionEvent, SessionUpdate,
    };

    #[test]
    fn stale_playback_updates_do_not_change_state_or_consume_subtitles() {
        let mut session = Session::new().unwrap();
        let url = url::Url::parse("https://example.invalid/video.mp4").unwrap();
        let relay = PlaybackRelay::start(url, BTreeMap::new()).unwrap();
        session.machine.begin_load(
            2,
            MediaIdentity {
                local_path: None,
                uri: "https://example.invalid/video.mp4".into(),
                title: Some("video.mp4".into()),
                private_identity: false,
                kind: Kind::Video,
                photo: None,
            },
            false,
            OpeningTransport::DirectRemote {
                relay,
                last_remote: Box::new(RemoteMediaRequest {
                    uri: MediaUri::HttpUrl("https://example.invalid/video.mp4".into()),
                    playback_url: "https://example.invalid/video.mp4".into(),
                    title: "video.mp4".into(),
                    kind: Kind::Video,
                    headers: BTreeMap::new(),
                    subtitles: Vec::new(),
                    is_live: false,
                    photo: None,
                }),
                pending_subs: vec!["memory://WEBVTT\n".into()],
            },
            false,
        );
        for event in [
            SessionEvent::Ended(true),
            SessionEvent::Loaded,
            SessionEvent::Error(-13),
        ] {
            assert!(
                session
                    .apply_update(&SessionUpdate {
                        playlist_entry_id: Some(1),
                        event
                    })
                    .is_none()
            );
        }
        assert!(!session.state().ended);
        assert!(!session.state().loaded);
        assert!(matches!(
            &session.machine.source,
            MediaSourceState::Opening {
                transport: OpeningTransport::DirectRemote { pending_subs, .. },
                ..
            } if pending_subs.len() == 1
        ));
        let update = SessionUpdate {
            playlist_entry_id: Some(2),
            event: SessionEvent::Loaded,
        };
        assert!(matches!(
            session.apply_update(&update),
            Some(SessionEvent::Loaded)
        ));
        assert!(session.state().loaded);
        assert!(matches!(
            &session.machine.source,
            MediaSourceState::Loaded {
                transport: LoadedTransport::DirectRemote { .. },
                ..
            }
        ));
    }

    #[test]
    fn switching_between_same_resolution_videos_preserves_video_size() {
        use std::time::{Duration, Instant};

        let mut session = Session::new().unwrap();
        session.mpv.set_property_string("vo", "null").unwrap();
        session.mpv.set_property_string("ao", "null").unwrap();
        let mut events = session.events().unwrap();
        let clip = format!(
            "memory://YUV4MPEG2 W16 H16 F10:1 Ip A1:1 C420jpeg\n{}",
            format!("FRAME\n{}", "x".repeat(384)).repeat(30)
        );
        for _ in 0..2 {
            let entry_id = session
                .mpv
                .loadfile_with_options(&clip, "replace", &[("pause", "yes")])
                .unwrap();
            session.prepare_enhancements_for_load();
            session.machine.begin_load(
                entry_id,
                MediaIdentity {
                    local_path: None,
                    uri: "memory://clip".into(),
                    title: None,
                    private_identity: false,
                    kind: Kind::Video,
                    photo: None,
                },
                false,
                OpeningTransport::Local,
                false,
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while !session.state().loaded {
                assert!(Instant::now() < deadline, "video load timed out");
                match events.try_recv() {
                    Ok(update) => {
                        session.apply_update(&update);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
            assert_eq!(session.state().video_size, (16, 16));
        }
        session.shutdown();
    }
}
