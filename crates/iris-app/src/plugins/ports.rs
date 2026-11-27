//! Headless player port, execution context, and pure domain change / side-effect
//! descriptors for [`super::PluginController`].
//!
//! Intentionally free of window-visibility flags and UI region names (`DirtyRegions`
//! mapping lives exclusively in `crates/iris`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iris_api::v2::{ErrorCode, Id};
use iris_core::media_uri::MediaUri;
use iris_core::player::{
    Chapter, Command as PlayerCommand, ExternalTrack, MonitorLutFilter, PlaybackSnapshotInput,
    PlaybackState, ScopedPlayback, Session,
};
use iris_core::settings::Settings;
use iris_core::store::Store;
use iris_plugin_host::DirtySlots;

use super::sources::ActiveSource;

/// Canonical fallback title when a plugin resolves a stream without providing a title.
pub const DEFAULT_PLUGIN_MEDIA_TITLE: &str = "插件媒体";

/// Localized user-facing description for a protocol-2 [`ErrorCode`].
pub const fn error_code_label(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::AuthFailed => "鉴权失败或播放源拒绝访问",
        ErrorCode::Denied => "播放地址或权限被策略拒绝",
        ErrorCode::QuotaExceeded => "插件资源配额不足",
        ErrorCode::Timeout => "请求超时",
        ErrorCode::IoError => "网络或文件读取失败",
        ErrorCode::StaleHandle => "会话已过期，请重试",
        ErrorCode::Unsupported => "不支持的媒体格式或操作",
        ErrorCode::Backpressure => "请求过于频繁，请稍后重试",
        ErrorCode::Cancelled => "操作已取消",
        ErrorCode::InvalidArgument => "参数格式无效",
        ErrorCode::RangeInvalid => "分片范围无效",
        ErrorCode::BadState => "插件状态异常，请重试",
        ErrorCode::OutOfWindow => "请求超出当前播放窗口范围",
        ErrorCode::Internal => "插件内部错误",
    }
}

/// Persistable playback position snapshot extracted from [`PluginPlayer`].
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerProgress {
    Local {
        path: PathBuf,
        pos: f64,
        dur: f64,
        eof: bool,
    },
    Uri {
        uri: String,
        pos: f64,
        dur: f64,
        eof: bool,
    },
}

/// Narrow player port consumed by [`super::PluginController`].
///
/// Implemented directly by [`Session`] in production and integration tests, and by
/// deterministic in-memory doubles in headless unit tests without invoking `mpv_create`.
pub trait PluginPlayer {
    fn current_local_path(&self) -> Option<&Path>;
    fn current_media_uri(&self) -> Option<MediaUri>;
    fn current_title(&self) -> Option<&str>;
    fn may_share_current_identity(&self) -> bool;
    fn state(&self) -> &PlaybackState;
    fn has_scoped_stream(&self) -> bool;
    fn playback_snapshot_input(&self) -> PlaybackSnapshotInput;
    fn progress(&self) -> Option<PlayerProgress>;
    fn execute(&mut self, command: PlayerCommand) -> anyhow::Result<()>;
    fn open_scoped(
        &mut self,
        scoped: Arc<ScopedPlayback>,
        uri: MediaUri,
        settings: &Settings,
        resume: f64,
    ) -> anyhow::Result<()>;
    fn set_private_identity(&mut self, private: bool);
    fn stop(&mut self) -> anyhow::Result<()>;
    fn release_scoped(&mut self);
    fn set_plugin_timeline(&mut self, owner: String, chapters: &[Chapter]) -> anyhow::Result<()>;
    fn attach_plugin_track(&mut self, track: Arc<dyn ExternalTrack>) -> anyhow::Result<()>;
    fn revoke_plugin_tracks(&mut self, owner: &str);
    fn set_monitor_lut(&mut self, filter: Option<Arc<dyn MonitorLutFilter>>) -> anyhow::Result<()>;
    fn flush_deferred(&mut self);
}

impl PluginPlayer for Session {
    fn current_local_path(&self) -> Option<&Path> {
        self.current()
    }

    fn current_media_uri(&self) -> Option<MediaUri> {
        self.current_media_uri()
    }

    fn current_title(&self) -> Option<&str> {
        self.current_title()
    }

    fn may_share_current_identity(&self) -> bool {
        self.may_share_current_identity()
    }

    fn state(&self) -> &PlaybackState {
        self.state()
    }

    fn has_scoped_stream(&self) -> bool {
        self.scoped_stream().is_some()
    }

    fn playback_snapshot_input(&self) -> PlaybackSnapshotInput {
        self.playback_snapshot_input()
    }

    fn progress(&self) -> Option<PlayerProgress> {
        if let Some((path, pos, dur, eof)) = self.progress() {
            Some(PlayerProgress::Local {
                path: path.to_path_buf(),
                pos,
                dur,
                eof,
            })
        } else {
            self.uri_progress()
                .map(|(uri, pos, dur, eof)| PlayerProgress::Uri {
                    uri: uri.to_owned(),
                    pos,
                    dur,
                    eof,
                })
        }
    }

    fn execute(&mut self, command: PlayerCommand) -> anyhow::Result<()> {
        self.execute(command)
    }

    fn open_scoped(
        &mut self,
        scoped: Arc<ScopedPlayback>,
        uri: MediaUri,
        settings: &Settings,
        resume: f64,
    ) -> anyhow::Result<()> {
        self.open_scoped(scoped, uri, settings, resume)
    }

    fn set_private_identity(&mut self, private: bool) {
        self.set_private_identity(private);
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.stop()
    }

    fn release_scoped(&mut self) {
        self.release_scoped();
    }

    fn set_plugin_timeline(&mut self, owner: String, chapters: &[Chapter]) -> anyhow::Result<()> {
        self.set_plugin_timeline(owner, chapters)
    }

    fn attach_plugin_track(&mut self, track: Arc<dyn ExternalTrack>) -> anyhow::Result<()> {
        self.attach_plugin_track(track)
    }

    fn revoke_plugin_tracks(&mut self, owner: &str) {
        self.revoke_plugin_tracks(owner);
    }

    fn set_monitor_lut(&mut self, filter: Option<Arc<dyn MonitorLutFilter>>) -> anyhow::Result<()> {
        self.set_monitor_lut(filter)
    }

    fn flush_deferred(&mut self) {
        self.flush_deferred();
    }
}

/// Disjoint sibling domain state borrowed while executing [`super::PluginController`] operations.
pub struct PluginContext<'a, P: PluginPlayer> {
    pub player: &'a mut P,
    pub store: &'a mut Store,
    pub settings: &'a Settings,
    pub active_source: &'a mut ActiveSource,
}

/// Pure domain change summary produced by [`super::PluginController`].
///
/// Describes *what* domain state mutated without naming UI regions or reading window visibility.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PluginDomainChanges {
    pub locations_reloaded: bool,
    pub catalog_changed: bool,
    pub favorites_changed: bool,
    pub theme_changed: bool,
    pub modal_changed: bool,
    pub consent_changed: bool,
    pub close_overlays: bool,
    pub stream_meta_cleared: bool,
    pub playback_changed: bool,
    pub timeline_changed: bool,
    pub track_changed: bool,
    pub lut_changed: bool,
    pub overlay_changed: bool,
    pub annotations_changed: bool,
    pub uninstalled_changed: bool,
}

impl PluginDomainChanges {
    pub const fn merge(self, other: Self) -> Self {
        Self {
            locations_reloaded: self.locations_reloaded || other.locations_reloaded,
            catalog_changed: self.catalog_changed || other.catalog_changed,
            favorites_changed: self.favorites_changed || other.favorites_changed,
            theme_changed: self.theme_changed || other.theme_changed,
            modal_changed: self.modal_changed || other.modal_changed,
            consent_changed: self.consent_changed || other.consent_changed,
            close_overlays: self.close_overlays || other.close_overlays,
            stream_meta_cleared: self.stream_meta_cleared || other.stream_meta_cleared,
            playback_changed: self.playback_changed || other.playback_changed,
            timeline_changed: self.timeline_changed || other.timeline_changed,
            track_changed: self.track_changed || other.track_changed,
            lut_changed: self.lut_changed || other.lut_changed,
            overlay_changed: self.overlay_changed || other.overlay_changed,
            annotations_changed: self.annotations_changed || other.annotations_changed,
            uninstalled_changed: self.uninstalled_changed || other.uninstalled_changed,
        }
    }
}

/// Minimal ordered platform/cross-controller side effects emitted by [`super::PluginController`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginSideEffect {
    ShowToast {
        message: String,
        is_error: bool,
    },
    StepMedia {
        delta: isize,
        wrap: bool,
    },
    RefreshSource {
        plugin_id: String,
        source_id: String,
    },
    OpenExternalUrl(String),
    ScheduleBrowseTimeout {
        plugin_id: String,
        request_id: Id,
        timeout: Duration,
    },
    ScheduleResolveTimeout {
        plugin_id: String,
        request_id: Id,
        timeout: Duration,
    },
    MediaResolveStarted,
    MediaLoadCancelled,
    MediaOpenSucceeded,
    MediaOpenFailed {
        show_toast: bool,
    },
    PlaybackStopped,
    ScheduleOverlayWake {
        delay: Duration,
    },
    ClearOverlayWake,
    ClearOwnerGlyphs(String),
    ClearAllGlyphs,
    ResetThumbnails,
}

/// Complete domain delta and ordered side-effect batch returned by [`super::PluginController`].
#[derive(Default, Debug)]
pub struct PluginBatchOutcome {
    pub dirty_slots: DirtySlots,
    pub status_changed: bool,
    pub domain: PluginDomainChanges,
    pub effects: Vec<PluginSideEffect>,
}

impl PluginBatchOutcome {
    pub fn merge(&mut self, mut other: Self) {
        self.dirty_slots.merge(other.dirty_slots);
        self.status_changed |= other.status_changed;
        self.domain = self.domain.merge(other.domain);
        self.effects.append(&mut other.effects);
    }

    pub fn push_toast(&mut self, message: impl Into<String>, is_error: bool) {
        self.effects.push(PluginSideEffect::ShowToast {
            message: message.into(),
            is_error,
        });
    }
}

/// Outcome of routing a user-submitted stream URL through matching `UrlResolver` plugins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenUrlOutcome {
    ResolvedByPlugin,
    OpenDirect { url: String },
    ValidationError(String),
}
