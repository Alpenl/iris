//! Viewport-owned playback session, structured `loadfile_with_options` dispatch,
//! and encapsulated playback state transitions.

mod backend;
mod enhance;
mod inspect;
mod model;
mod prepare;
mod relay;
mod scoped;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use futures::channel::mpsc::UnboundedReceiver;
use iris_mpv::{Mpv, RenderHandle};
use relay::PlaybackRelay;
use serde_json::json;

use crate::catalog::LocalDirectory;
use crate::library::Kind;
use crate::logging;
use crate::media_uri::MediaUri;
use crate::photo::Photo;
use crate::settings::{Hwdec, OnEof, Settings};
use crate::vfs::{self, http_client::origin_headers};

pub use enhance::ExternalTrack;
pub use model::{
    AspectMode, AudioDevice, Chapter, Command, MAX_SPEED, MIN_SPEED, MediaStats, PlaybackState,
    PlayerActionError, RemoteMediaRequest, RemoteSubtitle, SPEED_STEPS, SessionEvent,
    SessionUpdate, Track, TrackKind, format_time, step_speed,
};
pub use prepare::{Prepared, PreparedQueue, prepare, prepare_queue};
pub use scoped::{ScopedAttachment, ScopedPlayback, ScopedStream, StreamProfile};

/// Validated CPU preview LUT filter provider (decoupled from plugin file/artifact internals).
pub trait MonitorLutFilter: Send + Sync {
    fn filter_spec(&self) -> Result<String>;
}

static NEXT_COMMAND_RECEIPT: AtomicU64 = AtomicU64::new(1);

pub(super) fn next_command_receipt() -> Result<u64> {
    NEXT_COMMAND_RECEIPT
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map_err(|_| anyhow!("exhausted mpv command receipt identifiers"))
}

/// Active media transport state; mutually exclusive across local, direct remote, and plugin-scoped loads.
enum MediaSourceState {
    Idle,
    Local,
    DirectRemote {
        relay: PlaybackRelay,
        last_remote: RemoteMediaRequest,
        pending_subs: Vec<String>,
    },
    PluginScoped {
        playback: Arc<ScopedPlayback>,
        pending_tracks: Vec<ScopedAttachment>,
    },
}

/// Active plugin-attached enhancements owned for the current media lifecycle.
#[derive(Default)]
struct PluginEnhancements {
    tracks: enhance::TrackState,
    timeline_owner: Option<String>,
    monitor_lut: Option<Arc<dyn MonitorLutFilter>>,
    retired_lut: Option<(u64, Arc<dyn MonitorLutFilter>)>,
}

/// Visual orientation and subtitle safe-area state for the active viewport.
#[derive(Default)]
struct VisualTransform {
    extra_rotate: u16,
    source_rotation: u16,
    subtitle_inset: f64,
}

pub struct Session {
    pub(super) mpv: Arc<Mpv>,
    event_pump: backend::EventPump,
    event_rx: RefCell<Option<UnboundedReceiver<SessionUpdate>>>,
    pub(super) state: PlaybackState,
    pub(super) current: Option<PathBuf>,
    pub(super) current_uri: Option<String>,
    pub(super) current_title: Option<String>,
    pub(super) private_identity: bool,
    pub(super) kind: Option<Kind>,
    pub(super) photo_meta: Option<Photo>,
    playlist_entry_id: Option<i64>,
    source: MediaSourceState,
    enhancements: PluginEnhancements,
    visual: VisualTransform,
    pub(super) user_hwdec: Cell<Hwdec>,
    on_eof: Cell<OnEof>,
    pub(super) tracks_dirty: bool,
    pub(super) chapters_dirty: bool,
}

impl Session {
    pub fn new() -> Result<Self> {
        let mpv = backend::create()?;
        let (event_pump, event_rx) = backend::spawn_event_pump(mpv.clone())?;
        Ok(Self {
            mpv,
            event_pump,
            event_rx: RefCell::new(Some(event_rx)),
            state: PlaybackState::default(),
            current: None,
            current_uri: None,
            current_title: None,
            private_identity: false,
            kind: None,
            photo_meta: None,
            playlist_entry_id: None,
            source: MediaSourceState::Idle,
            enhancements: PluginEnhancements::default(),
            visual: VisualTransform::default(),
            user_hwdec: Cell::new(Hwdec::AutoSafe),
            on_eof: Cell::new(OnEof::Pause),
            tracks_dirty: false,
            chapters_dirty: false,
        })
    }

    pub fn render_handle(&self) -> RenderHandle {
        RenderHandle::new(self.mpv.clone())
    }

    /// Takes the session's single event receiver.
    pub fn events(&self) -> Option<UnboundedReceiver<SessionUpdate>> {
        self.event_rx.borrow_mut().take()
    }

    pub fn state(&self) -> &PlaybackState {
        &self.state
    }

    pub fn current(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    pub fn current_uri(&self) -> Option<&str> {
        self.current_uri.as_deref()
    }

    pub fn current_media_uri(&self) -> Option<MediaUri> {
        if let Some(u) = &self.current_uri {
            Some(MediaUri::parse(u))
        } else {
            self.current.clone().map(MediaUri::Local)
        }
    }

    pub fn current_title(&self) -> Option<&str> {
        self.current_title.as_deref()
    }

    pub fn set_private_identity(&mut self, private: bool) {
        self.private_identity = private;
    }

    pub fn may_share_current_identity(&self) -> bool {
        !self.private_identity
            && self
                .current_media_uri()
                .is_some_and(|u| u.may_share_identity())
    }

    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }

    pub fn is_actively_playing_video(&self) -> bool {
        self.kind == Some(Kind::Video)
            && self.state.loaded
            && !self.state.paused
            && !self.state.ended
    }

    pub fn stop(&mut self) -> Result<()> {
        self.execute(Command::ClearAbLoop)?;
        self.set_monitor_lut(None)?;
        self.mpv.command_async(&["stop"])?;
        self.clear_media_source();
        self.enhancements.tracks.clear();
        self.enhancements.timeline_owner = None;
        self.current = None;
        self.current_uri = None;
        self.current_title = None;
        self.private_identity = false;
        self.playlist_entry_id = None;
        self.kind = None;
        self.photo_meta = None;
        self.visual.extra_rotate = 0;
        self.state.photo_colour = None;
        self.state.photo_exif = None;
        self.state.tracks.clear();
        self.state.chapters.clear();
        self.state.loaded = false;
        self.state.ended = false;
        self.state.buffering = false;
        self.state.cache_duration = 0.;
        self.state.is_live = false;
        self.tracks_dirty = false;
        self.chapters_dirty = false;
        Ok(())
    }

    fn clear_media_source(&mut self) {
        match std::mem::replace(&mut self.source, MediaSourceState::Idle) {
            MediaSourceState::PluginScoped { playback, .. } => playback.stream.stop(),
            MediaSourceState::DirectRemote { relay, .. } => relay.cancel(),
            MediaSourceState::Idle | MediaSourceState::Local => {}
        }
        self.event_pump.set_relay(Default::default());
        self.event_pump.set_scoped(scoped::empty_scoped());
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

    #[allow(clippy::too_many_arguments)]
    pub(super) fn reset_for_load(
        &mut self,
        current: Option<PathBuf>,
        current_uri: String,
        current_title: Option<String>,
        playlist_entry_id: i64,
        kind: Kind,
        is_live: bool,
        photo: Option<Photo>,
    ) {
        self.current = current;
        self.current_uri = Some(current_uri);
        self.current_title = current_title;
        self.private_identity = false;
        self.playlist_entry_id = Some(playlist_entry_id);
        self.kind = Some(kind);
        self.state.is_live = is_live;
        self.state.ab_start = None;
        self.state.ab_end = None;
        self.enhancements.monitor_lut = None;
        self.enhancements.retired_lut = None;
        self.enhancements.tracks.loading();
        self.enhancements.timeline_owner = None;
        self.state.buffering = false;
        self.state.cache_duration = 0.;
        self.state.photo_colour = photo
            .as_ref()
            .and_then(|p| p.colour)
            .map(|c| c.display_name());
        self.state.photo_exif = photo.as_ref().and_then(|p| p.exif.clone());
        self.photo_meta = photo;
        self.visual.extra_rotate = 0;
        self.visual.source_rotation = 0;
        self.state.aspect = AspectMode::Original;
        self.state.hflip = false;
        self.state.vflip = false;
        self.state.sub_delay = 0.;
        self.state.ended = false;
        self.state.loaded = false;
        self.state.time_pos = 0.;
        self.state.duration = 0.;
        self.state.video_size = (0, 0);
        self.state.rotation = 0;
        self.state.tracks.clear();
        self.state.chapters.clear();
        self.state.current_chapter = -1;
        self.tracks_dirty = false;
        self.chapters_dirty = false;
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
        let main_url = vfs::validate_stream_url(&req.playback_url)?;
        let clean_headers = origin_headers(&main_url, &req.headers);
        let relay = PlaybackRelay::start(main_url.clone(), clean_headers)?;
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
                    let sub_url = vfs::validate_stream_url(&raw_sub)?;
                    pending_subs.push(relay.subtitle_url(&sub_url)?);
                }
                RemoteSubtitle::InlineText(text) => {
                    pending_subs.push(format!("memory://{}", model::subtitle_text(&text)?));
                }
            }
        }

        self.reset_backend_for_load()?;
        self.on_eof.set(settings.on_eof);
        owned_opts.push(("loop-file", self.loop_value(req.is_live).into()));
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
        if let MediaSourceState::PluginScoped { playback, .. } = std::mem::replace(
            &mut self.source,
            MediaSourceState::DirectRemote {
                relay,
                last_remote: saved_remote,
                pending_subs,
            },
        ) {
            playback.stream.stop();
        }
        self.reset_for_load(
            None,
            req.uri.to_uri_string(),
            Some(req.title),
            entry_id,
            req.kind,
            req.is_live,
            req.photo,
        );
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
        let mpv_path = path.to_string_lossy().into_owned();
        let canonical_uri = MediaUri::Local(path.to_path_buf()).to_uri_string();
        let kind = kind.unwrap_or(Kind::Video);
        let mut owned_opts: Vec<(&'static str, String)> = Vec::new();
        if let Some(p) = &photo {
            owned_opts.extend(p.mpv_options());
        } else if matches!(kind, Kind::Video | Kind::Audio) && resume.is_finite() && resume > 0. {
            owned_opts.push(("start", format!("{resume:.3}")));
        }

        self.reset_backend_for_load()?;
        owned_opts.push(("loop-file", self.loop_value(false).into()));
        let opt_refs: Vec<(&str, &str)> =
            owned_opts.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let entry_id = self
            .mpv
            .loadfile_with_options(&mpv_path, "replace", &opt_refs)?;
        self.clear_media_source();
        self.source = MediaSourceState::Local;
        self.reset_for_load(
            Some(path.to_path_buf()),
            canonical_uri,
            None,
            entry_id,
            kind,
            false,
            photo,
        );
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
        let path = self.current.as_deref()?;
        (matches!(self.kind, Some(Kind::Video | Kind::Audio)) && self.state.loaded).then_some((
            path,
            self.state.time_pos,
            self.state.duration,
            self.state.ended,
        ))
    }

    pub fn uri_progress(&self) -> Option<(&str, f64, f64, bool)> {
        let uri = self.current_uri.as_deref()?;
        (!self.private_identity
            && !self.state.is_live
            && matches!(self.kind, Some(Kind::Video | Kind::Audio))
            && self.state.loaded)
            .then_some((
                uri,
                self.state.time_pos,
                self.state.duration,
                self.state.ended,
            ))
    }

    /// Applies current-playback and session-wide events, returning the event
    /// only when the UI should act on it.
    pub fn apply_update<'a>(&mut self, update: &'a SessionUpdate) -> Option<&'a SessionEvent> {
        if let Some(entry_id) = update.playlist_entry_id
            && self.playlist_entry_id != Some(entry_id)
        {
            return None;
        }
        self.apply(&update.event);
        Some(&update.event)
    }

    fn attach_pending_remote_subs(&mut self) {
        let MediaSourceState::DirectRemote { pending_subs, .. } = &mut self.source else {
            return;
        };
        for sub_mem in std::mem::take(pending_subs) {
            let _ = self.mpv.command_async(&["sub-add", &sub_mem, "auto"]);
        }
    }

    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Seek => {}
            SessionEvent::CommandReply { id, error } => {
                if self
                    .enhancements
                    .retired_lut
                    .as_ref()
                    .is_some_and(|(rid, _)| *rid == *id)
                {
                    self.enhancements.retired_lut = None;
                }
                self.enhancements.tracks.reply(&self.mpv, *id, *error);
                self.tracks_dirty = true;
            }
            SessionEvent::Position(v) => {
                self.state.time_pos = *v;
                self.update_plugin_chapter();
            }
            SessionEvent::Duration(v) => self.state.duration = *v,
            SessionEvent::Paused(v) => self.state.paused = *v,
            SessionEvent::Volume(v) => self.state.volume = *v,
            SessionEvent::Muted(v) => self.state.muted = *v,
            SessionEvent::Speed(v) => self.state.speed = *v,
            SessionEvent::Width(v) => self.state.video_size.0 = *v,
            SessionEvent::Height(v) => self.state.video_size.1 = *v,
            SessionEvent::Hardware(v) => {
                if self.state.hwdec != *v && self.state.loaded {
                    logging::event(
                        logging::EventLevel::Info,
                        "player",
                        "hwdec_changed",
                        json!({
                            "from": self.state.hwdec,
                            "to": v,
                        }),
                    );
                }
                self.state.hwdec.clone_from(v);
            }
            SessionEvent::Subtitles(v) => self.state.subs_visible = *v,
            SessionEvent::SubtitleTrack(v) => {
                if self.state.tracks.is_empty() {
                    self.tracks_dirty = true;
                } else {
                    let selected_id = v.parse::<i64>().ok();
                    for t in &mut self.state.tracks {
                        if t.kind == TrackKind::Sub {
                            t.selected = selected_id == Some(t.id);
                        }
                    }
                }
            }
            SessionEvent::SubDelay(v) => self.state.sub_delay = *v,
            SessionEvent::Ended(v) => self.state.ended = *v,
            SessionEvent::Audio(v) => {
                self.state.audio.clone_from(v);
                if self.state.tracks.is_empty() {
                    self.tracks_dirty = true;
                } else {
                    let selected_id = v.parse::<i64>().ok();
                    for t in &mut self.state.tracks {
                        if t.kind == TrackKind::Audio {
                            t.selected = selected_id == Some(t.id);
                        }
                    }
                }
            }
            SessionEvent::Rotation(v) => self.state.rotation = *v,
            SessionEvent::SourceRotation(v) => {
                self.visual.source_rotation = v.rem_euclid(360) as u16;
            }
            SessionEvent::LoopFile(v) => self.state.loop_file = *v,
            SessionEvent::AbStart(v) => self.state.ab_start = *v,
            SessionEvent::AbEnd(v) => self.state.ab_end = *v,
            SessionEvent::Chapter(v) => {
                if self.enhancements.timeline_owner.is_none() {
                    self.state.current_chapter = *v;
                }
            }
            SessionEvent::ChaptersChanged => self.chapters_dirty = true,
            SessionEvent::TracksChanged => self.tracks_dirty = true,
            SessionEvent::Buffering(v) => self.state.buffering = *v,
            SessionEvent::CacheDuration(v) => {
                self.state.cache_duration = if v.is_finite() { v.max(0.) } else { 0. };
            }
            SessionEvent::Loaded => {
                self.enhancements.tracks.loaded();
                self.state.loaded = true;
                self.sync_video_params();
                if self.state.duration <= 0.
                    && let Ok(v) = self.mpv.get_property_f64("duration")
                    && v.is_finite()
                    && v > 0.
                {
                    self.state.duration = v;
                }
                self.attach_scoped_tracks();
                self.attach_pending_remote_subs();
                self.tracks_dirty = true;
                self.chapters_dirty = true;
                let video_codec = self
                    .mpv
                    .get_property_string("video-codec")
                    .unwrap_or_default();
                let audio_codec = self
                    .mpv
                    .get_property_string("audio-codec-name")
                    .unwrap_or_default();
                logging::event(
                    logging::EventLevel::Info,
                    "player",
                    "media_loaded",
                    json!({
                        "kind": self.kind.map(|k| format!("{k:?}")),
                        "width": self.state.video_size.0,
                        "height": self.state.video_size.1,
                        "duration_s": self.state.duration,
                        "hwdec": self.state.hwdec,
                        "video_codec": video_codec,
                        "audio_codec": audio_codec,
                        "is_live": self.state.is_live,
                    }),
                );
            }
            SessionEvent::Reconfigured => self.sync_video_params(),
            SessionEvent::QueueOverflow => self.resync_after_overflow(),
            SessionEvent::Error(code) => {
                logging::event(
                    logging::EventLevel::Error,
                    "player",
                    "media_error",
                    json!({
                        "code": code,
                        "kind": self.kind.map(|k| format!("{k:?}")),
                        "time_pos_s": self.state.time_pos,
                        "duration_s": self.state.duration,
                        "hwdec": self.state.hwdec,
                        "is_live": self.state.is_live,
                    }),
                );
                self.clear_media_source();
                self.enhancements.tracks.clear();
                self.enhancements.timeline_owner = None;
                self.enhancements.retired_lut = None;
                self.state.loaded = false;
            }
        }
    }

    fn sync_video_params(&mut self) {
        let i64_prop = |name: &str| {
            self.mpv
                .get_property_string(name)
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
        };
        if let Some(w) = i64_prop("video-out-params/dw")
            && w > 0
        {
            self.state.video_size.0 = w;
        }
        if let Some(h) = i64_prop("video-out-params/dh")
            && h > 0
        {
            self.state.video_size.1 = h;
        }
        if let Some(r) = i64_prop("video-out-params/rotate") {
            self.state.rotation = r;
        }
        if let Some(r) = i64_prop("video-dec-params/rotate") {
            self.visual.source_rotation = r.rem_euclid(360) as u16;
        }
        if let Ok(hw) = self.mpv.get_property_string("hwdec-current") {
            self.state.hwdec = hw;
        }
    }

    fn resync_after_overflow(&mut self) {
        let active_id = backend::active_playlist_entry_id(&self.mpv);
        if self.playlist_entry_id.is_some() && active_id != self.playlist_entry_id {
            if self
                .mpv
                .get_property_string("idle-active")
                .is_ok_and(|s| s == "yes")
            {
                self.enhancements.tracks.clear();
                self.enhancements.timeline_owner = None;
                self.state.loaded = false;
            }
            return;
        }
        self.sync_video_params();
        let f64_prop = |name: &str| {
            self.mpv
                .get_property_f64(name)
                .ok()
                .filter(|v| v.is_finite())
        };
        let flag_prop = |name: &str| {
            self.mpv
                .get_property_string(name)
                .ok()
                .map(|s| s == "yes" || s == "true")
        };
        if let Some(v) = f64_prop("time-pos") {
            self.state.time_pos = v;
        }
        if let Some(v) = f64_prop("duration") {
            self.state.duration = v;
        }
        if let Some(v) = flag_prop("pause") {
            self.state.paused = v;
        }
        if let Some(v) = flag_prop("eof-reached") {
            self.state.ended = v;
        }
        if let Some(v) = f64_prop("volume") {
            self.state.volume = v;
        }
        if let Some(v) = flag_prop("mute") {
            self.state.muted = v;
        }
        if let Some(v) = f64_prop("speed") {
            self.state.speed = v;
        }
        if let Some(v) = flag_prop("paused-for-cache") {
            self.state.buffering = v;
        }
        if let Some(v) = f64_prop("demuxer-cache-duration") {
            self.state.cache_duration = v.max(0.);
        }
        self.state.ab_start = f64_prop("ab-loop-a").filter(|v| *v >= 0.);
        self.state.ab_end = f64_prop("ab-loop-b").filter(|v| *v >= 0.);
        if !self.state.loaded
            && active_id.is_some()
            && (self.state.duration > 0.
                || self.state.video_size.0 > 0
                || flag_prop("seeking") == Some(false))
        {
            self.enhancements.tracks.loaded();
            self.state.loaded = true;
            self.attach_scoped_tracks();
            self.attach_pending_remote_subs();
        }
        self.tracks_dirty = true;
        self.chapters_dirty = true;
    }

    pub fn flush_deferred(&mut self) {
        if self.tracks_dirty {
            self.tracks_dirty = false;
            self.refresh_tracks();
        }
        if self.chapters_dirty {
            self.chapters_dirty = false;
            self.refresh_chapters();
        }
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
        match &self.source {
            MediaSourceState::PluginScoped { playback, .. } => playback.stream.stop(),
            MediaSourceState::DirectRemote { relay, .. } => relay.cancel(),
            MediaSourceState::Idle | MediaSourceState::Local => {}
        }
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
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn stale_playback_updates_do_not_change_state_or_consume_subtitles() {
        let mut session = Session::new().unwrap();
        session.playlist_entry_id = Some(2);
        let url = url::Url::parse("https://example.invalid/video.mp4").unwrap();
        let relay = PlaybackRelay::start(url, BTreeMap::new()).unwrap();
        session.source = MediaSourceState::DirectRemote {
            relay,
            last_remote: RemoteMediaRequest {
                uri: MediaUri::HttpUrl("https://example.invalid/video.mp4".into()),
                playback_url: "https://example.invalid/video.mp4".into(),
                title: "video.mp4".into(),
                kind: Kind::Video,
                headers: BTreeMap::new(),
                subtitles: Vec::new(),
                is_live: false,
                photo: None,
            },
            pending_subs: vec!["memory://WEBVTT\n".into()],
        };
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
        assert!(!session.state.ended);
        assert!(!session.state.loaded);
        assert!(matches!(
            &session.source,
            MediaSourceState::DirectRemote { pending_subs, .. } if pending_subs.len() == 1
        ));
        let update = SessionUpdate {
            playlist_entry_id: Some(2),
            event: SessionEvent::Loaded,
        };
        assert!(matches!(
            session.apply_update(&update),
            Some(SessionEvent::Loaded)
        ));
        assert!(session.state.loaded);
        assert!(matches!(
            &session.source,
            MediaSourceState::DirectRemote { pending_subs, .. } if pending_subs.is_empty()
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
            format!("FRAME\n{}", "x".repeat(384)).repeat(5)
        );
        for attempt in 0..2 {
            let entry_id = session
                .mpv
                .loadfile_with_options(&clip, "replace", &[("pause", "yes")])
                .unwrap();
            session.reset_for_load(
                None,
                format!("https://example.invalid/clip-{attempt}"),
                None,
                entry_id,
                Kind::Video,
                false,
                None,
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while !session.state.loaded || session.state.video_size != (16, 16) {
                while let Ok(update) = events.try_recv() {
                    session.apply_update(&update);
                }
                assert!(
                    Instant::now() < deadline,
                    "attempt {attempt}: loaded={} video_size={:?}",
                    session.state.loaded,
                    session.state.video_size
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        session.shutdown();
    }
}
