//! Pure, `libmpv`-free playback state machine (`PlayerMachine`), `MediaSourceState`
//! lifecycle variants, `MpvOp` effect descriptors, and event-batch coalescing.
//!
//! Illegal state combinations (e.g. `pending_subs` after `Loaded`, `in_flight_adds`
//! outside `Loaded` `PluginScoped`, `deferred_local_subs` when `Idle`/`Opening`, or
//! `error_*` recovery snapshots outside `Failed`) are unrepresentable by construction.

mod reduce;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use crate::library::Kind;
use crate::media_uri::MediaUri;
use crate::photo::Photo;
use crate::settings::{Hwdec, OnEof};

use super::CommandReceipts;
use super::MonitorLutFilter;
use super::enhance::ExternalTrack;
use super::model::{AspectMode, Chapter, PlaybackState, RemoteMediaRequest};
use super::relay::PlaybackRelay;
use super::scoped::{ScopedAttachment, ScopedPlayback, ScopedStream};

pub use reduce::coalesce_updates;

/// Declarative mpv side effect emitted by [`PlayerMachine`] transitions and
/// executed by the thin `Mpv` adapter on `Session`.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::player) enum MpvOp {
    CommandAsync(Vec<String>),
    CommandWithReply { receipt: u64, args: Vec<String> },
    CommandSync(Vec<String>),
    SetPropertyString { name: String, value: String },
    SetPropertyF64Async { name: String, value: f64 },
    RestoreAttachmentPolicy,
    DisableHlsPolicy,
    SyncVideoParams,
    QueryDurationIfMissing,
    RefreshTracks,
    RefreshChapters,
    LogLoaded,
    LogError(i32),
}

impl MpvOp {
    pub(in crate::player) fn cmd_async(args: &[&str]) -> Self {
        Self::CommandAsync(args.iter().map(|s| (*s).to_owned()).collect())
    }

    pub(in crate::player) fn cmd_reply(receipt: u64, args: &[&str]) -> Self {
        Self::CommandWithReply {
            receipt,
            args: args.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    pub(in crate::player) fn cmd_sync(args: &[&str]) -> Self {
        Self::CommandSync(args.iter().map(|s| (*s).to_owned()).collect())
    }
}

/// Canonical identity of an opening, loaded, or failed media item.
#[derive(Clone)]
pub(in crate::player) struct MediaIdentity {
    pub local_path: Option<PathBuf>,
    pub uri: String,
    pub title: Option<String>,
    pub private_identity: bool,
    pub kind: Kind,
    pub photo: Option<Photo>,
}

/// Transport owned while mpv is opening the file (`StartFile` .. `FileLoaded`).
pub(in crate::player) enum OpeningTransport {
    Local,
    DirectRemote {
        relay: PlaybackRelay,
        last_remote: Box<RemoteMediaRequest>,
        pending_subs: Vec<String>,
    },
    PluginScoped {
        playback: Arc<ScopedPlayback>,
        pending_tracks: Vec<ScopedAttachment>,
    },
}

/// Transport owned once `FileLoaded` has fired and initial tracks were dispatched.
pub(in crate::player) enum LoadedTransport {
    Local,
    DirectRemote {
        relay: PlaybackRelay,
        last_remote: Box<RemoteMediaRequest>,
    },
    PluginScoped {
        playback: Arc<ScopedPlayback>,
        in_flight_adds: BTreeSet<u64>,
    },
}

/// Recoverable transport retained after `EndReason::Error` or idle queue overflow.
pub(in crate::player) enum FailedTransport {
    Local,
    StoppedRemote {
        last_remote: Box<RemoteMediaRequest>,
    },
    PluginScoped {
        playback: Arc<ScopedPlayback>,
    },
}

/// Weak/value snapshots of plugin enhancements captured on playback failure so a
/// subsequent GPU-reset recovery can restore them if still live.
#[derive(Default)]
pub(in crate::player) struct FailedEnhancements {
    pub lut: Option<Weak<dyn MonitorLutFilter>>,
    pub tracks: Vec<Weak<dyn ExternalTrack>>,
    pub timeline: Option<(String, Vec<Chapter>)>,
}

/// Coalesced metadata refresh flags valid only while a medium is [`MediaSourceState::Loaded`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player) struct DirtyFlags {
    pub tracks: bool,
    pub chapters: bool,
}

/// Explicit media session lifecycle state; eliminates loose boolean/Vec flags on `Session`.
pub(in crate::player) enum MediaSourceState {
    Idle {
        eof_paused: bool,
    },
    Opening {
        entry_id: i64,
        identity: MediaIdentity,
        transport: OpeningTransport,
        reattach_on_load: Vec<Arc<dyn ExternalTrack>>,
        eof_paused: bool,
    },
    Loaded {
        entry_id: i64,
        identity: MediaIdentity,
        transport: LoadedTransport,
        /// Validated UTF-8 local subtitle paths waiting for in-flight async track adds.
        deferred_local_subs: Vec<String>,
        dirty: DirtyFlags,
        eof_paused: bool,
    },
    Failed {
        identity: MediaIdentity,
        transport: FailedTransport,
        enhancements: FailedEnhancements,
        eof_paused: bool,
    },
}

impl Default for MediaSourceState {
    fn default() -> Self {
        Self::Idle { eof_paused: false }
    }
}

impl MediaSourceState {
    pub fn eof_paused(&self) -> bool {
        match self {
            Self::Idle { eof_paused }
            | Self::Opening { eof_paused, .. }
            | Self::Loaded { eof_paused, .. }
            | Self::Failed { eof_paused, .. } => *eof_paused,
        }
    }

    pub fn set_eof_paused(&mut self, value: bool) {
        match self {
            Self::Idle { eof_paused }
            | Self::Opening { eof_paused, .. }
            | Self::Loaded { eof_paused, .. }
            | Self::Failed { eof_paused, .. } => *eof_paused = value,
        }
    }

    pub fn entry_id(&self) -> Option<i64> {
        match self {
            Self::Opening { entry_id, .. } | Self::Loaded { entry_id, .. } => Some(*entry_id),
            Self::Idle { .. } | Self::Failed { .. } => None,
        }
    }

    pub fn identity(&self) -> Option<&MediaIdentity> {
        match self {
            Self::Opening { identity, .. }
            | Self::Loaded { identity, .. }
            | Self::Failed { identity, .. } => Some(identity),
            Self::Idle { .. } => None,
        }
    }

    pub fn identity_mut(&mut self) -> Option<&mut MediaIdentity> {
        match self {
            Self::Opening { identity, .. }
            | Self::Loaded { identity, .. }
            | Self::Failed { identity, .. } => Some(identity),
            Self::Idle { .. } => None,
        }
    }

    pub fn is_plugin_scoped(&self) -> bool {
        matches!(
            self,
            Self::Opening {
                transport: OpeningTransport::PluginScoped { .. },
                ..
            } | Self::Loaded {
                transport: LoadedTransport::PluginScoped { .. },
                ..
            } | Self::Failed {
                transport: FailedTransport::PluginScoped { .. },
                ..
            }
        )
    }

    pub fn scoped_playback(&self) -> Option<&Arc<ScopedPlayback>> {
        match self {
            Self::Opening {
                transport: OpeningTransport::PluginScoped { playback, .. },
                ..
            }
            | Self::Loaded {
                transport: LoadedTransport::PluginScoped { playback, .. },
                ..
            }
            | Self::Failed {
                transport: FailedTransport::PluginScoped { playback },
                ..
            } => Some(playback),
            _ => None,
        }
    }

    pub fn scoped_stream(&self) -> Option<Arc<dyn ScopedStream>> {
        self.scoped_playback().map(|p| Arc::clone(&p.stream))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn has_in_flight_scoped_adds(&self) -> bool {
        matches!(
            self,
            Self::Loaded {
                transport: LoadedTransport::PluginScoped { in_flight_adds, .. },
                ..
            } if !in_flight_adds.is_empty()
        )
    }

    pub fn has_deferred_local_subs(&self) -> bool {
        matches!(
            self,
            Self::Loaded {
                deferred_local_subs,
                ..
            } if !deferred_local_subs.is_empty()
        )
    }

    pub fn mark_chapters_dirty(&mut self) {
        if let Self::Loaded { dirty, .. } = self {
            dirty.chapters = true;
        }
    }

    pub fn stop_active_transports(&self) {
        match self {
            Self::Opening {
                transport: OpeningTransport::PluginScoped { playback, .. },
                ..
            }
            | Self::Loaded {
                transport: LoadedTransport::PluginScoped { playback, .. },
                ..
            }
            | Self::Failed {
                transport: FailedTransport::PluginScoped { playback },
                ..
            } => playback.stream.stop(),
            Self::Opening {
                transport: OpeningTransport::DirectRemote { relay, .. },
                ..
            }
            | Self::Loaded {
                transport: LoadedTransport::DirectRemote { relay, .. },
                ..
            } => relay.cancel(),
            _ => {}
        }
    }
}

/// Context queried from `PluginEnhancements` during pure state-machine transitions.
#[derive(Clone, Copy, Debug, Default)]
pub(in crate::player) struct EnhancementView {
    pub has_pending_plugin_adds: bool,
    pub plugin_reply_done: bool,
    pub has_any_plugin_tracks: bool,
    pub has_timeline_owner: bool,
}

/// Pure playback state machine testable without `libmpv` or OS threads.
pub(in crate::player) struct PlayerMachine {
    pub state: PlaybackState,
    pub source: MediaSourceState,
    pub user_hwdec: Hwdec,
    pub on_eof: OnEof,
    receipts: CommandReceipts,
}

impl PlayerMachine {
    pub fn new(receipts: CommandReceipts) -> Self {
        Self {
            state: PlaybackState::default(),
            source: MediaSourceState::default(),
            user_hwdec: Hwdec::AutoSafe,
            on_eof: OnEof::Pause,
            receipts,
        }
    }

    pub fn kind(&self) -> Option<Kind> {
        self.source.identity().map(|id| id.kind)
    }

    pub fn photo_meta(&self) -> Option<&Photo> {
        self.source.identity().and_then(|id| id.photo.as_ref())
    }

    pub fn current_path(&self) -> Option<&Path> {
        self.source
            .identity()
            .and_then(|id| id.local_path.as_deref())
    }

    pub fn current_uri(&self) -> Option<&str> {
        self.source.identity().map(|id| id.uri.as_str())
    }

    pub fn current_title(&self) -> Option<&str> {
        self.source.identity().and_then(|id| id.title.as_deref())
    }

    pub fn private_identity(&self) -> bool {
        self.source.identity().is_some_and(|id| id.private_identity)
    }

    pub fn set_private_identity(&mut self, private: bool) {
        if let Some(identity) = self.source.identity_mut() {
            identity.private_identity = private;
        }
    }

    pub fn current_media_uri(&self) -> Option<MediaUri> {
        let id = self.source.identity()?;
        if !id.uri.is_empty() {
            Some(MediaUri::parse(&id.uri))
        } else {
            id.local_path.clone().map(MediaUri::Local)
        }
    }

    pub fn loop_value(&self, live: bool) -> &'static str {
        if !live && self.on_eof == OnEof::Loop {
            "inf"
        } else {
            "no"
        }
    }

    pub fn effective_hwdec_str(&self, has_monitor_lut: bool) -> &'static str {
        match self.user_hwdec {
            Hwdec::Off => "no",
            Hwdec::AutoSafe => {
                let kind = self.kind();
                if (kind == Some(Kind::Video) && (self.state.hflip || self.state.vflip))
                    || (matches!(kind, Some(Kind::Video | Kind::Photo)) && has_monitor_lut)
                {
                    "auto-copy"
                } else {
                    "auto-safe"
                }
            }
        }
    }

    /// Installs a newly opened media item into [`MediaSourceState::Opening`], stopping
    /// any superseded transport (unless `PluginScoped` is reopening the exact same stream)
    /// and emitting post-EOF unpause if needed.
    pub fn begin_load(
        &mut self,
        entry_id: i64,
        identity: MediaIdentity,
        is_live: bool,
        transport: OpeningTransport,
        unpause_after_eof: bool,
    ) -> Vec<MpvOp> {
        let same_scoped = match (&self.source, &transport) {
            (
                state,
                OpeningTransport::PluginScoped {
                    playback: next_pb, ..
                },
            ) => state
                .scoped_playback()
                .is_some_and(|old_pb| Arc::ptr_eq(old_pb, next_pb)),
            _ => false,
        };
        if !same_scoped {
            self.source.stop_active_transports();
        }

        let photo_colour = identity
            .photo
            .as_ref()
            .and_then(|p| p.colour)
            .map(|c| c.display_name());
        let photo_exif = identity.photo.as_ref().and_then(|p| p.exif.clone());

        self.source = MediaSourceState::Opening {
            entry_id,
            identity,
            transport,
            reattach_on_load: Vec::new(),
            eof_paused: false,
        };
        self.state.is_live = is_live;
        self.state.ab_start = None;
        self.state.ab_end = None;
        self.state.buffering = false;
        self.state.cache_duration = 0.;
        self.state.photo_colour = photo_colour;
        self.state.photo_exif = photo_exif;
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

        let mut ops = Vec::new();
        if unpause_after_eof {
            self.state.paused = false;
            ops.push(MpvOp::SetPropertyString {
                name: "pause".into(),
                value: "no".into(),
            });
        }
        ops
    }

    /// Transitions the state machine to [`MediaSourceState::Idle`] on explicit stop.
    pub fn transition_to_idle(&mut self, ended_now: bool) {
        let eof_paused = ended_now || self.state.ended || self.source.eof_paused();
        self.source.stop_active_transports();
        self.source = MediaSourceState::Idle { eof_paused };
        self.state.ab_start = None;
        self.state.ab_end = None;
        self.state.photo_colour = None;
        self.state.photo_exif = None;
        self.state.tracks.clear();
        self.state.chapters.clear();
        self.state.loaded = false;
        self.state.ended = false;
        self.state.buffering = false;
        self.state.cache_duration = 0.;
        self.state.is_live = false;
    }

    /// Transitions `Opening` or `Loaded` into [`MediaSourceState::Failed`], preserving
    /// recoverable source descriptors for retry or GPU-reset reload.
    pub fn transition_to_failed(&mut self, enhancements: FailedEnhancements) {
        let eof_paused = self.source.eof_paused();
        let next = match std::mem::take(&mut self.source) {
            MediaSourceState::Opening {
                identity,
                transport,
                ..
            } => match transport {
                OpeningTransport::Local => MediaSourceState::Failed {
                    identity,
                    transport: FailedTransport::Local,
                    enhancements,
                    eof_paused,
                },
                OpeningTransport::DirectRemote {
                    relay, last_remote, ..
                } => {
                    relay.cancel();
                    MediaSourceState::Failed {
                        identity,
                        transport: FailedTransport::StoppedRemote { last_remote },
                        enhancements,
                        eof_paused,
                    }
                }
                OpeningTransport::PluginScoped { playback, .. } => {
                    if playback.stream.is_cancelled() {
                        playback.stream.stop();
                        MediaSourceState::Idle { eof_paused }
                    } else {
                        MediaSourceState::Failed {
                            identity,
                            transport: FailedTransport::PluginScoped { playback },
                            enhancements,
                            eof_paused,
                        }
                    }
                }
            },
            MediaSourceState::Loaded {
                identity,
                transport,
                ..
            } => match transport {
                LoadedTransport::Local => MediaSourceState::Failed {
                    identity,
                    transport: FailedTransport::Local,
                    enhancements,
                    eof_paused,
                },
                LoadedTransport::DirectRemote { relay, last_remote } => {
                    relay.cancel();
                    MediaSourceState::Failed {
                        identity,
                        transport: FailedTransport::StoppedRemote { last_remote },
                        enhancements,
                        eof_paused,
                    }
                }
                LoadedTransport::PluginScoped { playback, .. } => {
                    if playback.stream.is_cancelled() {
                        playback.stream.stop();
                        MediaSourceState::Idle { eof_paused }
                    } else {
                        MediaSourceState::Failed {
                            identity,
                            transport: FailedTransport::PluginScoped { playback },
                            enhancements,
                            eof_paused,
                        }
                    }
                }
            },
            MediaSourceState::Failed {
                identity,
                transport,
                ..
            } => MediaSourceState::Failed {
                identity,
                transport,
                enhancements,
                eof_paused,
            },
            MediaSourceState::Idle { eof_paused } => MediaSourceState::Idle { eof_paused },
        };
        self.source = next;
        self.state.loaded = false;
    }

    /// Promotes [`MediaSourceState::Opening`] to [`MediaSourceState::Loaded`] and returns
    /// the attachment commands (`sub-add` / `audio-add`) and tracks to reattach.
    pub fn promote_opening_to_loaded(&mut self) -> (Vec<MpvOp>, Vec<Arc<dyn ExternalTrack>>) {
        let mut ops = vec![MpvOp::SyncVideoParams, MpvOp::QueryDurationIfMissing];
        let mut reattach = Vec::new();

        let prev = std::mem::take(&mut self.source);
        self.source = match prev {
            MediaSourceState::Opening {
                entry_id,
                identity,
                transport,
                reattach_on_load,
                eof_paused,
            } => {
                reattach = reattach_on_load;
                let loaded_transport = match transport {
                    OpeningTransport::Local => LoadedTransport::Local,
                    OpeningTransport::DirectRemote {
                        relay,
                        last_remote,
                        pending_subs,
                    } => {
                        for sub in pending_subs {
                            ops.push(MpvOp::cmd_async(&["sub-add", &sub, "auto"]));
                        }
                        LoadedTransport::DirectRemote { relay, last_remote }
                    }
                    OpeningTransport::PluginScoped {
                        playback,
                        pending_tracks,
                    } => {
                        let mut in_flight_adds = BTreeSet::new();
                        if !pending_tracks.is_empty() {
                            ops.push(MpvOp::RestoreAttachmentPolicy);
                            for track in pending_tracks {
                                let cmd = if track.audio { "audio-add" } else { "sub-add" };
                                let flags = if track.default { "select" } else { "auto" };
                                if let Ok(receipt) = self.receipts.next() {
                                    ops.push(MpvOp::cmd_reply(
                                        receipt,
                                        &[
                                            cmd,
                                            &track.url,
                                            flags,
                                            &track.label,
                                            track.lang.as_deref().unwrap_or(""),
                                        ],
                                    ));
                                    in_flight_adds.insert(receipt);
                                }
                            }
                        }
                        LoadedTransport::PluginScoped {
                            playback,
                            in_flight_adds,
                        }
                    }
                };
                MediaSourceState::Loaded {
                    entry_id,
                    identity,
                    transport: loaded_transport,
                    deferred_local_subs: Vec::new(),
                    dirty: DirtyFlags {
                        tracks: true,
                        chapters: true,
                    },
                    eof_paused,
                }
            }
            MediaSourceState::Loaded {
                entry_id,
                identity,
                transport,
                deferred_local_subs,
                eof_paused,
                ..
            } => MediaSourceState::Loaded {
                entry_id,
                identity,
                transport,
                deferred_local_subs,
                dirty: DirtyFlags {
                    tracks: true,
                    chapters: true,
                },
                eof_paused,
            },
            other => other,
        };
        self.state.loaded = true;
        ops.push(MpvOp::LogLoaded);
        (ops, reattach)
    }
}

#[cfg(test)]
mod tests;
