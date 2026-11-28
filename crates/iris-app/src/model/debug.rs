//! Narrow `IRIS_CONTROL` diagnostic snapshot and debug-command forwarding APIs
//! on [`super::AppModel`], gated strictly behind `#[cfg(any(debug_assertions, feature = "devtools"))]`.

#[cfg(any(debug_assertions, feature = "devtools"))]
use iris_core::catalog::MediaFilter;
#[cfg(any(debug_assertions, feature = "devtools"))]
use iris_core::keymap::KeymapScheme;
#[cfg(any(debug_assertions, feature = "devtools"))]
use iris_core::player::{AspectMode, Session};
#[cfg(any(debug_assertions, feature = "devtools"))]
use iris_core::preset::SidebarMode;
#[cfg(any(debug_assertions, feature = "devtools"))]
use iris_core::settings::Side;

#[cfg(any(debug_assertions, feature = "devtools"))]
use super::AppModel;
#[cfg(any(debug_assertions, feature = "devtools"))]
use crate::playback::PlaybackSessionPort;
#[cfg(any(debug_assertions, feature = "devtools"))]
use crate::plugins::PluginPlayer;

/// Parses a `filter <arg>` `IRIS_CONTROL` command token into a [`MediaFilter`].
#[cfg(any(debug_assertions, feature = "devtools"))]
pub fn parse_debug_filter(arg: &str) -> MediaFilter {
    match arg {
        "video" => MediaFilter::Video,
        "photo" => MediaFilter::Photo,
        "audio" => MediaFilter::Audio,
        "favorites" => MediaFilter::FavoritesOnly,
        _ => MediaFilter::All,
    }
}

/// Read-only headless diagnostic snapshot for `[iris/control] state ...`.
#[cfg(any(debug_assertions, feature = "devtools"))]
#[derive(Clone, Debug, PartialEq)]
pub struct DebugStateSnapshot<'a> {
    pub time_pos: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub loop_file: bool,
    pub aspect: AspectMode,
    pub sub_delay: f64,
    pub video_size: (i64, i64),
    pub hwdec: &'a str,
    pub directory_open: bool,
    pub directory_width: f32,
    pub directory_side: Side,
    pub filter: MediaFilter,
    pub favorite: bool,
}

/// Read-only headless diagnostic snapshot for `[iris/control] ui ...`.
#[cfg(any(debug_assertions, feature = "devtools"))]
#[derive(Clone, Debug, PartialEq)]
pub struct DebugUiSnapshot<'a> {
    pub active_preset: &'a str,
    pub seek_step: f64,
    pub always_on_top: bool,
    pub photo_filmstrip: bool,
    pub sidebar_mode: SidebarMode,
    pub rows_len: usize,
    pub queue_len: usize,
    pub keymap_scheme: KeymapScheme,
    pub theme_id: &'a str,
    pub hide_titlebar: bool,
    pub modal_open: bool,
    pub sources_len: usize,
}

#[cfg(any(debug_assertions, feature = "devtools"))]
impl<S: PlaybackSessionPort + PluginPlayer> AppModel<S> {
    /// Applies a `filter <arg>` debug command to the catalog and rebuilds directory rows.
    pub fn debug_apply_filter(&mut self, arg: &str) -> MediaFilter {
        let filter = parse_debug_filter(arg);
        self.set_filter(filter);
        self.rebuild_directory_rows();
        self.catalog.filter()
    }

    /// Captures the headless domain fields formatted by `[iris/control] state`.
    pub fn debug_state_snapshot(&self) -> DebugStateSnapshot<'_> {
        let session = self.playback.session();
        let st = PlaybackSessionPort::state(session);
        let favorite = PlaybackSessionPort::current_media_uri(session)
            .is_some_and(|u| self.catalog.store().is_favorite(&u));
        let s = &self.workbench.settings;
        DebugStateSnapshot {
            time_pos: st.time_pos,
            duration: st.duration,
            paused: st.paused,
            volume: st.volume,
            muted: st.muted,
            speed: st.speed,
            loop_file: st.loop_file,
            aspect: st.aspect,
            sub_delay: st.sub_delay,
            video_size: st.video_size,
            hwdec: &st.hwdec,
            directory_open: s.directory_open,
            directory_width: s.directory_width,
            directory_side: s.directory_side,
            filter: self.catalog.filter(),
            favorite,
        }
    }

    /// Captures the headless domain fields formatted by `[iris/control] ui`.
    pub fn debug_ui_snapshot(&self) -> DebugUiSnapshot<'_> {
        let s = &self.workbench.settings;
        DebugUiSnapshot {
            active_preset: &s.active_preset,
            seek_step: s.seek_step,
            always_on_top: s.always_on_top,
            photo_filmstrip: s.photo_filmstrip,
            sidebar_mode: self.catalog.sidebar_mode(),
            rows_len: self.catalog.rows().len(),
            queue_len: self.catalog.queue().len(),
            keymap_scheme: self.workbench.keymap.scheme,
            theme_id: &s.theme_id,
            hide_titlebar: self.effective_hide_titlebar(),
            modal_open: self.plugins.active_modal().is_some(),
            sources_len: self.plugins.media_sources().len(),
        }
    }
}

#[cfg(any(debug_assertions, feature = "devtools"))]
impl AppModel<Session> {
    pub fn debug_mpv_property(&self, name: &str) -> Result<String, anyhow::Error> {
        self.playback.session().debug_property(name)
    }

    pub fn debug_mpv_command(&self, args: &[&str]) -> Result<(), anyhow::Error> {
        self.playback.session().debug_command(args)
    }
}
