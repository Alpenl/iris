//! Viewport-owned playback session, structured `loadfile_with_options` dispatch,
//! and encapsulated playback state transitions.

mod backend;
mod inspect;
mod model;
mod prepare;
mod relay;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use futures::channel::mpsc::UnboundedReceiver;
use iris_mpv::{Mpv, RenderHandle};
use relay::PlaybackRelay;

use crate::library::{self, Entry, FolderEntry, Kind, SortBy, SortOrder};
use crate::media_uri::MediaUri;
use crate::photo::Photo;
use crate::settings::{Hwdec, OnEof, Settings};
use crate::vfs::{self, http_client::origin_headers};

pub use model::{
    AspectMode, AudioDevice, Chapter, Command, MAX_SPEED, MIN_SPEED, MediaStats, PlaybackState,
    RemoteMediaRequest, RemoteSubtitle, SPEED_STEPS, SessionEvent, SessionUpdate, Track, TrackKind,
    format_time, friendly_error, step_speed,
};
pub use prepare::{Prepared, PreparedQueue, prepare, prepare_queue};

pub struct Session {
    pub(super) mpv: Arc<Mpv>,
    event_pump: backend::EventPump,
    event_rx: RefCell<Option<UnboundedReceiver<SessionUpdate>>>,
    pub(super) state: PlaybackState,
    folder: Option<PathBuf>,
    entries: Vec<Entry>,
    subfolders: Vec<FolderEntry>,
    pub(super) current: Option<PathBuf>,
    pub(super) current_uri: Option<String>,
    pub(super) current_title: Option<String>,
    pub(super) kind: Option<Kind>,
    pub(super) photo_meta: Option<Photo>,
    pub(super) last_remote: Option<RemoteMediaRequest>,
    playlist_entry_id: Option<i64>,
    pending_remote_subs: Vec<String>,
    playback_relay: Option<PlaybackRelay>,
    pub(super) extra_rotate: u16,
    source_rotation: u16,
    pub(super) user_hwdec: Cell<Hwdec>,
    on_eof: Cell<OnEof>,
    pub(super) subtitle_inset: f64,
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
            folder: None,
            entries: Vec::new(),
            subfolders: Vec::new(),
            current: None,
            current_uri: None,
            current_title: None,
            kind: None,
            photo_meta: None,
            last_remote: None,
            playlist_entry_id: None,
            pending_remote_subs: Vec::new(),
            playback_relay: None,
            extra_rotate: 0,
            source_rotation: 0,
            user_hwdec: Cell::new(Hwdec::AutoSafe),
            on_eof: Cell::new(OnEof::Pause),
            subtitle_inset: 0.,
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

    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn subfolders(&self) -> &[FolderEntry] {
        &self.subfolders
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

    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }

    pub fn is_actively_playing_video(&self) -> bool {
        self.kind == Some(Kind::Video)
            && self.state.loaded
            && !self.state.paused
            && !self.state.ended
    }

    pub fn set_directory(
        &mut self,
        folder: Option<PathBuf>,
        entries: Vec<Entry>,
        subfolders: Vec<FolderEntry>,
    ) {
        self.folder = folder;
        self.entries = entries;
        self.subfolders = subfolders;
    }

    pub fn sort_directory(&mut self, by: SortBy, order: SortOrder) {
        library::sort_entries(&mut self.entries, by, order);
    }

    pub fn enqueue_entries(&mut self, extra: Vec<Entry>) {
        for entry in extra {
            if !self.entries.iter().any(|e| e.path == entry.path) {
                self.entries.push(entry);
            }
        }
    }

    pub fn remove_entry(&mut self, path: &Path) -> bool {
        if let Some(ix) = self.entries.iter().position(|e| e.path == path) {
            self.entries.remove(ix);
            true
        } else {
            false
        }
    }

    pub fn clear_entries(&mut self) {
        self.entries.clear();
    }

    pub fn stop(&mut self) -> Result<()> {
        self.mpv.command_async(&["stop"])?;
        self.current = None;
        self.current_uri = None;
        self.current_title = None;
        self.playlist_entry_id = None;
        self.pending_remote_subs.clear();
        self.playback_relay = None;
        self.event_pump.set_relay(Default::default());
        self.kind = None;
        self.photo_meta = None;
        self.last_remote = None;
        self.extra_rotate = 0;
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

    pub fn open(&mut self, prepared: Prepared, resume: f64) -> Result<()> {
        let Prepared {
            folder,
            entries,
            subfolders,
            target,
            kind,
            photo,
        } = prepared;
        self.entries = entries;
        self.subfolders = subfolders;
        self.folder = Some(folder);
        match target {
            Some(path) => self.load(&path, kind, photo, resume),
            None => self.stop(),
        }
    }

    pub fn open_prepared_queue(&mut self, prepared: PreparedQueue, resume: f64) -> Result<()> {
        let PreparedQueue {
            folder,
            entries,
            photo,
        } = prepared;
        let Some(first) = entries.first() else {
            return Ok(());
        };
        let first_path = first.path.clone();
        let first_kind = first.kind;
        self.folder = Some(folder);
        self.entries = entries;
        self.subfolders.clear();
        self.load(&first_path, Some(first_kind), photo, resume)
    }

    #[allow(clippy::too_many_arguments)]
    fn reset_for_load(
        &mut self,
        current: Option<PathBuf>,
        current_uri: String,
        current_title: Option<String>,
        playlist_entry_id: i64,
        pending_remote_subs: Vec<String>,
        kind: Kind,
        is_live: bool,
        photo: Option<Photo>,
    ) {
        self.current = current;
        self.current_uri = Some(current_uri);
        self.current_title = current_title;
        self.playlist_entry_id = Some(playlist_entry_id);
        self.pending_remote_subs = pending_remote_subs;
        self.kind = Some(kind);
        self.state.is_live = is_live;
        self.state.buffering = false;
        self.state.cache_duration = 0.;
        self.state.photo_colour = photo
            .as_ref()
            .and_then(|p| p.colour)
            .map(|c| c.display_name());
        self.state.photo_exif = photo.as_ref().and_then(|p| p.exif.clone());
        self.photo_meta = photo;
        self.extra_rotate = 0;
        self.source_rotation = 0;
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

    fn load_remote(
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
        let entry_id = match self
            .mpv
            .loadfile_with_options(&playback_url, "replace", &opt_refs)
        {
            Ok(id) => id,
            Err(err) => {
                self.event_pump.set_relay(previous);
                return Err(err.into());
            }
        };
        self.playback_relay = Some(relay);
        self.reset_for_load(
            None,
            req.uri.to_uri_string(),
            Some(req.title),
            entry_id,
            pending_subs,
            req.kind,
            req.is_live,
            req.photo,
        );
        self.last_remote = Some(saved_remote);
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
        self.playback_relay = None;
        self.event_pump.set_relay(Default::default());
        self.reset_for_load(
            Some(path.to_path_buf()),
            canonical_uri,
            None,
            entry_id,
            Vec::new(),
            kind,
            false,
            photo,
        );
        self.last_remote = None;
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
        (!self.state.is_live
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

    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Position(v) => self.state.time_pos = *v,
            SessionEvent::Duration(v) => self.state.duration = *v,
            SessionEvent::Paused(v) => self.state.paused = *v,
            SessionEvent::Volume(v) => self.state.volume = *v,
            SessionEvent::Muted(v) => self.state.muted = *v,
            SessionEvent::Speed(v) => self.state.speed = *v,
            SessionEvent::Width(v) => self.state.video_size.0 = *v,
            SessionEvent::Height(v) => self.state.video_size.1 = *v,
            SessionEvent::Hardware(v) => self.state.hwdec.clone_from(v),
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
            SessionEvent::SourceRotation(v) => self.source_rotation = v.rem_euclid(360) as u16,
            SessionEvent::LoopFile(v) => self.state.loop_file = *v,
            SessionEvent::Chapter(v) => self.state.current_chapter = *v,
            SessionEvent::ChaptersChanged => self.chapters_dirty = true,
            SessionEvent::TracksChanged => self.tracks_dirty = true,
            SessionEvent::Buffering(v) => self.state.buffering = *v,
            SessionEvent::CacheDuration(v) => {
                self.state.cache_duration = if v.is_finite() { v.max(0.) } else { 0. };
            }
            SessionEvent::Loaded => {
                self.state.loaded = true;
                for sub_mem in std::mem::take(&mut self.pending_remote_subs) {
                    let _ = self.mpv.command_async(&["sub-add", &sub_mem, "auto"]);
                }
                self.tracks_dirty = true;
                self.chapters_dirty = true;
            }
            SessionEvent::Error(_) => {
                self.pending_remote_subs.clear();
                self.playback_relay = None;
                self.event_pump.set_relay(Default::default());
                self.state.loaded = false;
            }
            _ => {}
        }
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
        if let Some(relay) = &self.playback_relay {
            relay.cancel();
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

    #[test]
    fn stale_playback_updates_do_not_change_state_or_consume_subtitles() {
        let mut session = Session::new().unwrap();
        session.playlist_entry_id = Some(2);
        session.pending_remote_subs.push("memory://WEBVTT\n".into());
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
        assert_eq!(session.pending_remote_subs.len(), 1);
        let update = SessionUpdate {
            playlist_entry_id: Some(2),
            event: SessionEvent::Loaded,
        };
        assert!(matches!(
            session.apply_update(&update),
            Some(SessionEvent::Loaded)
        ));
        assert!(session.state.loaded);
        assert!(session.pending_remote_subs.is_empty());
    }
}
