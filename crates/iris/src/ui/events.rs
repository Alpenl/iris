//! Session event batching, adaptive 0/1/4Hz timeline throttle, on-demand 5Hz
//! plugin `Tick`, two-phase media opening, thumbnail pumping, and progress persistence.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::Context;
use iris_api::HostEvent;
use iris_core::library::{self, Kind, ScanResult};
use iris_core::player::{self, SessionEvent, TrackKind};
use iris_core::settings::{OnEof, PhotoDefault};
use iris_core::store::Store;

use super::IrisWindow;
use super::state::ActiveSource;
use crate::platform;

const SAVE_EVERY: Duration = Duration::from_secs(5);
const MIN_BAR_TICK_INTERVAL: Duration = Duration::from_millis(250);
const STATS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

impl IrisWindow {
    pub(super) fn on_events(&mut self, batch: Vec<SessionEvent>, cx: &mut Context<Self>) {
        let was_ended = self.active().session.state().ended;
        let mut need_notify = false;
        let mut saw_position: Option<f64> = None;
        let mut saw_pause_or_speed = false;
        let mut saw_loaded_or_reconfig = false;
        let mut saw_ended_now = false;
        let mut saw_audio = false;
        let mut saw_sub = false;
        let mut last_error: Option<i32> = None;

        for ev in &batch {
            self.active_mut().session.apply(ev);
            match ev {
                SessionEvent::Position(t) => saw_position = Some(*t),
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
                SessionEvent::Loaded | SessionEvent::Reconfigured => {
                    saw_loaded_or_reconfig = true;
                    need_notify = true;
                }
                SessionEvent::Error(code) => last_error = Some(*code),
                _ => need_notify = true,
            }
        }
        self.active_mut().session.flush_deferred();

        self.sleep_preventer
            .set_active(self.active().session.is_actively_playing_video());
        if self.last_save.elapsed() >= SAVE_EVERY {
            self.save_progress_async(cx);
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
                    position: st.time_pos,
                    duration: st.duration,
                    paused: st.paused,
                };
                if self.broadcast_plugin_event(ev, None, cx) {
                    need_notify = true;
                }
            }
            self.sync_now_playing(false);
        }

        if saw_pause_or_speed {
            self.sync_now_playing(true);
            let snap = self.plugin_snapshot();
            if self.broadcast_plugin_event(HostEvent::StateChanged { snapshot: snap }, None, cx) {
                need_notify = true;
            }
        }

        if saw_ended_now && !was_ended && self.active().session.state().ended {
            self.save_progress_async(cx);
            self.sync_now_playing(true);
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
                .map_or_else(|| "无音轨".into(), |t| t.label());
            self.show_osd(format!("音轨 · {label}"), cx);
            need_notify = false;
        }

        if saw_sub && self.active().announce_sub {
            self.active_mut().announce_sub = false;
            let label = self
                .active()
                .session
                .selected_track(TrackKind::Sub)
                .map_or_else(|| "无字幕".into(), |t| t.label());
            self.show_osd(format!("字幕 · {label}"), cx);
            need_notify = false;
        }

        if let Some(code) = last_error {
            self.show_toast(
                format!("{}（错误 {code}）", player::friendly_error(code)),
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

    pub(super) fn save_progress_async(&mut self, cx: &mut Context<Self>) {
        self.record_progress_memory();
        if let Some((path, snapshot)) = self.store.take_dirty_snapshot() {
            cx.background_executor()
                .spawn(async move { Store::write_snapshot(&path, &snapshot) })
                .detach();
        }
        self.last_save = Instant::now();
    }

    pub(super) fn finish_media_open(&mut self, cx: &mut Context<Self>) {
        if let Some(uri) = self.active().session.current_media_uri() {
            self.store.push_recent(&uri);
        } else if let Some(dir) = self.active().session.folder().map(Path::to_path_buf)
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
        }
        self.chrome.last_render_sec = -1;
        self.chrome.last_render_bar_px = -1;
        self.rebuild_directory_rows();
        self.scroll_directory_to_current();
        self.ensure_filmstrip_thumbnails(cx);
        self.sync_now_playing(true);
        let snap = self.plugin_snapshot();
        let _ = self.broadcast_plugin_event(HostEvent::MediaOpened { snapshot: snap }, None, cx);
        if self.overlays.info_open {
            self.active_mut().refresh_cached_stats(true);
        }
    }

    pub(crate) fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.save_progress_sync();
        self.overlays.close_menus();
        self.catalog.active_source = ActiveSource::Local;
        self.catalog.active_stream_meta = None;
        let safe_path = library::safe_resolve_path(path);
        let cached = {
            let session = &self.active().session;
            session
                .folder()
                .filter(|folder| safe_path.parent() == Some(*folder))
                .map(|folder| {
                    (
                        folder.to_path_buf(),
                        ScanResult {
                            entries: session.entries().to_vec(),
                            subfolders: session.subfolders().to_vec(),
                        },
                    )
                })
        };
        let generation = {
            let vp = self.active_mut();
            vp.load_generation += 1;
            vp.pending_path = Some(safe_path);
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
                    this.active_mut().session.open(prepared, resume)
                });
                match result {
                    Ok(()) => this.finish_media_open(cx),
                    Err(e) => this.show_toast(format!("无法打开 · {e}"), cx),
                }
                this.active_mut().image.reset();
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn open_multi_queue(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.save_progress_sync();
        self.overlays.close_menus();
        self.catalog.active_source = ActiveSource::Local;
        self.catalog.active_stream_meta = None;
        let generation = {
            let vp = self.active_mut();
            vp.load_generation += 1;
            vp.pending_path = paths.first().map(|p| library::safe_resolve_path(p));
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
                    Ok(()) => {
                        this.finish_media_open(cx);
                        this.show_toast(format!("已加入播放队列 · {count} 个文件"), cx);
                    }
                    Err(e) => this.show_toast(format!("无法打开 · {e}"), cx),
                }
                this.active_mut().image.reset();
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn ensure_filmstrip_thumbnails(&mut self, cx: &mut Context<Self>) {
        if self.active().session.kind() != Some(Kind::Photo) || !self.settings.photo_filmstrip {
            return;
        }
        let photos = &self.catalog.photo_paths;
        if photos.len() < 2 {
            return;
        }
        let cur_ix = self
            .active()
            .session
            .current()
            .and_then(|p| photos.iter().position(|x| x == p))
            .unwrap_or(0);
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
                    let keep = this.catalog.thumbs.generation() == batch_gen || decoded.is_some();
                    this.catalog
                        .thumbs
                        .finish(path, if keep { decoded } else { None });
                    this.pump_thumbnail_queue(cx);
                    cx.notify();
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
        if let Some(bridge) = &self.media_keys {
            let session = &self.active().session;
            let title = matches!(session.kind(), Some(Kind::Video | Kind::Audio))
                .then(|| {
                    session
                        .current()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .or_else(|| session.current_title().map(str::to_string))
                })
                .flatten();
            let st = session.state();
            bridge.update_now_playing(
                title.as_deref(),
                st.duration,
                st.time_pos,
                !st.paused && !st.ended,
                st.speed,
            );
        }
    }
}
