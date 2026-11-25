//! Session event batching, adaptive 0/1/4Hz timeline throttle, on-demand 5Hz
//! plugin `Tick`, two-phase media opening, thumbnail pumping, and progress persistence.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::Context;
use iris_api::v2::HostEvent;
use iris_core::library::{self, Kind, ScanResult};
use iris_core::player::{self, Command, SessionEvent, SessionUpdate, TrackKind};
use iris_core::settings::{OnEof, PhotoDefault};

use super::IrisWindow;
use super::state::ActiveSource;
use crate::platform;

const SAVE_EVERY: Duration = Duration::from_secs(5);
const MIN_BAR_TICK_INTERVAL: Duration = Duration::from_millis(250);
const STATS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Subtitle selections follow the load that owns them, including during a folder scan.
#[derive(Default)]
pub(super) struct PendingSubtitles {
    generation: u64,
    paths: Vec<PathBuf>,
}

impl PendingSubtitles {
    fn add(&mut self, generation: u64, paths: Vec<PathBuf>) {
        if self.generation == generation && !self.paths.is_empty() {
            self.paths.extend(paths);
        } else {
            self.generation = generation;
            self.paths = paths;
        }
    }

    fn take(&mut self, generation: u64) -> Vec<PathBuf> {
        let paths = std::mem::take(&mut self.paths);
        if self.generation == generation {
            paths
        } else {
            Vec::new()
        }
    }

    pub(super) fn clear(&mut self) {
        self.paths.clear();
    }

    pub(super) fn advance(&mut self, from: u64, to: u64) {
        if self.generation == from {
            self.generation = to;
        }
    }
}

fn split_open_paths(mut paths: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    paths.retain(|path| !path.as_os_str().is_empty());
    let subtitles = paths
        .extract_if(.., |path| library::is_subtitle(path))
        .collect();
    (paths, subtitles)
}

impl IrisWindow {
    pub(super) fn on_events(&mut self, batch: Vec<SessionUpdate>, cx: &mut Context<Self>) {
        let was_ended = self.active().session.state().ended;
        let mut need_notify = false;
        let mut saw_position: Option<f64> = None;
        let mut clock_changed = false;
        let mut saw_seek = false;
        let mut saw_duration = false;
        let mut saw_pause_or_speed = false;
        let mut saw_loaded_or_reconfig = false;
        let mut saw_ended_now = false;
        let mut saw_audio = false;
        let mut saw_sub = false;
        let mut saw_volume = false;
        let mut saw_cache_duration = false;
        let mut last_error: Option<i32> = None;

        for update in batch {
            let Some(ev) = self.active_mut().session.apply_update(&update) else {
                continue;
            };
            clock_changed |= matches!(
                ev,
                SessionEvent::Position(_)
                    | SessionEvent::Duration(_)
                    | SessionEvent::Paused(_)
                    | SessionEvent::Speed(_)
                    | SessionEvent::Buffering(_)
                    | SessionEvent::Ended(_)
                    | SessionEvent::Error(_)
                    | SessionEvent::Loaded
            );
            match ev {
                SessionEvent::Seek => {
                    saw_seek = true;
                    clock_changed = true;
                    need_notify = true;
                }
                SessionEvent::Position(t) => saw_position = Some(*t),
                SessionEvent::Duration(_) => {
                    saw_duration = true;
                    need_notify = true;
                }
                SessionEvent::Paused(_) | SessionEvent::Speed(_) => {
                    saw_pause_or_speed = true;
                    need_notify = true;
                }
                SessionEvent::Ended(true) => saw_ended_now = true,
                SessionEvent::Audio(_) => {
                    saw_audio = true;
                    need_notify = true;
                }
                SessionEvent::SubtitleTrack(_) => {
                    saw_sub = true;
                    need_notify = true;
                }
                SessionEvent::Volume(_) => {
                    saw_volume = true;
                    need_notify = true;
                }
                SessionEvent::Loaded | SessionEvent::Reconfigured => {
                    saw_loaded_or_reconfig = true;
                    need_notify = true;
                }
                SessionEvent::CacheDuration(_) => saw_cache_duration = true,
                SessionEvent::Error(code) => last_error = Some(*code),
                _ => need_notify = true,
            }
        }
        self.active_mut().session.flush_deferred();
        if clock_changed {
            self.sync_overlay_clock();
        }
        if saw_seek {
            self.advance_plugin_seek(cx);
        }
        if saw_loaded_or_reconfig && self.announce_plugin_media_if_loaded(cx) {
            need_notify = true;
        }
        if saw_seek || saw_loaded_or_reconfig {
            self.reconcile_enhancements(cx);
        } else if clock_changed {
            self.update_overlay_window(cx);
        }
        if saw_loaded_or_reconfig
            && self.active().session.state().loaded
            && !self.media_load_pending()
        {
            self.flush_pending_subtitles(cx);
        }

        self.sleep_preventer
            .set_active(self.active().session.is_actively_playing_video());
        if self.last_save.elapsed() >= SAVE_EVERY {
            self.save_progress_periodic(cx);
        }

        if saw_loaded_or_reconfig && self.overlays.info_open {
            self.active_mut().refresh_cached_stats(true);
        }

        if let Some(t) = saw_position {
            if self.overlays.info_open
                && self.active().last_stats_refresh.elapsed() >= STATS_REFRESH_INTERVAL
            {
                self.active_mut().refresh_cached_stats(false);
                need_notify = true;
            }
            if self.chrome_shown() || self.active().session.kind() == Some(Kind::Audio) {
                let sec = t.max(0.).floor() as i64;
                let duration = self.active().session.state().duration;
                let bar_px = if duration > 0. {
                    let logical_w = self
                        .chrome
                        .probes
                        .progress_bounds
                        .get()
                        .map_or(800., |b| f32::from(b.size.width).max(1.) as f64);
                    ((t / duration).clamp(0., 1.) * logical_w * self.chrome.last_scale).round()
                        as i32
                } else {
                    -1
                };
                let sec_changed = sec != self.chrome.last_render_sec;
                let bar_moved = bar_px != self.chrome.last_render_bar_px
                    && self.chrome.last_bar_tick.elapsed() >= MIN_BAR_TICK_INTERVAL;
                if sec_changed || bar_moved {
                    self.chrome.last_render_sec = sec;
                    self.chrome.last_render_bar_px = bar_px;
                    self.chrome.last_bar_tick = Instant::now();
                    need_notify = true;
                }
            }
            if self.plugins.has_state_listeners()
                && self.last_plugin_tick.elapsed() >= Duration::from_millis(200)
            {
                self.last_plugin_tick = Instant::now();
                let st = self.active().session.state();
                let ev = HostEvent::Tick {
                    generation: self.plugins.generation(),
                    position: st.time_pos,
                    duration: st.duration,
                    paused: st.paused,
                };
                if self.broadcast_plugin_event(ev, None, cx) {
                    need_notify = true;
                }
            }
        }
        let (duration, paused, ended, cache_end) = {
            let state = self.active().session.state();
            let buffer = (!state.is_live && state.cache_duration > 0.)
                .then_some(state.time_pos + state.cache_duration);
            (state.duration, state.paused, state.ended, buffer)
        };
        if let (Some(position), Some(ratio)) = (saw_position, self.chrome.scrub.seek_preview_ratio)
            && !self.chrome.scrub.seeking
            && (position - f64::from(ratio) * duration).abs() <= 0.5
        {
            self.chrome.scrub.seek_preview_ratio = None;
            self.chrome.scrub.seek_preview_timeout = None;
            need_notify = true;
        }
        if saw_cache_duration && self.chrome_shown() && !paused && duration > 0. {
            let width = self
                .chrome
                .probes
                .progress_bounds
                .get()
                .map_or(800., |bounds| f64::from(f32::from(bounds.size.width)));
            let ratio = cache_end.map_or(0., |end| (end / duration).clamp(0., 1.));
            let cache_px = (ratio * width * self.chrome.last_scale).round() as i32;
            if cache_px != self.chrome.last_render_cache_px
                && self.chrome.last_cache_tick.elapsed() >= MIN_BAR_TICK_INTERVAL
            {
                self.chrome.last_render_cache_px = cache_px;
                self.chrome.last_cache_tick = Instant::now();
                need_notify = true;
            }
        }

        let force_now_playing = saw_pause_or_speed
            || saw_loaded_or_reconfig
            || was_ended != ended
            || (paused && (saw_position.is_some() || saw_duration));
        if force_now_playing || saw_position.is_some() || saw_duration {
            self.sync_now_playing(force_now_playing);
        }

        if saw_pause_or_speed {
            let snap = self.plugin_snapshot();
            if self.broadcast_plugin_event(
                HostEvent::StateChanged {
                    generation: self.plugins.generation(),
                    snapshot: snap,
                },
                None,
                cx,
            ) {
                need_notify = true;
            }
        }

        if saw_ended_now
            && !was_ended
            && !self.media_load_pending()
            && self.active().session.state().ended
        {
            self.save_progress_sync();
            if matches!(
                self.active().session.kind(),
                Some(Kind::Video | Kind::Audio)
            ) && self.settings.on_eof == OnEof::Next
                && !self.active().session.state().loop_file
            {
                self.step(1, false, cx);
            } else {
                need_notify = true;
            }
        }

        if saw_audio && self.active().announce_audio {
            self.active_mut().announce_audio = false;
            let label = self
                .active()
                .session
                .selected_track(TrackKind::Audio)
                .map_or_else(|| "无音轨".into(), super::strings::chrome::track_label);
            self.show_osd(format!("音轨 · {label}"), cx);
            need_notify = false;
        }

        if saw_sub && self.active().announce_sub {
            self.active_mut().announce_sub = false;
            let label = self
                .active()
                .session
                .selected_track(TrackKind::Sub)
                .map_or_else(|| "无字幕".into(), super::strings::chrome::track_label);
            self.show_osd(format!("字幕 · {label}"), cx);
            need_notify = false;
        }

        if saw_volume
            && self
                .active_mut()
                .announce_volume
                .take()
                .is_some_and(|requested| requested.elapsed() < Duration::from_secs(1))
        {
            let volume = self.active().session.state().volume;
            self.show_osd(format!("音量 · {volume:.0}%"), cx);
            need_notify = false;
        }

        if let Some(code) = last_error {
            if !self.media_load_pending() {
                self.active_mut().pending_subtitles.clear();
            }
            self.sync_now_playing(true);
            self.show_toast(
                format!("{}（错误 {code}）", super::strings::errors::mpv_error(code)),
                cx,
            );
            need_notify = false;
        }

        if need_notify {
            cx.notify();
        }
    }

    pub(super) fn record_progress_memory(&mut self) {
        if !self.settings.resume_playback {
            return;
        }
        if let Some((path, pos, dur, eof)) = self.active().session.progress() {
            let path = path.to_path_buf();
            self.store.remember_path(&path, pos, dur, eof);
        } else if let Some((uri_str, pos, dur, eof)) = self.active().session.uri_progress() {
            let uri = iris_core::media_uri::MediaUri::parse(uri_str);
            self.store.remember(&uri, pos, dur, eof);
        }
    }

    pub(super) fn save_progress_sync(&mut self) {
        self.record_progress_memory();
        if self.store.is_dirty() {
            self.store.save();
        }
        self.last_save = Instant::now();
    }

    fn save_progress_periodic(&mut self, cx: &mut Context<Self>) {
        self.record_progress_memory();
        self.last_save = Instant::now();
        if !self.store.is_dirty() || self.store_flush.is_some() {
            return;
        }
        let Some(flush) = self.store.prepare_progress_flush() else {
            self.store.save();
            return;
        };
        let work = cx.background_executor().spawn(async move { flush.run() });
        self.store_flush = Some(cx.spawn(async move |this, cx| {
            let applied = work.await;
            let _ = this.update(cx, |this, _| {
                this.store_flush = None;
                this.store.finish_progress_flush(applied);
            });
        }));
    }

    pub(super) fn finish_media_open(&mut self, cx: &mut Context<Self>) {
        self.refresh_annotations(cx);
        if let Some(uri) = self.active().session.current_media_uri() {
            if self.active().session.may_share_current_identity() {
                let title = match &uri {
                    iris_core::media_uri::MediaUri::Local(_) => None,
                    iris_core::media_uri::MediaUri::Plugin { .. }
                    | iris_core::media_uri::MediaUri::HttpUrl(_) => self
                        .active()
                        .session
                        .current_title()
                        .map(str::trim)
                        .filter(|t| {
                            !t.is_empty()
                                && *t != super::strings::chrome::DEFAULT_PLUGIN_MEDIA_TITLE
                        })
                        .map(str::to_owned),
                };
                self.store.push_recent_with_title(&uri, title.as_deref());
            }
        } else if let Some(dir) = self.catalog.local_dir.folder().map(Path::to_path_buf)
            && self.catalog.active_source.is_local()
        {
            self.store.push_recent_path(&dir);
        }
        let is_photo_actual = self.active().session.kind() == Some(Kind::Photo)
            && self.settings.photo_default == PhotoDefault::Actual;
        {
            let vp = self.active_mut();
            vp.pending_photo_actual = is_photo_actual;
            vp.cached_stats = None;
            vp.pending_win_drag_origin = None;
            vp.pending_click_origin = None;
        }
        self.chrome.last_render_sec = -1;
        self.chrome.last_render_bar_px = -1;
        self.chrome.last_render_cache_px = -1;
        self.chrome.scrub.seeking = false;
        self.chrome.scrub.seek_preview_ratio = None;
        self.chrome.scrub.seek_preview_timeout = None;
        self.chrome.scrub.timeline_hover = false;
        self.rebuild_directory_rows();
        self.scan_visible_favorites(cx);
        self.scroll_directory_to_current();
        self.ensure_filmstrip_thumbnails(cx);
        self.sync_now_playing(true);
        let _ = self.announce_plugin_media_if_loaded(cx);
        if self.overlays.info_open {
            self.active_mut().refresh_cached_stats(true);
        }
    }

    pub(crate) fn open_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let (packages, paths): (Vec<_>, Vec<_>) = paths.into_iter().partition(|p| {
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("irisplugin"))
        });
        for package in packages.into_iter().take(2) {
            self.review_plugin_path(package, cx);
        }
        let (paths, subtitles) = split_open_paths(paths);
        match paths.as_slice() {
            [] => {}
            [path] => self.open_path(path, cx),
            _ => self.open_multi_queue(paths, cx),
        }
        if subtitles.is_empty() {
            return;
        }
        let pending = self.media_load_pending();
        let vp = self.active_mut();
        vp.pending_subtitles.add(vp.load_generation, subtitles);
        if !pending && (vp.session.state().loaded || vp.session.kind().is_none()) {
            self.flush_pending_subtitles(cx);
        }
    }

    pub(super) fn media_load_pending(&self) -> bool {
        self.active().pending_path.is_some()
            || self.catalog.remote_load.is_some()
            || self.catalog.pending_resolve.is_some()
    }

    fn flush_pending_subtitles(&mut self, cx: &mut Context<Self>) {
        let vp = self.active_mut();
        let paths = vp.pending_subtitles.take(vp.load_generation);
        if paths.is_empty() {
            return;
        }
        if !matches!(vp.session.kind(), Some(Kind::Video | Kind::Audio)) {
            self.show_toast("请先打开视频或音频，再加载字幕", cx);
            return;
        }
        let count = paths.len();
        let mut failed = 0;
        let mut last_error = None;
        for path in paths {
            let result = std::path::absolute(&path)
                .map_err(anyhow::Error::from)
                .and_then(|local| vp.session.execute(Command::LoadSubtitle(local)));
            if let Err(error) = result {
                failed += 1;
                last_error = Some(error);
            }
        }
        if let Some(error) = last_error {
            log::warn!("subtitle load failed: {error:#}");
            self.show_toast(
                format!(
                    "{count} 个字幕文件中有 {failed} 个加载失败 · {}",
                    super::strings::errors::player_action_error(&error)
                ),
                cx,
            );
        } else {
            self.show_toast(format!("已请求加载 {count} 个字幕文件"), cx);
        }
    }

    pub(crate) fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.cancel_media_loads();
        self.close_plugin_browse();
        if !self.advance_plugin_media(cx) {
            return;
        }
        self.save_progress_sync();
        self.overlays.close_menus();
        self.catalog.active_source = ActiveSource::Local;
        self.catalog.active_stream_meta = None;
        let cached = {
            let local_dir = &self.catalog.local_dir;
            local_dir
                .folder()
                .filter(|folder| path.parent() == Some(*folder))
                .map(|folder| {
                    (
                        folder.to_path_buf(),
                        ScanResult {
                            entries: local_dir.entries().to_vec(),
                            subfolders: local_dir.subfolders().to_vec(),
                        },
                    )
                })
        };
        let generation = {
            let vp = self.active_mut();
            vp.load_generation += 1;
            vp.pending_path = Some(path.to_path_buf());
            vp.loading_visible = false;
            vp.load_generation
        };
        let target_path = path.to_path_buf();
        let task = cx
            .background_executor()
            .spawn(async move { player::prepare(&target_path, cached) });

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let _ = this.update(cx, |this, cx| {
                let vp = this.active_mut();
                if vp.load_generation == generation && vp.pending_path.is_some() {
                    vp.loading_visible = true;
                    cx.notify();
                }
            });
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let prepared = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.active().load_generation != generation {
                    return;
                }
                {
                    let vp = this.active_mut();
                    vp.pending_path = None;
                    vp.loading_visible = false;
                }
                let result = prepared.and_then(|prepared| {
                    let resume = if this.settings.resume_playback {
                        prepared.target().map_or(0., |p| this.store.resume_path(p))
                    } else {
                        0.
                    };
                    let dir = this.active_mut().session.open(prepared, resume)?;
                    this.catalog.local_dir = dir;
                    Ok(())
                });
                match result {
                    Ok(()) => {
                        this.finish_media_open(cx);
                        if this.active().session.kind().is_none() {
                            this.flush_pending_subtitles(cx);
                        }
                    }
                    Err(e) => {
                        this.active_mut().pending_subtitles.paths.clear();
                        this.rebuild_directory_rows();
                        this.sync_now_playing(true);
                        this.show_toast(format!("无法打开 · {e}"), cx);
                    }
                }
                this.active_mut().image.reset();
                cx.notify();
            });
        })
        .detach();
    }

    fn open_multi_queue(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.cancel_media_loads();
        self.close_plugin_browse();
        if !self.advance_plugin_media(cx) {
            return;
        }
        self.save_progress_sync();
        self.overlays.close_menus();
        self.catalog.active_source = ActiveSource::Local;
        self.catalog.active_stream_meta = None;
        let generation = {
            let vp = self.active_mut();
            vp.load_generation += 1;
            vp.pending_path = paths.first().cloned();
            vp.loading_visible = false;
            vp.load_generation
        };
        let task = cx
            .background_executor()
            .spawn(async move { player::prepare_queue(paths) });

        cx.spawn(async move |this, cx| {
            let prepared = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.active().load_generation != generation {
                    return;
                }
                {
                    let vp = this.active_mut();
                    vp.pending_path = None;
                    vp.loading_visible = false;
                }
                let Some(prepared) = prepared else {
                    this.active_mut().pending_subtitles.paths.clear();
                    this.rebuild_directory_rows();
                    this.show_toast("所选项目中没有可播放的媒体", cx);
                    return;
                };
                let count = prepared.len();
                this.queue.replace_from_entries(prepared.entries());
                let resume = if this.settings.resume_playback {
                    prepared.target().map_or(0., |p| this.store.resume_path(p))
                } else {
                    0.
                };
                match this
                    .active_mut()
                    .session
                    .open_prepared_queue(prepared, resume)
                {
                    Ok(dir) => {
                        this.catalog.local_dir = dir;
                        this.finish_media_open(cx);
                        this.show_toast(format!("已加入播放队列 · {count} 个文件"), cx);
                    }
                    Err(e) => {
                        this.rebuild_directory_rows();
                        this.sync_now_playing(true);
                        this.active_mut().pending_subtitles.paths.clear();
                        this.show_toast(format!("无法打开 · {e}"), cx);
                    }
                }
                this.active_mut().image.reset();
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn ensure_filmstrip_thumbnails(&mut self, cx: &mut Context<Self>) {
        if self.active().session.kind() != Some(Kind::Photo) || !self.settings.photo_filmstrip {
            self.catalog.thumbs.enqueue_window(Vec::new());
            return;
        }
        let photos = &self.catalog.photo_paths;
        if photos.len() < 2 {
            self.catalog.thumbs.enqueue_window(Vec::new());
            return;
        }
        let cur_ix = self.catalog.filmstrip_index;
        let mut prioritized = Vec::with_capacity(13);
        prioritized.push(photos[cur_ix].clone());
        for d in 1..=6usize {
            if let Some(p) = photos.get(cur_ix + d) {
                prioritized.push(p.clone());
            }
            if cur_ix >= d {
                prioritized.push(photos[cur_ix - d].clone());
            }
        }
        self.catalog.thumbs.enqueue_window(prioritized);
        self.pump_thumbnail_queue(cx);
    }

    fn pump_thumbnail_queue(&mut self, cx: &mut Context<Self>) {
        for (batch_gen, path) in self.catalog.thumbs.pop_next_batch() {
            let p_decode = path.clone();
            let task = cx
                .background_executor()
                .spawn(async move { platform::decode_thumbnail(&p_decode, 160) });
            cx.spawn(async move |this, cx| {
                let decoded = task.await;
                let _ = this.update(cx, |this, cx| {
                    let changed = this.catalog.thumbs.finish(batch_gen, path, decoded);
                    this.pump_thumbnail_queue(cx);
                    if changed && this.chrome_shown() && this.settings.photo_filmstrip {
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    pub(super) fn sync_now_playing(&mut self, force: bool) {
        if !force && self.last_now_playing.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_now_playing = Instant::now();
        let Some(bridge) = &mut self.media_keys else {
            return;
        };
        let session = &self.viewports[0].session;
        let media = match session.kind() {
            Some(kind @ (Kind::Video | Kind::Audio)) if session.state().loaded => {
                let title = session
                    .current_title()
                    .map(std::borrow::Cow::Borrowed)
                    .or_else(|| {
                        session
                            .current()
                            .and_then(Path::file_name)
                            .map(|name| name.to_string_lossy())
                    });
                title.map(|title| platform::NowPlaying {
                    title,
                    is_video: kind == Kind::Video,
                    state: session.state(),
                })
            }
            _ => None,
        };
        bridge.update_now_playing(media);
    }
}

#[cfg(test)]
mod tests {
    use super::{PendingSubtitles, split_open_paths};
    use std::path::PathBuf;

    #[test]
    fn mixed_opens_preserve_media_and_subtitle_order() {
        let paths = ["A.srt", "first.mkv", "B.ASS", "second.mp4"].map(PathBuf::from);
        let (media, subtitles) = split_open_paths(paths.into());
        assert_eq!(media, ["first.mkv", "second.mp4"].map(PathBuf::from));
        assert_eq!(subtitles, ["A.srt", "B.ASS"].map(PathBuf::from));
    }

    #[test]
    fn additional_subtitles_join_only_the_same_pending_load() {
        let mut pending = PendingSubtitles::default();
        pending.add(1, vec!["old.srt".into()]);
        pending.add(2, vec!["new.srt".into()]);
        pending.add(2, vec!["second.srt".into()]);
        assert_eq!(
            pending.take(2),
            ["new.srt", "second.srt"].map(PathBuf::from)
        );
        pending.add(3, vec!["superseded.srt".into()]);
        assert!(pending.take(4).is_empty());

        pending.add(5, vec!["while-resolving.srt".into()]);
        pending.advance(5, 6);
        assert_eq!(pending.take(6), [PathBuf::from("while-resolving.srt")]);
    }
}
