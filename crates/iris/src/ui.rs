//! The main window: title bar, docked directory, video area, control bar,
//! OSD, media info, context menus, popovers, command palette, and settings modal.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    Bounds, Context, CursorStyle, EntityInputHandler, ExternalPaths, FocusHandle,
    InteractiveElement, IntoElement, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement,
    Pixels, Point, Render, ScrollHandle, ScrollStrategy, ScrollWheelEvent, SharedString, Styled,
    Subscription, Task, UTF16Selection, UniformListScrollHandle, Window, div,
    prelude::FluentBuilder, px,
};

use iris_api::{HostCommand, HostEvent, PlaybackSnapshot};

use crate::geometry::ImageView;
use crate::input::{self, InputContext, PointerCommand, PointerInput};
use crate::library::{self, Entry, Kind, ScanResult, SortBy, SortOrder};
use crate::platform::{self, MediaKey, MediaKeyBridge, PinchMonitor, SleepPreventer};
use crate::player::{self, Command, MediaStats, Session, SessionEvent, TrackKind};
use crate::plugin::{self, PluginManager};
use crate::settings::{
    MAX_DIRECTORY_WIDTH, MIN_DIRECTORY_WIDTH, OnEof, PhotoDefault, Settings, Side, VideoWheel,
};
use crate::store::Store;
use crate::theme::*;
use crate::video::VideoSurface;

mod actions;
mod context_menu;
mod controls;
mod debug_cmd;
mod directory;
mod info;
mod palette;
mod plugin_ui;
mod popover;
mod settings;
mod state;
mod titlebar;
mod viewport;
mod widgets;

pub use actions::*;

use state::{
    DirectoryCache, DirectoryFilter, OverlayState, PopoverKind, ScrubState, SettingsTab,
    TextInputState, ThumbCache,
};
use viewport::Viewport;

/// While a video plays, dirty playback progress is persisted this often.
const SAVE_EVERY: Duration = Duration::from_secs(5);
/// Minimum interval between fast seeks while dragging the timeline.
const SEEK_DRAG_INTERVAL: Duration = Duration::from_millis(40);
/// Minimum interval between pixel-level timeline redraws on short clips (max 4 Hz;
/// long videos drop to 1 Hz once per displayed second, and hidden fullscreen chrome is 0 Hz).
const MIN_BAR_TICK_INTERVAL: Duration = Duration::from_millis(250);
/// Dynamic decoder stats refresh cadence while the Media Info overlay is open.
const STATS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

pub struct IrisWindow {
    focus: FocusHandle,
    search_focus: FocusHandle,
    palette_focus: FocusHandle,
    // A window owns viewports; each viewport owns one independent session/surface.
    viewports: Vec<Viewport>,
    show_directory: bool,
    sidebar_plugins: bool,
    plugins: PluginManager,
    overlays: OverlayState,
    scrub: ScrubState,
    dir_cache: DirectoryCache,
    thumbs: ThumbCache,
    search_open: bool,
    search_input: TextInputState,
    palette_input: TextInputState,
    palette_index: usize,
    palette_scroll: ScrollHandle,
    last_plugin_tick: Instant,
    last_render_sec: i64,
    last_render_bar_px: i32,
    last_bar_tick: Instant,
    last_scale: f64,
    cached_stats: Option<MediaStats>,
    last_stats_refresh: Instant,
    render_count: u64,
    toast: Option<SharedString>,
    toast_seq: u64,
    /// Lightweight non-intrusive OSD for keyboard seek / volume / pause.
    osd: Option<SharedString>,
    osd_seq: u64,
    progress_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    vol_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    anchor_sub: Rc<Cell<Option<Bounds<Pixels>>>>,
    anchor_audio: Rc<Cell<Option<Bounds<Pixels>>>>,
    anchor_speed: Rc<Cell<Option<Bounds<Pixels>>>>,
    anchor_more: Rc<Cell<Option<Bounds<Pixels>>>>,
    settings: Settings,
    /// Path-keyed favorites, playback positions and recent history.
    store: Store,
    filter: DirectoryFilter,
    /// Bumped per open request; a slower earlier scan never replaces a later one.
    load_generation: u64,
    /// The path being prepared in the background, for stepping before it loads.
    pending_path: Option<PathBuf>,
    /// Delayed loading indicator when background preparation takes >150ms.
    loading_visible: bool,
    /// When a photo opens with `PhotoDefault::Actual`, apply 1:1 once its size arrives.
    pending_photo_actual: bool,
    last_save: Instant,
    shutdown_done: bool,
    /// Mirrors `window.is_fullscreen()`, updated at the start of each render.
    fullscreen: bool,
    /// Toggle between total duration and remaining time on the clock readout.
    show_remaining: bool,
    /// Active directory width drag: `(start_pointer_x, start_width)`.
    resizing_dir: Option<(f32, f32)>,
    /// Virtualized directory scroll handle.
    dir_scroll: UniformListScrollHandle,
    /// Last recorded windowed viewport size, persisted on exit.
    last_window_size: Option<(f32, f32)>,
    /// Announce track change via OSD after a keyboard cycle.
    announce_audio: bool,
    announce_sub: bool,
    /// Fullscreen only: whether the control bar, info chip and cursor are shown.
    chrome_visible: bool,
    last_activity: Instant,
    /// A hide timer is running.
    hide_armed: bool,
    mouse_pos: Option<Point<Pixels>>,
    /// The fullscreen control bar's frame, for keeping the chrome up while
    /// the pointer rests on it.
    dock_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// OS idle sleep prevention while video plays.
    sleep_preventer: SleepPreventer,
    /// Native trackpad pinch gesture recognizer (macOS).
    _pinch_monitor: Option<PinchMonitor>,
    /// Native OS media keys & Now Playing bridge (macOS).
    media_keys: Option<MediaKeyBridge>,
    last_now_playing: Instant,
    _quit: Subscription,
    _events: Task<()>,
    _plugin_events: Task<()>,
}

impl IrisWindow {
    pub fn new(
        session: Session,
        settings: Settings,
        initial: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let video = match VideoSurface::attach(window, session.render_handle()) {
            Ok(v) => Some(v),
            Err(e) => {
                log::error!("video surface failed: {e:#}");
                cx.quit();
                None
            }
        };
        platform::hide_traffic_lights(window);
        if settings.always_on_top {
            platform::set_always_on_top(window, true);
        }
        let focus = cx.focus_handle();
        let search_focus = cx.focus_handle();
        let palette_focus = cx.focus_handle();
        focus.focus(window);

        let mut rx = session.events();
        let events = cx.spawn(async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(ev) = rx.try_recv() {
                    batch.push(ev);
                    if batch.len() >= 64 {
                        break;
                    }
                }
                if this
                    .update(cx, |this, cx| this.on_events(batch, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let (plugins, mut plugin_wake_rx) =
            PluginManager::load_with_wake(&settings.disabled_plugins);
        let plugin_events = cx.spawn(async move |this, cx| {
            while plugin_wake_rx.next().await.is_some() {
                let ok = this.update(cx, |this, cx| {
                    let (cmds, changed) = this.plugins.poll();
                    let has_cmds = !cmds.is_empty();
                    if has_cmds {
                        this.execute_plugin_commands(cmds, cx);
                    }
                    if changed || has_cmds {
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        });

        let pinch_monitor = if let Some((monitor, mut pinch_rx)) = PinchMonitor::attach(window) {
            cx.spawn(async move |this, cx| {
                while let Some(ev) = pinch_rx.next().await {
                    let ok = this.update(cx, |this, cx| {
                        let pt = Point::new(px(ev.position[0] as f32), px(ev.position[1] as f32));
                        if let Some(PointerCommand::Zoom(delta)) = input::resolve_pointer_input(
                            PointerInput::Pinch(ev.delta),
                            this.input_context(),
                        ) {
                            this.zoom_at(delta, Some(pt), cx);
                        }
                    });
                    if ok.is_err() {
                        break;
                    }
                }
            })
            .detach();
            Some(monitor)
        } else {
            None
        };

        let media_keys = if let Some((bridge, mut mk_rx)) = MediaKeyBridge::attach() {
            cx.spawn(async move |this, cx| {
                while let Some(key) = mk_rx.next().await {
                    let ok = this.update(cx, |this, cx| match key {
                        MediaKey::Play => {
                            if this.active().session.state.paused {
                                this.command(Command::TogglePlayback, cx);
                            }
                        }
                        MediaKey::Pause => {
                            if !this.active().session.state.paused {
                                this.command(Command::TogglePlayback, cx);
                            }
                        }
                        MediaKey::Toggle => this.command(Command::TogglePlayback, cx),
                        MediaKey::Next => this.step(1, cx),
                        MediaKey::Prev => this.step(-1, cx),
                    });
                    if ok.is_err() {
                        break;
                    }
                }
            })
            .detach();
            Some(bridge)
        } else {
            None
        };

        let quit = cx.on_app_quit(|this, _| {
            this.shutdown();
            async {}
        });
        if let Some(mut control) = crate::debug::control_channel() {
            cx.spawn_in(window, async move |this, cx| {
                while let Some(line) = control.next().await {
                    if let Some(arg) = line.strip_prefix("key ") {
                        let arg = arg.trim().to_string();
                        let _ = cx.update(|window, cx| match gpui::Keystroke::parse(&arg) {
                            Ok(k) => {
                                let handled = window.dispatch_keystroke(k, cx);
                                eprintln!("[iris/control] key {arg} handled={handled}");
                            }
                            Err(e) => eprintln!("[iris/control] bad keystroke {arg}: {e:?}"),
                        });
                        continue;
                    }
                    if let Some((c, x, y)) = debug_cmd::parse_mouse_command(&line) {
                        let send = cx.update(|window, _| {
                            platform::synth_mouse(window, c == "click", x as f64, y as f64)
                        });
                        if let Ok(Some(send)) = send {
                            send();
                            eprintln!("[iris/control] {c} {x} {y}");
                        }
                        continue;
                    }
                    if this
                        .update_in(cx, |this, window, cx| this.debug_command(&line, window, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }

        let show_directory = settings.directory_open;
        let initial_scale = window.scale_factor() as f64;
        let now = Instant::now();
        let mut this = Self {
            focus,
            search_focus,
            palette_focus,
            viewports: vec![Viewport {
                session,
                video: Rc::new(RefCell::new(video)),
                rect: Rc::new(Cell::new(None)),
                image: ImageView::default(),
                drag_origin: None,
            }],
            show_directory,
            sidebar_plugins: false,
            plugins,
            overlays: OverlayState::default(),
            scrub: ScrubState::default(),
            dir_cache: DirectoryCache::default(),
            thumbs: ThumbCache::default(),
            search_open: false,
            search_input: TextInputState::default(),
            palette_input: TextInputState::default(),
            palette_index: 0,
            palette_scroll: ScrollHandle::new(),
            last_plugin_tick: now,
            last_render_sec: -1,
            last_render_bar_px: -1,
            last_bar_tick: now,
            last_scale: initial_scale,
            cached_stats: None,
            last_stats_refresh: now,
            render_count: 0,
            toast: None,
            toast_seq: 0,
            osd: None,
            osd_seq: 0,
            progress_bounds: Rc::new(Cell::new(None)),
            vol_bounds: Rc::new(Cell::new(None)),
            anchor_sub: Rc::new(Cell::new(None)),
            anchor_audio: Rc::new(Cell::new(None)),
            anchor_speed: Rc::new(Cell::new(None)),
            anchor_more: Rc::new(Cell::new(None)),
            settings,
            store: Store::load(),
            filter: DirectoryFilter::All,
            load_generation: 0,
            pending_path: None,
            loading_visible: false,
            pending_photo_actual: false,
            last_save: now,
            shutdown_done: false,
            fullscreen: false,
            show_remaining: false,
            resizing_dir: None,
            dir_scroll: UniformListScrollHandle::new(),
            last_window_size: None,
            announce_audio: false,
            announce_sub: false,
            chrome_visible: true,
            last_activity: now,
            hide_armed: false,
            mouse_pos: None,
            dock_bounds: Rc::new(Cell::new(None)),
            sleep_preventer: SleepPreventer::new(),
            _pinch_monitor: pinch_monitor,
            media_keys,
            last_now_playing: now - Duration::from_secs(10),
            _quit: quit,
            _events: events,
            _plugin_events: plugin_events,
        };
        this.rebuild_directory_rows();
        this.active().session.apply_settings(&this.settings);
        if let Some(v) = this.settings.volume {
            this.command(Command::SetVolume(v), cx);
        }
        if this.settings.remember_speed
            && let Some(v) = this.settings.speed
        {
            this.command(Command::SetSpeed(v), cx);
        }
        if let Some(path) = initial {
            this.open_path(&path, cx);
        }
        this
    }

    #[inline]
    fn active(&self) -> &Viewport {
        &self.viewports[0]
    }

    #[inline]
    fn active_mut(&mut self) -> &mut Viewport {
        &mut self.viewports[0]
    }

    fn popover_anchor(&self, kind: PopoverKind) -> Option<Bounds<Pixels>> {
        match kind {
            PopoverKind::Subtitles => self.anchor_sub.get(),
            PopoverKind::Audio => self.anchor_audio.get(),
            PopoverKind::Speed => self.anchor_speed.get(),
            PopoverKind::More => self.anchor_more.get(),
        }
    }

    fn shutdown(&mut self) {
        if self.shutdown_done {
            return;
        }
        self.shutdown_done = true;
        self.sleep_preventer.set_active(false);
        self.save_progress_sync();
        let (volume, speed) = {
            let state = &self.active().session.state;
            (state.volume.clamp(0., 100.), state.speed.clamp(0.25, 4.))
        };
        self.settings.volume = Some(volume);
        if self.settings.remember_speed {
            self.settings.speed = Some(speed);
        }
        self.settings.directory_open = self.show_directory;
        if self.settings.remember_window_size
            && let Some((w, h)) = self.last_window_size
        {
            self.settings.window_width = Some(w);
            self.settings.window_height = Some(h);
        }
        self.settings.save();
        drop(self.active().video.borrow_mut().take());
        self.active().session.shutdown();
    }

    fn record_progress_memory(&mut self) {
        if !self.settings.resume_playback {
            return;
        }
        if let Some((path, position, duration, ended)) = self.active().session.progress() {
            let path = path.to_path_buf();
            self.store.remember(&path, position, duration, ended);
        }
    }

    fn save_progress_sync(&mut self) {
        self.record_progress_memory();
        if self.store.is_dirty() {
            self.store.save();
        }
        self.last_save = Instant::now();
    }

    fn save_progress_async(&mut self, cx: &mut Context<Self>) {
        self.record_progress_memory();
        if let Some((path, snapshot)) = self.store.take_dirty_snapshot() {
            cx.background_executor()
                .spawn(async move { Store::write_snapshot(&path, &snapshot) })
                .detach();
        }
        self.last_save = Instant::now();
    }

    fn finish_media_open(&mut self, cx: &mut Context<Self>) {
        if let Some(cur) = self.active().session.current.clone() {
            self.store.push_recent(&cur);
        } else if let Some(dir) = self.active().session.folder.clone() {
            self.store.push_recent(&dir);
        }
        self.pending_photo_actual = self.active().session.kind() == Some(Kind::Photo)
            && self.settings.photo_default == PhotoDefault::Actual;
        self.cached_stats = None;
        self.last_render_sec = -1;
        self.last_render_bar_px = -1;
        self.rebuild_directory_rows();
        self.scroll_directory_to_current();
        self.ensure_filmstrip_thumbnails(cx);
        self.sync_now_playing(true);
        let snap = self.plugin_snapshot();
        self.dispatch_plugin_event(HostEvent::FileOpened { snapshot: snap }, cx);
        if self.overlays.info_open {
            self.refresh_cached_stats(true);
        }
    }

    pub(crate) fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.save_progress_sync();
        self.overlays.close_menus();
        self.load_generation += 1;
        let generation = self.load_generation;
        self.pending_path = std::path::absolute(path).ok();
        self.loading_visible = false;
        let session = &self.active().session;
        let cached = session
            .folder
            .clone()
            .filter(|folder| {
                self.pending_path.as_deref().and_then(Path::parent) == Some(folder.as_path())
            })
            .map(|folder| {
                (
                    folder,
                    ScanResult {
                        entries: session.entries.clone(),
                        subfolders: session.subfolders.clone(),
                    },
                )
            });
        let path = path.to_path_buf();
        let task = cx
            .background_executor()
            .spawn(async move { player::prepare(&path, cached) });

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.load_generation == generation && this.pending_path.is_some() {
                    this.loading_visible = true;
                    cx.notify();
                }
            });
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let prepared = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.pending_path = None;
                this.loading_visible = false;
                let result = prepared.and_then(|prepared| {
                    let resume = if this.settings.resume_playback {
                        prepared.target().map_or(0., |p| this.store.resume(p))
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

    fn open_multi_queue(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.save_progress_sync();
        self.overlays.close_menus();
        self.load_generation += 1;
        let generation = self.load_generation;
        self.pending_path = paths.first().and_then(|p| std::path::absolute(p).ok());
        self.loading_visible = false;

        let task = cx
            .background_executor()
            .spawn(async move { player::prepare_queue(paths) });

        cx.spawn(async move |this, cx| {
            let prepared = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.pending_path = None;
                this.loading_visible = false;
                let Some(prepared) = prepared else {
                    return;
                };
                let count = prepared.len();
                let resume = if this.settings.resume_playback {
                    prepared.target().map_or(0., |p| this.store.resume(p))
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
                        this.show_toast(format!("已加入临时播放队列 · {count} 个文件"), cx);
                    }
                    Err(e) => this.show_toast(format!("无法打开 · {e}"), cx),
                }
                this.active_mut().image.reset();
                cx.notify();
            });
        })
        .detach();
    }

    fn ensure_filmstrip_thumbnails(&mut self, cx: &mut Context<Self>) {
        if self.active().session.kind() != Some(Kind::Photo) || !self.settings.photo_filmstrip {
            return;
        }
        let photos = &self.dir_cache.photo_entries;
        if photos.len() < 2 {
            return;
        }
        let cur_ix = self
            .active()
            .session
            .current
            .as_ref()
            .and_then(|p| photos.iter().position(|x| &x.path == p))
            .unwrap_or(0);
        // Prioritize thumbnails closest to the current photo first.
        let mut prioritized = Vec::with_capacity(13);
        prioritized.push(photos[cur_ix].path.clone());
        for d in 1..=6usize {
            if let Some(e) = photos.get(cur_ix + d) {
                prioritized.push(e.path.clone());
            }
            if cur_ix >= d {
                prioritized.push(photos[cur_ix - d].path.clone());
            }
        }
        self.thumbs.enqueue_window(prioritized);
        self.pump_thumbnail_queue(cx);
    }

    fn pump_thumbnail_queue(&mut self, cx: &mut Context<Self>) {
        for (batch_gen, path) in self.thumbs.pop_next_batch() {
            let p_decode = path.clone();
            let task = cx
                .background_executor()
                .spawn(async move { platform::decode_thumbnail(&p_decode, 160) });
            cx.spawn(async move |this, cx| {
                let decoded = task.await;
                let _ = this.update(cx, |this, cx| {
                    let keep = this.thumbs.generation() == batch_gen || decoded.is_some();
                    this.thumbs.finish(path, if keep { decoded } else { None });
                    this.pump_thumbnail_queue(cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    pub(super) fn refresh_cached_stats(&mut self, full: bool) {
        if full || self.cached_stats.is_none() {
            self.cached_stats = Some(self.active().session.media_stats());
        } else if let Some(stats) = &mut self.cached_stats {
            self.viewports[0].session.refresh_dynamic_stats(stats);
        }
        self.last_stats_refresh = Instant::now();
    }

    fn scroll_directory_to_current(&self) {
        let Some(current) = self.active().session.current.as_ref() else {
            return;
        };
        if let Some(ix) = self
            .dir_cache
            .rows
            .iter()
            .position(|r| matches!(r, directory::DirRow::Media(e) if &e.path == current))
        {
            self.dir_scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
    }

    /// Computes the sorted directory listing under the current filter & sort order.
    /// Cached into `self.dir_cache.sorted_entries` by `rebuild_directory_rows()`.
    fn compute_sorted_entries(&self) -> Vec<Entry> {
        let session = &self.active().session;
        let mut entries: Vec<Entry> = match self.filter {
            DirectoryFilter::Favorites => self
                .store
                .records
                .iter()
                .filter(|(_, r)| r.favorite)
                .filter_map(|(path, _)| {
                    Some(Entry {
                        path: path.clone(),
                        name: path.file_name()?.to_string_lossy().into_owned(),
                        kind: library::kind_of(path).unwrap_or(Kind::Video),
                        size: 0,
                        modified_secs: 0,
                    })
                })
                .collect(),
            filter => session
                .entries
                .iter()
                .filter(|e| match filter {
                    DirectoryFilter::Videos => matches!(e.kind, Kind::Video | Kind::Audio),
                    DirectoryFilter::Photos => e.kind == Kind::Photo,
                    _ => true,
                })
                .cloned()
                .collect(),
        };
        library::sort_entries(
            &mut entries,
            self.settings.sort_by,
            self.settings.sort_order,
        );
        entries
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let entries = &self.dir_cache.sorted_entries;
        if entries.is_empty() {
            return;
        }
        let current = self
            .pending_path
            .as_ref()
            .or(self.active().session.current.as_ref())
            .and_then(|p| entries.iter().position(|e| &e.path == p));
        let next = current
            .map(|i| (i as isize + delta).rem_euclid(entries.len() as isize) as usize)
            .unwrap_or(if delta < 0 { entries.len() - 1 } else { 0 });
        let target = entries[next].path.clone();
        self.open_path(&target, cx);
    }

    fn step_next_no_wrap(&mut self, cx: &mut Context<Self>) {
        let entries = &self.dir_cache.sorted_entries;
        let current = self
            .active()
            .session
            .current
            .as_ref()
            .and_then(|p| entries.iter().position(|e| &e.path == p));
        if let Some(i) = current
            && i + 1 < entries.len()
        {
            let target = entries[i + 1].path.clone();
            self.open_path(&target, cx);
        } else {
            self.show_toast("已播完当前列表", cx);
        }
    }

    fn command(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(e) = self.active_mut().session.execute(command) {
            self.show_toast(format!("操作失败 · {e}"), cx);
        }
    }

    fn show_toast(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast = Some(msg.into());
        self.toast_seq += 1;
        let seq = self.toast_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1800))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.toast_seq == seq {
                    this.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Lightweight OSD feedback that does NOT wake the fullscreen toolbar or shift subtitles.
    fn show_osd(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.osd = Some(msg.into());
        self.osd_seq += 1;
        let seq = self.osd_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.osd_seq == seq {
                    this.osd = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Damps MacBook trackpad inertia so volume scroll feels controlled while
    /// discrete mouse wheels remain crisp.
    fn damped_volume_delta(&mut self, ev: &ScrollWheelEvent) -> f64 {
        let raw_y = f32::from(ev.delta.pixel_delta(px(20.)).y) as f64;
        if ev.delta.precise() {
            self.scrub.wheel_accum += raw_y * 0.14;
            let whole = self.scrub.wheel_accum.trunc();
            if whole.abs() >= 1. {
                self.scrub.wheel_accum -= whole;
                whole.clamp(-8., 8.)
            } else {
                0.
            }
        } else {
            (raw_y / 10.).clamp(-10., 10.)
        }
    }

    fn damped_wheel_steps(&mut self, ev: &ScrollWheelEvent, threshold: f64) -> isize {
        let raw_y = f32::from(ev.delta.pixel_delta(px(20.)).y) as f64;
        if ev.delta.precise() {
            self.scrub.wheel_accum += raw_y;
            if self.scrub.wheel_accum.abs() >= threshold {
                let dir = if self.scrub.wheel_accum > 0. { 1 } else { -1 };
                self.scrub.wheel_accum = 0.;
                dir
            } else {
                0
            }
        } else if raw_y > 0. {
            1
        } else if raw_y < 0. {
            -1
        } else {
            0
        }
    }

    fn sync_now_playing(&mut self, force: bool) {
        if !force && self.last_now_playing.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_now_playing = Instant::now();
        if let Some(bridge) = &self.media_keys {
            let session = &self.active().session;
            let title = matches!(session.kind(), Some(Kind::Video | Kind::Audio))
                .then(|| {
                    session
                        .current
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                })
                .flatten();
            bridge.update_now_playing(
                title.as_deref(),
                session.state.duration,
                session.state.time_pos,
                !session.state.paused && !session.state.ended,
                session.state.speed,
            );
        }
    }

    // ---- fullscreen chrome & overlays ----

    fn hide_after(&self) -> Duration {
        Duration::from_millis(self.settings.fullscreen_hide_ms.clamp(1000, 10000))
    }

    fn sync_fullscreen(&mut self, window: &Window, cx: &mut Context<Self>) {
        let fs = window.is_fullscreen();
        if fs == self.fullscreen {
            return;
        }
        self.fullscreen = fs;
        self.overlays.dir_popup = false;
        self.overlays.close_menus();
        self.chrome_visible = true;
        if fs {
            self.bump_activity(cx);
        } else {
            platform::hide_traffic_lights(window);
        }
    }

    fn bump_activity(&mut self, cx: &mut Context<Self>) {
        self.last_activity = Instant::now();
        if !self.chrome_visible {
            self.chrome_visible = true;
            cx.notify();
        }
        if self.fullscreen && !self.hide_armed {
            self.hide_armed = true;
            cx.spawn(async move |this, cx| {
                while let Ok(Some(wait)) = this.update(cx, |this, cx| this.hide_check(cx)) {
                    cx.background_executor().timer(wait).await;
                }
            })
            .detach();
        }
    }

    fn hide_check(&mut self, cx: &mut Context<Self>) -> Option<Duration> {
        let over_dock = match (self.mouse_pos, self.dock_bounds.get()) {
            (Some(p), Some(b)) => b.contains(&p),
            _ => false,
        };
        if self.fullscreen
            && self.chrome_visible
            && !self.overlays.has_blocking()
            && self.resizing_dir.is_none()
            && !self.scrub.seeking
            && !self.scrub.dragging_volume
            && !over_dock
        {
            let delay = self.hide_after();
            let idle = self.last_activity.elapsed();
            if idle < delay {
                return Some(delay - idle);
            }
            self.chrome_visible = false;
            cx.notify();
        }
        self.hide_armed = false;
        None
    }

    fn chrome_shown(&self) -> bool {
        !self.fullscreen || self.chrome_visible
    }

    fn close_overlays(&mut self, cx: &mut Context<Self>) {
        self.overlays.close_all();
        self.bump_activity(cx);
        cx.notify();
    }

    // ---- plugin control-plane helpers ----

    fn plugin_snapshot(&self) -> PlaybackSnapshot {
        let session = &self.active().session;
        let fav = session
            .current
            .as_deref()
            .is_some_and(|p| self.store.favorite(p));
        plugin::snapshot_from_session(session, fav)
    }

    fn dispatch_plugin_event(&mut self, event: HostEvent, cx: &mut Context<Self>) {
        let cmds = self.plugins.dispatch(&event);
        self.execute_plugin_commands(cmds, cx);
    }

    fn execute_plugin_commands(&mut self, commands: Vec<HostCommand>, cx: &mut Context<Self>) {
        for cmd in commands {
            match cmd {
                HostCommand::TogglePlayback => self.command(Command::TogglePlayback, cx),
                HostCommand::SeekRelative { seconds } => {
                    self.command(Command::SeekRelative(seconds), cx)
                }
                HostCommand::SeekAbsolute { seconds } => {
                    self.command(Command::SeekAbsolute(seconds), cx)
                }
                HostCommand::SetSpeed { speed } => self.command(Command::SetSpeed(speed), cx),
                HostCommand::SetVolume { volume } => self.command(Command::SetVolume(volume), cx),
                HostCommand::ToggleMute => self.command(Command::ToggleMute, cx),
                HostCommand::OpenNext => self.step(1, cx),
                HostCommand::OpenPrevious => self.step(-1, cx),
                HostCommand::ToggleFavorite => {
                    if let Some(path) = self.active().session.current.clone() {
                        self.store.toggle_favorite(&path);
                        self.rebuild_directory_rows();
                    }
                }
                HostCommand::ShowToast { message } => self.show_toast(message, cx),
                HostCommand::SetSidebar { .. }
                | HostCommand::SetStatus { .. }
                | HostCommand::SetOverlay { .. }
                | HostCommand::ClearOverlay => {}
            }
        }
    }

    fn invoke_plugin_command(
        &mut self,
        plugin_id: &str,
        command_id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snap = self.plugin_snapshot();
        let cmds = self.plugins.invoke_command(plugin_id, command_id, snap);
        self.execute_plugin_commands(cmds, cx);
        cx.notify();
    }

    fn invoke_plugin_action(
        &mut self,
        plugin_id: &str,
        action_id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snap = self.plugin_snapshot();
        let cmds = self.plugins.invoke_action(plugin_id, action_id, snap);
        self.execute_plugin_commands(cmds, cx);
        cx.notify();
    }

    fn toggle_plugin_enabled(&mut self, id: &str, enabled: bool, cx: &mut Context<Self>) {
        if enabled {
            self.settings.disabled_plugins.retain(|d| d != id);
        } else if !self.settings.disabled_plugins.iter().any(|d| d == id) {
            self.settings.disabled_plugins.push(id.to_string());
        }
        self.settings.save();
        self.plugins.set_enabled(id, enabled);
        if enabled {
            let snap = self.plugin_snapshot();
            self.dispatch_plugin_event(HostEvent::StateChanged { snapshot: snap }, cx);
        }
        cx.notify();
    }

    fn reload_plugins(&mut self, cx: &mut Context<Self>) {
        self.plugins.reload(&self.settings.disabled_plugins);
        let snap = self.plugin_snapshot();
        self.dispatch_plugin_event(HostEvent::StateChanged { snapshot: snap }, cx);
        self.show_toast("插件列表已重新扫描", cx);
    }

    fn toggle_popover(&mut self, kind: PopoverKind, cx: &mut Context<Self>) {
        self.overlays.toggle_popover(kind);
        self.bump_activity(cx);
        cx.notify();
    }

    fn set_directory_side(&mut self, side: Side, cx: &mut Context<Self>) {
        if self.settings.directory_side == side {
            return;
        }
        self.settings.directory_side = side;
        self.settings.save();
        self.show_toast(
            if side == Side::Left {
                "目录移到左侧"
            } else {
                "目录移到右侧"
            },
            cx,
        );
    }

    fn cycle_sort(&mut self, cx: &mut Context<Self>) {
        let (next_by, next_ord) = match (self.settings.sort_by, self.settings.sort_order) {
            (SortBy::Name, SortOrder::Asc) => (SortBy::Name, SortOrder::Desc),
            (SortBy::Name, SortOrder::Desc) => (SortBy::Modified, SortOrder::Desc),
            (SortBy::Modified, SortOrder::Desc) => (SortBy::Modified, SortOrder::Asc),
            (SortBy::Modified, SortOrder::Asc) => (SortBy::Size, SortOrder::Desc),
            (SortBy::Size, SortOrder::Desc) => (SortBy::Size, SortOrder::Asc),
            (SortBy::Size, SortOrder::Asc) => (SortBy::Name, SortOrder::Asc),
        };
        self.settings.sort_by = next_by;
        self.settings.sort_order = next_ord;
        self.settings.save();
        self.rebuild_directory_rows();
        let label = format!(
            "排序 · {} {}",
            match next_by {
                SortBy::Name => "名称",
                SortBy::Modified => "修改时间",
                SortBy::Size => "文件大小",
            },
            match next_ord {
                SortOrder::Asc => "升序",
                SortOrder::Desc => "降序",
            }
        );
        self.show_toast(label, cx);
    }

    /// Processes a coalesced batch of mpv events in one UI update, avoiding
    /// redundant metadata queries and intermediate repaints.
    fn on_events(&mut self, batch: Vec<SessionEvent>, cx: &mut Context<Self>) {
        let was_ended = self.active().session.state.ended;
        let mut need_notify = false;
        let mut saw_position: Option<f64> = None;
        let mut saw_pause_change = false;
        let mut saw_speed_change = false;
        let mut saw_loaded_or_reconfig = false;
        let mut saw_ended_now = false;
        let mut saw_audio = false;
        let mut saw_sub = false;
        let mut last_error: Option<i32> = None;

        for ev in &batch {
            self.active_mut().session.apply(ev);
            match ev {
                SessionEvent::Position(t) => saw_position = Some(*t),
                SessionEvent::Paused(_) => {
                    saw_pause_change = true;
                    need_notify = true;
                }
                SessionEvent::Speed(_) => {
                    saw_speed_change = true;
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
        // Flush deferred track-list / chapter-list JSON queries at most once per batch.
        self.active_mut().session.flush_deferred();

        self.sleep_preventer
            .set_active(self.active().session.is_actively_playing_video());
        if self.last_save.elapsed() >= SAVE_EVERY {
            self.save_progress_async(cx);
        }

        if saw_loaded_or_reconfig && self.overlays.info_open {
            self.refresh_cached_stats(true);
        }

        if let Some(t) = saw_position {
            if self.overlays.info_open
                && self.last_stats_refresh.elapsed() >= STATS_REFRESH_INTERVAL
            {
                self.refresh_cached_stats(false);
                need_notify = true;
            }
            // Repaint only when the displayed second changes OR the timeline bar
            // advances by >= 1 physical pixel (capped at 4 Hz on short clips,
            // dropping to 1 Hz on long videos and 0 Hz when fullscreen chrome hides).
            if self.chrome_shown() || self.active().session.kind() == Some(Kind::Audio) {
                let sec = t.max(0.).floor() as i64;
                let duration = self.active().session.state.duration;
                let bar_px = if duration > 0. {
                    let logical_w = self
                        .progress_bounds
                        .get()
                        .map_or(800., |b| f32::from(b.size.width).max(1.) as f64);
                    ((t / duration).clamp(0., 1.) * logical_w * self.last_scale).round() as i32
                } else {
                    -1
                };
                let sec_changed = sec != self.last_render_sec;
                let bar_moved = bar_px != self.last_render_bar_px
                    && self.last_bar_tick.elapsed() >= MIN_BAR_TICK_INTERVAL;
                if sec_changed || bar_moved {
                    self.last_render_sec = sec;
                    self.last_render_bar_px = bar_px;
                    self.last_bar_tick = Instant::now();
                    need_notify = true;
                }
            }
            if self.last_plugin_tick.elapsed() >= Duration::from_millis(200) {
                self.last_plugin_tick = Instant::now();
                let state = &self.active().session.state;
                let ev = HostEvent::PositionTick {
                    position: state.time_pos,
                    duration: state.duration,
                    paused: state.paused,
                };
                self.dispatch_plugin_event(ev, cx);
            }
            self.sync_now_playing(false);
        }

        if saw_pause_change || saw_speed_change {
            self.sync_now_playing(true);
            let snap = self.plugin_snapshot();
            self.dispatch_plugin_event(HostEvent::StateChanged { snapshot: snap }, cx);
        }

        if saw_ended_now && !was_ended && self.active().session.state.ended {
            self.save_progress_async(cx);
            self.sync_now_playing(true);
            if matches!(
                self.active().session.kind(),
                Some(Kind::Video | Kind::Audio)
            ) && self.settings.on_eof == OnEof::Next
                && !self.active().session.state.loop_file
            {
                self.step_next_no_wrap(cx);
            } else {
                need_notify = true;
            }
        }

        if saw_audio && self.announce_audio {
            self.announce_audio = false;
            let label = self
                .active()
                .session
                .selected_track(TrackKind::Audio)
                .map(|t| t.label())
                .unwrap_or_else(|| "无音轨".into());
            self.show_osd(format!("音轨 · {label}"), cx);
            need_notify = false; // show_osd already called cx.notify()
        }

        if saw_sub && self.announce_sub {
            self.announce_sub = false;
            let label = self
                .active()
                .session
                .selected_track(TrackKind::Sub)
                .map(|t| t.label())
                .unwrap_or_else(|| "无字幕".into());
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

    // ---- image geometry & transforms ----

    fn image_geometry(&self) -> ([f64; 2], [f64; 2]) {
        let rect = self.active().rect.get().map(|r| r.0).unwrap_or([0.; 4]);
        let state = &self.active().session.state;
        let (mut w, mut h) = state.video_size;
        if state.rotation.rem_euclid(180) == 90 {
            std::mem::swap(&mut w, &mut h);
        }
        ([rect[2], rect[3]], [w.max(1) as f64, h.max(1) as f64])
    }

    fn apply_image_view(&mut self, cx: &mut Context<Self>) {
        let view = self.active();
        if let Err(e) = view.session.transform(view.image.zoom, view.image.pan) {
            self.show_toast(format!("缩放失败 · {e}"), cx);
        }
        cx.notify();
    }

    fn zoom_at(&mut self, delta: f64, point: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let (viewport, media) = self.image_geometry();
        let rect = self.active().rect.get().map(|r| r.0).unwrap_or([0.; 4]);
        let point = point
            .map(|p| {
                [
                    f32::from(p.x) as f64 - rect[0],
                    f32::from(p.y) as f64 - rect[1],
                ]
            })
            .unwrap_or([viewport[0] / 2., viewport[1] / 2.]);
        self.active_mut()
            .image
            .zoom_at(delta, point, viewport, media);
        self.apply_image_view(cx);
    }

    fn input_context(&self) -> InputContext {
        InputContext {
            media: self.active().session.kind(),
            zoomed: self.active().image.zoomed(),
            overlay: self.overlays.has_blocking(),
            video_wheel_volume: self.settings.video_wheel == VideoWheel::Volume,
            single_click_pause: self.settings.single_click_pause,
            photo_double_click_actual: self.settings.photo_double_click_actual,
        }
    }

    fn media_info(&self) -> Option<String> {
        let session = &self.active().session;
        let kind = session.kind()?;
        let state = &session.state;
        let (w, h) = state.video_size;
        let mut parts = Vec::new();
        if w > 0 && h > 0 && kind != Kind::Audio {
            parts.push(format!("{w}×{h}"));
        }
        match kind {
            Kind::Video => {
                parts.push(if state.hwdec.is_empty() || state.hwdec == "no" {
                    "软解".into()
                } else {
                    format!("硬解 {}", state.hwdec)
                });
            }
            Kind::Photo => {
                if let Some(colour) = state.photo_colour {
                    parts.push(colour.to_string());
                }
            }
            Kind::Audio => {
                parts.push("音频".into());
            }
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    fn update_seek_preview(&mut self, x: Pixels) {
        let Some(b) = self.progress_bounds.get() else {
            return;
        };
        if b.size.width <= px(0.) {
            return;
        }
        let ratio = ((x - b.origin.x) / b.size.width).clamp(0., 1.);
        self.scrub.seek_preview_ratio = Some(ratio);
    }

    fn seek_to_x(&mut self, x: Pixels, exact: bool) {
        let Some(b) = self.progress_bounds.get() else {
            return;
        };
        if self.active().session.state.duration <= 0. || b.size.width <= px(0.) {
            return;
        }
        let ratio = ((x - b.origin.x) / b.size.width).clamp(0., 1.) as f64;
        let cmd = if exact {
            Command::SeekPercent(ratio * 100.)
        } else {
            Command::SeekPercentFast(ratio * 100.)
        };
        if let Err(e) = self.active_mut().session.execute(cmd) {
            log::error!("seek: {e}");
        }
    }

    fn set_volume_from_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some(b) = self.vol_bounds.get() else {
            return;
        };
        if b.size.width <= px(0.) {
            return;
        }
        let ratio = ((x - b.origin.x) / b.size.width).clamp(0., 1.) as f64;
        let vol = (ratio * 100.).round();
        self.command(Command::SetVolume(vol), cx);
        cx.notify();
    }

    /// Identifies which single-line text input is active for IME delivery.
    fn active_text_input(&self, window: &Window) -> Option<ActiveInput> {
        if self.overlays.palette_open && self.palette_focus.is_focused(window) {
            Some(ActiveInput::Palette)
        } else if self.search_open && self.search_focus.is_focused(window) {
            Some(ActiveInput::Search)
        } else {
            None
        }
    }

    fn active_input_mut(&mut self, which: ActiveInput) -> &mut TextInputState {
        match which {
            ActiveInput::Search => &mut self.search_input,
            ActiveInput::Palette => &mut self.palette_input,
        }
    }

    fn active_input_ref(&self, which: ActiveInput) -> &TextInputState {
        match which {
            ActiveInput::Search => &self.search_input,
            ActiveInput::Palette => &self.palette_input,
        }
    }

    fn on_text_input_changed(&mut self, which: ActiveInput, cx: &mut Context<Self>) {
        match which {
            ActiveInput::Search => self.rebuild_directory_rows(),
            ActiveInput::Palette => {
                self.palette_index = 0;
                self.palette_scroll.scroll_to_item(0);
            }
        }
        cx.notify();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveInput {
    Search,
    Palette,
}

impl EntityInputHandler for IrisWindow {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let which = self.active_text_input(window)?;
        let input = self.active_input_ref(which);
        let range = input.range_from_utf16(&range_utf16);
        adjusted_range.replace(input.range_to_utf16(&range));
        Some(input.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let which = self.active_text_input(window)?;
        let input = self.active_input_ref(which);
        Some(UTF16Selection {
            range: input.range_to_utf16(&input.selected_range),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let which = self.active_text_input(window)?;
        let input = self.active_input_ref(which);
        input.marked_range.as_ref().map(|r| input.range_to_utf16(r))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(which) = self.active_text_input(window) {
            self.active_input_mut(which).marked_range = None;
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(which) = self.active_text_input(window) else {
            return;
        };
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        self.active_input_mut(which).replace_utf16(range, &clean);
        self.on_text_input_changed(which, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(which) = self.active_text_input(window) else {
            return;
        };
        self.active_input_mut(which)
            .replace_and_mark_utf16(range, new_text, new_selected_range);
        self.on_text_input_changed(which, cx);
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Anchor OS IME candidate window cleanly right under the input field.
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let which = self.active_text_input(window)?;
        let input = self.active_input_ref(which);
        Some(input.offset_to_utf16(input.content.len()))
    }
}

impl Render for IrisWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_count += 1;
        self.sync_fullscreen(window, cx);
        let fullscreen = self.fullscreen;
        if !fullscreen && !window.is_maximized() {
            let sz = window.viewport_size();
            let (w, h) = (f32::from(sz.width), f32::from(sz.height));
            if w >= 480. && h >= 300. {
                self.last_window_size = Some((w, h));
            }
        }
        if self.pending_photo_actual
            && self.active().session.kind() == Some(Kind::Photo)
            && self.active().session.state.video_size.0 > 0
            && self.active().rect.get().is_some()
        {
            self.pending_photo_actual = false;
            let (viewport, media) = self.image_geometry();
            self.active_mut().image.toggle_actual_size(
                viewport,
                media,
                window.scale_factor() as f64,
            );
            let view = self.active();
            let _ = view.session.transform(view.image.zoom, view.image.pan);
        }

        let scale = window.scale_factor() as f64;
        self.last_scale = scale;
        if let Some(v) = self.active().video.borrow_mut().as_mut() {
            v.set_scale(scale);
        }

        let inset = if fullscreen && self.chrome_visible {
            (DOCK_H + FLOAT_GAP + 8.) as f64
        } else {
            0.
        };
        let height = if fullscreen {
            f32::from(window.viewport_size().height) as f64
        } else {
            self.active().rect.get().map(|r| r.0[3]).unwrap_or(0.)
        };
        self.active_mut().session.subtitle_inset(inset, height);

        div()
            .id("root")
            .relative()
            .key_context("Workspace")
            .capture_any_mouse_up(cx.listener(|this, _, window, _| {
                // Keep keyboard focus in text inputs while typing there; everywhere else returns to viewport.
                if !this.search_focus.is_focused(window) && !this.palette_focus.is_focused(window) {
                    this.focus.focus(window);
                }
            }))
            .on_any_mouse_down(cx.listener(|this, _, window, _| {
                // Clicking inside search_bar or palette_box stops propagation before reaching root;
                // clicking anywhere else returns keyboard focus to the main viewport.
                this.focus.focus(window);
            }))
            .size_full()
            .flex()
            .flex_col()
            .font_family(FONT_UI)
            .text_color(text())
            .on_action(cx.listener(Self::toggle_pause))
            .on_action(cx.listener(Self::seek_back))
            .on_action(cx.listener(Self::seek_forward))
            .on_action(cx.listener(Self::seek_back_large))
            .on_action(cx.listener(Self::seek_forward_large))
            .on_action(cx.listener(Self::frame_back))
            .on_action(cx.listener(Self::frame_forward))
            .on_action(cx.listener(Self::volume_up))
            .on_action(cx.listener(Self::volume_down))
            .on_action(cx.listener(Self::toggle_mute))
            .on_action(cx.listener(Self::speed_down))
            .on_action(cx.listener(Self::speed_up))
            .on_action(cx.listener(Self::reset_speed))
            .on_action(cx.listener(Self::toggle_loop))
            .on_action(cx.listener(Self::toggle_directory))
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(Self::refresh_directory))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::open_command_palette))
            .on_action(cx.listener(Self::toggle_always_on_top))
            .on_action(cx.listener(Self::toggle_info))
            .on_action(cx.listener(Self::take_screenshot))
            .on_action(cx.listener(Self::cycle_aspect))
            .on_action(cx.listener(Self::toggle_fullscreen))
            .on_action(cx.listener(Self::exit_fullscreen))
            .on_action(cx.listener(Self::toggle_subs))
            .on_action(cx.listener(Self::cycle_sub))
            .on_action(cx.listener(Self::sub_delay_back))
            .on_action(cx.listener(Self::sub_delay_forward))
            .on_action(cx.listener(Self::sub_delay_reset))
            .on_action(cx.listener(Self::cycle_audio))
            .on_action(cx.listener(Self::prev_chapter))
            .on_action(cx.listener(Self::next_chapter))
            .on_action(cx.listener(Self::toggle_image_size))
            .on_action(cx.listener(Self::fit_image))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::rotate_cw))
            .on_action(cx.listener(Self::rotate_ccw))
            .on_action(cx.listener(Self::toggle_hflip))
            .on_action(cx.listener(Self::toggle_vflip))
            .on_action(cx.listener(Self::toggle_filmstrip))
            .on_action(cx.listener(Self::next_item))
            .on_action(cx.listener(Self::prev_item))
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::toggle_favorite))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                let is_video = this.active().session.kind() == Some(Kind::Video);
                let dropped = paths.paths();
                if is_video {
                    for p in dropped {
                        if library::is_subtitle(p) {
                            this.command(Command::LoadSubtitle(p.clone()), cx);
                            let name = p
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            this.show_toast(format!("已加载字幕 · {name}"), cx);
                            return;
                        }
                    }
                }
                let media_files: Vec<PathBuf> = dropped
                    .iter()
                    .filter(|p| p.is_file() && library::kind_of(p).is_some())
                    .cloned()
                    .collect();
                if media_files.len() > 1 {
                    this.open_multi_queue(media_files, cx);
                } else if let Some(first) = dropped.first() {
                    this.open_path(first, cx);
                }
            }))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                this.mouse_pos = Some(ev.position);
                if let Some((start_x, start_w)) = this.resizing_dir {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        let dx = f32::from(ev.position.x) - start_x;
                        let signed = if this.settings.directory_side == Side::Left {
                            dx
                        } else {
                            -dx
                        };
                        let vw = f32::from(window.viewport_size().width);
                        let max = if vw > 0. {
                            (vw * 0.45).clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH)
                        } else {
                            MAX_DIRECTORY_WIDTH
                        };
                        this.settings.directory_width =
                            (start_w + signed).clamp(MIN_DIRECTORY_WIDTH, max);
                        cx.notify();
                    } else {
                        this.resizing_dir = None;
                        this.settings.save();
                        cx.notify();
                    }
                    return;
                }
                if this.scrub.dragging_volume {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        this.set_volume_from_x(ev.position.x, cx);
                    } else {
                        this.scrub.dragging_volume = false;
                        cx.notify();
                    }
                    return;
                }
                if this.scrub.seeking {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        this.update_seek_preview(ev.position.x);
                        if this.scrub.last_seek_drag.elapsed() >= SEEK_DRAG_INTERVAL {
                            this.scrub.last_seek_drag = Instant::now();
                            this.seek_to_x(ev.position.x, false);
                        }
                    } else {
                        this.scrub.seeking = false;
                        this.scrub.seek_preview_ratio = None;
                    }
                    cx.notify();
                } else if this.scrub.timeline_hover {
                    cx.notify();
                }
                if ev.pressed_button == Some(MouseButton::Left) {
                    if let Some(origin) = this.active().drag_origin {
                        this.active_mut().drag_origin = Some(ev.position);
                        let delta = [
                            f32::from(ev.position.x - origin.x) as f64,
                            f32::from(ev.position.y - origin.y) as f64,
                        ];
                        let (viewport, media) = this.image_geometry();
                        this.active_mut().image.pan_by(delta, viewport, media);
                        this.apply_image_view(cx);
                    }
                } else if this.active_mut().drag_origin.take().is_some() {
                    cx.notify();
                }
                if this.fullscreen {
                    this.bump_activity(cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                    if this.resizing_dir.take().is_some() {
                        this.settings.save();
                        cx.notify();
                    }
                    if this.scrub.dragging_volume {
                        this.scrub.dragging_volume = false;
                        cx.notify();
                    }
                    if this.active_mut().drag_origin.take().is_some() {
                        cx.notify();
                    }
                    if this.scrub.seeking {
                        this.scrub.seeking = false;
                        this.scrub.seek_preview_ratio = None;
                        this.seek_to_x(ev.position.x, true);
                        cx.notify();
                    }
                }),
            )
            .when(self.resizing_dir.is_some(), |d| {
                d.cursor(CursorStyle::ResizeLeftRight)
            })
            .when(!self.chrome_shown(), |d| d.cursor(CursorStyle::None))
            .when(!fullscreen, |d| d.child(self.render_titlebar(cx)))
            .child({
                let directory = (!fullscreen && self.show_directory)
                    .then(|| self.render_directory(false, window, cx).into_any_element());
                let player = div()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.render_video_area(fullscreen, window, cx))
                    .when(!fullscreen, |d| {
                        d.child(self.render_dock(false, window, cx))
                    })
                    .into_any_element();
                let row = div().flex_1().min_h(px(0.)).flex();
                if self.settings.directory_side == Side::Left {
                    row.children(directory).child(player)
                } else {
                    row.child(player).children(directory)
                }
            })
            .when(self.overlays.settings_open, |d| {
                d.child(self.render_settings_modal(window, cx))
            })
            .when(self.overlays.palette_open, |d| {
                d.child(self.render_command_palette(window, cx))
            })
            .when_some(self.overlays.context_menu.clone(), |d, menu| {
                d.child(self.render_context_menu(&menu, window, cx))
            })
    }
}
