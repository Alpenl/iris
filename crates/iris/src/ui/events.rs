//! Session event batching, adaptive 0/1/4Hz timeline pixel throttle, on-demand 5Hz
//! plugin `Tick`, two-phase media opening, thumbnail pumping, and progress persistence.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::Context;
use iris_app::{
    EofOutcome, NowPlayingSnapshot, OpenMediaBatchAction, SAVE_EVERY, StoreFlushIntent,
    SubtitleFlushOutcome, plan_open_paths,
};
use iris_core::library::Kind;
use iris_core::player::{self, SessionUpdate};

use super::AppModel;
use super::views::{DirtyRegions, dirty_regions_for_playback};
use crate::platform;

const MIN_BAR_TICK_INTERVAL: Duration = Duration::from_millis(250);

impl AppModel {
    pub(super) fn on_events(&mut self, batch: Vec<SessionUpdate>, cx: &mut Context<Self>) {
        let now = Instant::now();
        let report = self.playback.process_session_updates(
            batch,
            self.media_load_pending(),
            self.plugins.scoped_released(),
            self.settings.on_eof,
            now,
        );
        let changes = report.changes;
        let mut plugin_dirty = DirtyRegions::NONE;
        let mut tick_dirty = false;
        let mut stats_refreshed = false;

        if report.clock_changed {
            self.sync_overlay_clock();
        }
        if changes.seeked {
            self.advance_plugin_seek(cx);
        }
        if changes.loaded_or_reconfigured {
            plugin_dirty = plugin_dirty.union(self.announce_plugin_media_if_loaded(cx));
        }
        if changes.seeked || changes.loaded_or_reconfigured {
            self.reconcile_enhancements(cx);
        } else if report.clock_changed {
            self.update_overlay_window(cx);
        }
        if report.clock_changed || changes.seeked || changes.loaded_or_reconfigured {
            self.step_timed_overlay_frame(cx);
        }
        if changes.loaded_or_reconfigured
            || changes.buffering_changed
            || changes.other_session_changed
            || (matches!(changes.media_kind, Some(Kind::Audio))
                && (changes.duration_changed || changes.pause_or_speed_changed))
        {
            self.sync_viewport_effects(cx);
        }
        if report.should_flush_subtitles {
            self.flush_pending_subtitles(cx);
        }

        self.sleep_preventer.set_active(report.sleep_prevent_active);
        if self.catalog.last_save().elapsed() >= SAVE_EVERY {
            self.save_progress_periodic(cx);
        }

        if changes.loaded_or_reconfigured && self.overlays.info_open {
            self.playback.refresh_cached_stats(true, now);
            stats_refreshed = true;
        }
        if let Some(t) = report.position {
            if self.overlays.info_open && self.playback.refresh_stats_if_due(now) {
                stats_refreshed = true;
                tick_dirty = true;
            }
            if self.chrome_shown() || changes.media_kind == Some(Kind::Audio) {
                let sec = t.max(0.).floor() as i64;
                let bar_px = if report.duration > 0. {
                    let logical_w = self
                        .chrome
                        .probes
                        .progress_bounds
                        .get()
                        .map_or(800., |b| f32::from(b.size.width).max(1.) as f64);
                    ((t / report.duration).clamp(0., 1.) * logical_w * self.chrome.last_scale)
                        .round() as i32
                } else {
                    -1
                };
                let sec_changed = sec != self.chrome.last_render_sec;
                let bar_moved = bar_px != self.chrome.last_render_bar_px
                    && self.chrome.last_bar_tick.elapsed() >= MIN_BAR_TICK_INTERVAL;
                if sec_changed || bar_moved {
                    self.chrome.last_render_sec = sec;
                    self.chrome.last_render_bar_px = bar_px;
                    self.chrome.last_bar_tick = now;
                    tick_dirty = true;
                }
            }
            if self.plugins.has_state_listeners() {
                let out = self.with_plugin_ctx(|ctrl, ctx| ctrl.maybe_tick(ctx, now));
                plugin_dirty = plugin_dirty.union(self.apply_plugin_outcome(out, None, cx));
            }
        }

        if let (Some(position), Some(ratio)) =
            (report.position, self.chrome.scrub.seek_preview_ratio)
            && !self.chrome.scrub.seeking
            && (position - f64::from(ratio) * report.duration).abs() <= 0.5
        {
            self.chrome.scrub.seek_preview_ratio = None;
            self.chrome.scrub.seek_preview_timeout = None;
            tick_dirty = true;
        }
        if report.cache_duration_changed
            && self.chrome_shown()
            && !report.paused
            && report.duration > 0.
        {
            let width = self
                .chrome
                .probes
                .progress_bounds
                .get()
                .map_or(800., |bounds| f64::from(f32::from(bounds.size.width)));
            let ratio = report
                .cache_end
                .map_or(0., |end| (end / report.duration).clamp(0., 1.));
            let cache_px = (ratio * width * self.chrome.last_scale).round() as i32;
            if cache_px != self.chrome.last_render_cache_px
                && self.chrome.last_cache_tick.elapsed() >= MIN_BAR_TICK_INTERVAL
            {
                self.chrome.last_render_cache_px = cache_px;
                self.chrome.last_cache_tick = now;
                tick_dirty = true;
            }
        }

        if let Some(np) = report.now_playing_updated {
            self.apply_now_playing_snapshot(np);
        }

        if changes.pause_or_speed_changed {
            let out = self.with_plugin_ctx(|ctrl, ctx| ctrl.broadcast_state_changed(ctx, now));
            plugin_dirty = plugin_dirty.union(self.apply_plugin_outcome(out, None, cx));
        }

        match report.eof_outcome {
            EofOutcome::None => {}
            EofOutcome::SaveAndStepNext => {
                self.save_progress_sync();
                self.step(1, false, cx);
            }
            EofOutcome::SaveAndSettle => {
                self.save_progress_sync();
            }
        }

        for osd in report.osd_messages {
            self.show_osd(osd, cx);
        }

        if let Some(code) = report.error_code {
            self.show_toast(
                format!("{}（错误 {code}）", super::strings::errors::mpv_error(code)),
                cx,
            );
        }

        let playback_dirty =
            dirty_regions_for_playback(&changes, tick_dirty, stats_refreshed, self.ui_visibility());
        let final_dirty = playback_dirty.union(plugin_dirty);
        if final_dirty.any() {
            self.notify_regions(final_dirty, cx);
        }
    }

    pub(super) fn save_progress_sync(&mut self) {
        self.catalog_ui.store_flush = None;
        let progress = iris_app::PluginPlayer::progress(self.playback.session());
        self.catalog.save_progress_sync(
            progress,
            self.settings.resume_playback,
            self.plugins.scoped_released(),
            Instant::now(),
        );
    }

    fn save_progress_periodic(&mut self, cx: &mut Context<Self>) {
        let progress = iris_app::PluginPlayer::progress(self.playback.session());
        if let Some(intent) = self.catalog.maybe_periodic_flush(
            progress,
            self.settings.resume_playback,
            self.plugins.scoped_released(),
            Instant::now(),
        ) {
            self.spawn_store_flush(intent, cx);
        }
    }

    fn spawn_store_flush(&mut self, intent: StoreFlushIntent, cx: &mut Context<Self>) {
        let work = cx.background_executor().spawn(async move { intent.run() });
        self.catalog_ui.store_flush = Some(cx.spawn(async move |this, cx| {
            let receipt = work.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(next_intent) = this.catalog.complete_store_flush(receipt) {
                    this.spawn_store_flush(next_intent, cx);
                } else if !this.catalog.is_store_flush_in_flight() {
                    this.catalog_ui.store_flush = None;
                }
            });
        }));
    }

    pub(super) fn finish_media_open(&mut self, cx: &mut Context<Self>) {
        self.plugins.sync_media_open_state(self.playback.session());
        self.refresh_annotations(cx);
        let cur_uri = self.session().current_media_uri();
        let may_share = self.session().may_share_current_identity();
        let cur_title = self.session().current_title().map(str::to_owned);
        self.catalog
            .record_media_opened(cur_uri, may_share, cur_title.as_deref());
        self.playback
            .on_media_open_succeeded(self.settings.photo_default);
        {
            let vp = self.active_mut();
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
            self.playback.refresh_cached_stats(true, Instant::now());
        }
        self.sync_viewport_effects(cx);
    }

    pub(crate) fn open_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let plan = plan_open_paths(paths);
        if let Some(package) = plan.package_to_review {
            self.review_plugin_path(package, cx);
        }
        match plan.media_action {
            OpenMediaBatchAction::None => {}
            OpenMediaBatchAction::SinglePath(path) => self.open_path(&path, cx),
            OpenMediaBatchAction::MultiQueue(paths) => self.open_multi_queue(paths, cx),
        }
        let pending = self.media_load_pending();
        if self.playback.enqueue_subtitles(plan.subtitles, pending) {
            self.flush_pending_subtitles(cx);
        }
    }

    pub(super) fn media_load_pending(&self) -> bool {
        self.playback.pending_path().is_some()
            || self.catalog.has_remote_load()
            || self.plugins.has_pending_resolve()
    }

    fn flush_pending_subtitles(&mut self, cx: &mut Context<Self>) {
        match self.playback.flush_pending_subtitles() {
            SubtitleFlushOutcome::Empty => {}
            SubtitleFlushOutcome::MediaNotReady => {
                self.show_toast("请先打开视频或音频，再加载字幕", cx);
            }
            SubtitleFlushOutcome::Loaded { count } => {
                self.show_toast(format!("已请求加载 {count} 个字幕文件"), cx);
            }
            SubtitleFlushOutcome::PartialFailure {
                count,
                failed,
                last_error,
            } => {
                log::warn!("subtitle load failed: {last_error:#}");
                self.show_toast(
                    format!(
                        "{count} 个字幕文件中有 {failed} 个加载失败 · {}",
                        super::strings::errors::player_action_error(&last_error)
                    ),
                    cx,
                );
            }
        }
    }

    fn schedule_loading_indicator(&self, generation: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.playback.maybe_show_loading_indicator(generation) {
                    this.notify_regions(
                        DirtyRegions {
                            root: true,
                            viewport: true,
                            ..DirtyRegions::NONE
                        },
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    pub(crate) fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.cancel_media_loads();
        self.close_plugin_browse();
        self.save_progress_sync();
        if !self.advance_plugin_media(cx) {
            return;
        }
        self.overlays.close_menus();
        self.plugins.clear_active_stream_meta();
        let cached = self.catalog.cached_parent_scan(path);
        let generation = self.playback.begin_local_load(Some(path.to_path_buf()));
        let target_path = path.to_path_buf();
        let task = cx
            .background_executor()
            .spawn(async move { player::prepare(&target_path, cached) });

        self.schedule_loading_indicator(generation, cx);

        cx.spawn(async move |this, cx| {
            let prepared = task.await;
            let _ = this.update(cx, |this, cx| {
                if !this.playback.accept_prepared_load(generation) {
                    return;
                }
                let result = prepared.and_then(|prepared| {
                    let resume = this
                        .catalog
                        .resume_for_local(this.settings.resume_playback, prepared.target());
                    let dir = this.session_mut().open(prepared, resume)?;
                    this.catalog.install_local_directory(dir);
                    Ok(())
                });
                match result {
                    Ok(()) => {
                        this.finish_media_open(cx);
                        if this.session().kind().is_none() {
                            this.flush_pending_subtitles(cx);
                        }
                    }
                    Err(e) => {
                        let np = this.playback.on_media_open_failed(Instant::now());
                        if let Some(snapshot) = np {
                            this.apply_now_playing_snapshot(snapshot);
                        }
                        this.rebuild_directory_rows();
                        this.show_toast(format!("无法打开 · {e}"), cx);
                    }
                }
                this.active_mut().image.reset();
                this.sync_viewport_effects(cx);
                this.notify_all(cx);
            });
        })
        .detach();
    }

    fn open_multi_queue(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.cancel_media_loads();
        self.close_plugin_browse();
        self.save_progress_sync();
        if !self.advance_plugin_media(cx) {
            return;
        }
        self.overlays.close_menus();
        self.plugins.clear_active_stream_meta();
        let generation = self.playback.begin_local_load(paths.first().cloned());
        let task = cx
            .background_executor()
            .spawn(async move { player::prepare_queue(paths) });
        self.schedule_loading_indicator(generation, cx);

        cx.spawn(async move |this, cx| {
            let prepared = task.await;
            let _ = this.update(cx, |this, cx| {
                if !this.playback.accept_prepared_load(generation) {
                    return;
                }
                let Some(prepared) = prepared else {
                    let np = this.playback.on_media_open_failed(Instant::now());
                    if let Some(snapshot) = np {
                        this.apply_now_playing_snapshot(snapshot);
                    }
                    this.rebuild_directory_rows();
                    this.show_toast("所选项目中没有可播放的媒体", cx);
                    this.notify_all(cx);
                    return;
                };
                let count = prepared.entries().len();
                this.catalog.replace_queue_from_entries(prepared.entries());
                let resume = this
                    .catalog
                    .resume_for_local(this.settings.resume_playback, prepared.target());
                match this.session_mut().open_prepared_queue(prepared, resume) {
                    Ok(dir) => {
                        this.catalog.install_local_directory(dir);
                        this.finish_media_open(cx);
                        this.show_toast(format!("已加入播放队列 · {count} 个文件"), cx);
                    }
                    Err(e) => {
                        let np = this.playback.on_media_open_failed(Instant::now());
                        if let Some(snapshot) = np {
                            this.apply_now_playing_snapshot(snapshot);
                        }
                        this.rebuild_directory_rows();
                        this.show_toast(format!("无法打开 · {e}"), cx);
                    }
                }
                this.active_mut().image.reset();
                this.sync_viewport_effects(cx);
                this.notify_all(cx);
            });
        })
        .detach();
    }

    pub(super) fn ensure_filmstrip_thumbnails(&mut self, cx: &mut Context<Self>) {
        let is_photo = self.session().kind() == Some(Kind::Photo);
        let prioritized = self
            .catalog
            .prioritized_filmstrip_window(is_photo, self.settings.photo_filmstrip);
        let has_work = !prioritized.is_empty();
        self.catalog_ui.thumbs.enqueue_window(prioritized);
        if has_work {
            self.pump_thumbnail_queue(cx);
        }
    }

    fn pump_thumbnail_queue(&mut self, cx: &mut Context<Self>) {
        for (batch_gen, path) in self.catalog_ui.thumbs.pop_next_batch() {
            let p_decode = path.clone();
            let task = cx
                .background_executor()
                .spawn(async move { platform::decode_thumbnail(&p_decode, 160) });
            cx.spawn(async move |this, cx| {
                let decoded = task.await;
                let _ = this.update(cx, |this, cx| {
                    let changed = this.catalog_ui.thumbs.finish(batch_gen, path, decoded);
                    this.pump_thumbnail_queue(cx);
                    if changed && this.chrome_shown() && this.settings.photo_filmstrip {
                        this.notify_viewport(cx);
                    }
                });
            })
            .detach();
        }
    }

    pub(super) fn sync_now_playing(&mut self, force: bool) {
        if let Some(snapshot) = self.playback.poll_now_playing(force, Instant::now()) {
            self.apply_now_playing_snapshot(snapshot);
        }
    }

    pub(super) fn apply_now_playing_snapshot(&mut self, snapshot: Option<NowPlayingSnapshot>) {
        let Some(bridge) = &mut self.media_keys else {
            return;
        };
        let state = self.playback.session().state();
        let media = snapshot.as_ref().map(|info| platform::NowPlaying {
            title: Cow::Borrowed(info.title.as_str()),
            is_video: info.is_video,
            state,
        });
        bridge.update_now_playing(media);
    }
}
