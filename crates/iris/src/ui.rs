//! The main window: title bar, docked directory, video area, control bar,
//! and overlays drawn by GPUI on top of mpv's video.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    App, Bounds, Context, CursorStyle, ExternalPaths, FocusHandle, InteractiveElement, IntoElement,
    KeyBinding, MouseButton, MouseMoveEvent, ParentElement, PathPromptOptions, Pixels, Point,
    Render, SharedString, Styled, Subscription, Task, Window, actions, div, prelude::FluentBuilder,
    px,
};

use crate::geometry::ImageView;
use crate::input::{self, InputContext, PointerCommand, PointerInput};
use crate::library::{self, Entry, Kind};
use crate::platform;
use crate::player::{self, Command, Session, SessionEvent};
use crate::settings::{Settings, Side};
use crate::store::Store;
use crate::theme::*;
use crate::video::VideoSurface;

mod controls;
mod directory;
mod titlebar;
mod viewport;
mod widgets;

use viewport::Viewport;

/// Which entries the directory lists. Favorites span folders.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DirectoryFilter {
    All,
    Videos,
    Photos,
    Favorites,
}

/// While a video plays, its position is written to the records this often.
const SAVE_EVERY: Duration = Duration::from_secs(5);

/// In fullscreen, the control bar, info chip and cursor hide after this long
/// without mouse movement.
const HIDE_AFTER: Duration = Duration::from_millis(2000);

actions!(
    iris,
    [
        TogglePause,
        SeekBackwardExact,
        SeekForwardExact,
        VolumeUp,
        VolumeDown,
        SpeedDown,
        SpeedUp,
        ToggleDirectory,
        ToggleFullscreen,
        ExitFullscreen,
        ToggleSubs,
        CycleSub,
        CycleAudio,
        ToggleImageSize,
        FitImage,
        ZoomIn,
        ZoomOut,
        OpenNextMedia,
        OpenPreviousMedia,
        OpenFile,
        ToggleFavorite,
        Quit
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("space", TogglePause, Some("Viewport && media == video")),
        KeyBinding::new(
            "left",
            SeekBackwardExact,
            Some("Viewport && media == video"),
        ),
        KeyBinding::new(
            "right",
            SeekForwardExact,
            Some("Viewport && media == video"),
        ),
        KeyBinding::new("up", VolumeUp, Some("Viewport && media == video")),
        KeyBinding::new("down", VolumeDown, Some("Viewport && media == video")),
        KeyBinding::new("[", SpeedDown, Some("Viewport && media == video")),
        KeyBinding::new("]", SpeedUp, Some("Viewport && media == video")),
        KeyBinding::new("v", ToggleSubs, Some("Viewport && media == video")),
        KeyBinding::new("j", CycleSub, Some("Viewport && media == video")),
        KeyBinding::new("a", CycleAudio, Some("Viewport && media == video")),
        KeyBinding::new("space", ToggleImageSize, Some("Viewport && media == image")),
        KeyBinding::new("0", FitImage, Some("Viewport && media == image")),
        KeyBinding::new("=", ZoomIn, Some("Viewport && media == image")),
        KeyBinding::new("-", ZoomOut, Some("Viewport && media == image")),
        KeyBinding::new(
            "left",
            OpenPreviousMedia,
            Some("Viewport && media == image"),
        ),
        KeyBinding::new("right", OpenNextMedia, Some("Viewport && media == image")),
        KeyBinding::new("tab", ToggleDirectory, Some("Workspace")),
        KeyBinding::new("f", ToggleFullscreen, Some("Workspace")),
        KeyBinding::new("escape", ExitFullscreen, Some("Workspace")),
        KeyBinding::new("pagedown", OpenNextMedia, Some("Viewport")),
        KeyBinding::new("pageup", OpenPreviousMedia, Some("Viewport")),
        KeyBinding::new("s", ToggleFavorite, Some("Workspace")),
        KeyBinding::new("secondary-o", OpenFile, Some("Workspace")),
        KeyBinding::new("secondary-q", Quit, Some("Workspace")),
    ]);
}

pub struct IrisWindow {
    focus: FocusHandle,
    // A window owns viewports; each viewport owns one independent session/surface.
    viewports: Vec<Viewport>,
    show_directory: bool,
    tick: i64,
    toast: Option<SharedString>,
    toast_seq: u64,
    progress_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    settings: Settings,
    /// Path-keyed favorites and playback positions (application data directory).
    store: Store,
    filter: DirectoryFilter,
    /// Bumped per open request; a slower earlier scan never replaces a later one.
    load_generation: u64,
    /// The path being prepared in the background, for stepping before it loads.
    pending_path: Option<PathBuf>,
    last_save: Instant,
    shutdown_done: bool,
    /// Mirrors `window.is_fullscreen()`, updated at the start of each render.
    fullscreen: bool,
    /// Fullscreen only: the directory floating over the video.
    dir_popup: bool,
    /// Fullscreen only: whether the control bar, info chip and cursor are shown.
    chrome_visible: bool,
    last_activity: Instant,
    /// A hide timer is running.
    hide_armed: bool,
    mouse_pos: Option<Point<Pixels>>,
    /// The fullscreen control bar's frame, for keeping the chrome up while
    /// the pointer rests on it.
    dock_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The pointer is over the timeline.
    timeline_hover: bool,
    /// A seek drag started on the timeline and the button is still down.
    seeking: bool,
    _quit: Subscription,
    _events: Task<()>,
}

impl IrisWindow {
    pub fn new(
        session: Session,
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
        let focus = cx.focus_handle();
        focus.focus(window);

        let mut rx = session.events();
        let events = cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                if this.update(cx, |this, cx| this.on_event(ev, cx)).is_err() {
                    break;
                }
            }
        });
        let quit = cx.on_app_quit(|this, _| {
            this.shutdown();
            async {}
        });
        if let Some(mut control) = crate::debug::control_channel() {
            cx.spawn_in(window, async move |this, cx| {
                while let Some(line) = control.next().await {
                    // Keystrokes go through the window's normal dispatch, which
                    // must not run while this view is being updated.
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
                    // `move x y` / `click x y`: synthetic mouse input in window
                    // coordinates, through the same hit-testing as real input.
                    if let Some((c, x, y)) = parse_mouse_command(&line) {
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

        let mut this = Self {
            focus,
            viewports: vec![Viewport {
                session,
                video: Rc::new(RefCell::new(video)),
                rect: Rc::new(Cell::new(None)),
                image: ImageView::default(),
                drag_origin: None,
            }],
            show_directory: true,
            tick: -1,
            toast: None,
            toast_seq: 0,
            progress_bounds: Rc::new(Cell::new(None)),
            settings: Settings::load(),
            store: Store::load(),
            filter: DirectoryFilter::All,
            load_generation: 0,
            pending_path: None,
            last_save: Instant::now(),
            shutdown_done: false,
            fullscreen: false,
            dir_popup: false,
            chrome_visible: true,
            last_activity: Instant::now(),
            hide_armed: false,
            mouse_pos: None,
            dock_bounds: Rc::new(Cell::new(None)),
            timeline_hover: false,
            seeking: false,
            _quit: quit,
            _events: events,
        };
        if let Some(v) = this.settings.volume {
            this.command(Command::SetVolume(v), cx);
        }
        if let Some(v) = this.settings.speed {
            this.command(Command::SetSpeed(v), cx);
        }
        if let Some(path) = initial {
            this.open_path(&path, cx);
        }
        this
    }

    fn debug_command(&mut self, line: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut parts = line.splitn(2, ' ');
        let (cmd, arg) = (
            parts.next().unwrap_or(""),
            parts.next().unwrap_or("").trim(),
        );
        match cmd {
            "snap" => match platform::snapshot_window(window, Path::new(arg)) {
                Ok(()) => eprintln!("[iris/control] snap {arg} ok"),
                Err(e) => eprintln!("[iris/control] snap failed: {e:#}"),
            },
            "resize" => {
                let mut it = arg.split_whitespace().filter_map(|s| s.parse::<f32>().ok());
                if let (Some(w), Some(h)) = (it.next(), it.next()) {
                    window.resize(gpui::size(px(w), px(h)));
                    eprintln!("[iris/control] resize {w}x{h}");
                }
            }
            "open" => self.open_path(Path::new(arg), cx),
            "filter" => {
                self.filter = match arg {
                    "video" => DirectoryFilter::Videos,
                    "photo" => DirectoryFilter::Photos,
                    "favorites" => DirectoryFilter::Favorites,
                    _ => DirectoryFilter::All,
                };
                eprintln!("[iris/control] filter {:?}", self.filter);
                cx.notify();
            }
            "state" => eprintln!(
                "[iris/control] state pos={:.2} dur={:.2} paused={} vol={} speed={:.2} video={:?} hwdec={} dir={} side={:?} popup={} chrome={} fullscreen={} video_rect={:?} dock={:?} filter={:?} favorite={}",
                self.viewports[0].session.state.time_pos,
                self.viewports[0].session.state.duration,
                self.viewports[0].session.state.paused,
                self.viewports[0].session.state.volume,
                self.viewports[0].session.state.speed,
                self.viewports[0].session.state.video_size,
                self.viewports[0].session.state.hwdec,
                self.show_directory,
                self.settings.directory_side,
                self.dir_popup,
                self.chrome_visible,
                window.is_fullscreen(),
                self.viewports[0].rect.get().map(|r| r.0),
                self.dock_bounds.get().map(|b| (
                    b.origin.x,
                    b.origin.y,
                    b.size.width,
                    b.size.height
                )),
                self.filter,
                self.viewports[0]
                    .session
                    .current
                    .as_deref()
                    .is_some_and(|p| self.store.favorite(p)),
            ),
            #[cfg(any(debug_assertions, feature = "devtools"))]
            "get" => match self.viewports[0].session.debug_property(arg) {
                Ok(v) => eprintln!("[iris/control] get {arg} = {v}"),
                Err(e) => eprintln!("[iris/control] get {arg} failed: {e}"),
            },
            #[cfg(any(debug_assertions, feature = "devtools"))]
            "mpv" => {
                let args: Vec<&str> = arg.split_whitespace().collect();
                match self.viewports[0].session.debug_command(&args) {
                    Ok(()) => eprintln!("[iris/control] mpv {arg} ok"),
                    Err(e) => eprintln!("[iris/control] mpv {arg} failed: {e}"),
                }
            }
            "quit" => cx.quit(),
            _ => eprintln!("[iris/control] unknown command: {line}"),
        }
    }

    fn shutdown(&mut self) {
        if self.shutdown_done {
            return;
        }
        self.shutdown_done = true;
        self.save_progress();
        let state = &self.viewports[0].session.state;
        // mpv may boost past 100 (volume-max); the saved setting stays in 0–100.
        self.settings.volume = Some(state.volume.clamp(0., 100.));
        self.settings.speed = Some(state.speed.clamp(0.1, 8.));
        self.settings.save();
        // Free the render context before mpv goes away, then stop mpv.
        drop(self.viewports[0].video.borrow_mut().take());
        self.viewports[0].session.shutdown();
    }

    /// Records the loaded video's position; finished files resume from the start.
    fn save_progress(&mut self) {
        if let Some((path, position, duration, ended)) = self.viewports[0].session.progress() {
            self.store.remember(path, position, duration, ended);
            self.store.save();
        }
        self.last_save = Instant::now();
    }

    /// Scans the folder and reads photo headers on a background thread (network
    /// drives can be slow), then loads on the UI thread unless a newer request won.
    pub(crate) fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.save_progress();
        self.load_generation += 1;
        let generation = self.load_generation;
        // No filesystem access here: `absolute` is lexical. A symlinked path
        // just misses the cache and gets a fresh scan.
        self.pending_path = std::path::absolute(path).ok();
        let session = &self.viewports[0].session;
        let cached = session
            .folder
            .clone()
            .filter(|folder| {
                self.pending_path.as_deref().and_then(Path::parent) == Some(folder.as_path())
            })
            .map(|folder| (folder, session.entries.clone()));
        let path = path.to_path_buf();
        let task = cx
            .background_executor()
            .spawn(async move { player::prepare(&path, cached) });
        cx.spawn(async move |this, cx| {
            let prepared = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.pending_path = None;
                let result = prepared.and_then(|prepared| {
                    let resume = prepared.target().map_or(0., |p| this.store.resume(p));
                    this.viewports[0].session.open(prepared, resume)
                });
                if let Err(e) = result {
                    this.show_toast(format!("无法打开 · {e}"), cx);
                }
                this.viewports[0].image.reset();
                cx.notify();
            });
        })
        .detach();
    }

    /// The directory listing under the current filter, in display order.
    fn display_entries(&self) -> Vec<Entry> {
        let session = &self.viewports[0].session;
        match self.filter {
            DirectoryFilter::Favorites => {
                let mut entries: Vec<Entry> = self
                    .store
                    .records
                    .iter()
                    .filter(|(_, r)| r.favorite)
                    .filter_map(|(path, _)| {
                        Some(Entry {
                            path: path.clone(),
                            name: path.file_name()?.to_string_lossy().into_owned(),
                            kind: library::kind_of(path).unwrap_or(Kind::Video),
                        })
                    })
                    .collect();
                entries.sort_by(|a, b| {
                    library::natural_cmp(&a.name, &b.name).then_with(|| a.path.cmp(&b.path))
                });
                entries
            }
            filter => session
                .entries
                .iter()
                .filter(|e| match filter {
                    DirectoryFilter::Videos => e.kind == Kind::Video,
                    DirectoryFilter::Photos => e.kind == Kind::Photo,
                    _ => true,
                })
                .cloned()
                .collect(),
        }
    }

    /// Moves through the filtered listing, from the file being opened if any.
    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let entries = self.display_entries();
        if entries.is_empty() {
            return;
        }
        let current = self
            .pending_path
            .as_ref()
            .or(self.viewports[0].session.current.as_ref())
            .and_then(|p| entries.iter().position(|e| &e.path == p));
        let next = current
            .map(|i| (i as isize + delta).rem_euclid(entries.len() as isize) as usize)
            .unwrap_or(if delta < 0 { entries.len() - 1 } else { 0 });
        self.open_path(&entries[next].path, cx);
    }

    fn toggle_favorite(&mut self, _: &ToggleFavorite, _: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.viewports[0].session.current.clone() else {
            return;
        };
        let added = self.store.toggle_favorite(&path);
        self.show_toast(
            if added {
                "已收藏 · S 取消"
            } else {
                "已取消收藏"
            },
            cx,
        );
    }

    fn command(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(e) = self.viewports[0].session.execute(command) {
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

    // ---- fullscreen chrome ----

    /// Picks up fullscreen changes; called at the start of every render, so
    /// the rest of this frame sees the new state.
    fn sync_fullscreen(&mut self, window: &Window, cx: &mut Context<Self>) {
        let fs = window.is_fullscreen();
        if fs == self.fullscreen {
            return;
        }
        self.fullscreen = fs;
        self.dir_popup = false;
        self.chrome_visible = true;
        if fs {
            self.bump_activity(cx);
        }
    }

    /// Mouse movement (or a key that changes what the bar shows): show the
    /// fullscreen chrome and restart the hide countdown.
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

    /// Hides the chrome once the mouse has been idle for `HIDE_AFTER`, unless
    /// it rests on the control bar, a seek drag is running or the directory
    /// popup is open. Returns how long
    /// to wait before checking again, or `None` when the timer should stop
    /// (the next `bump_activity` starts a new one).
    fn hide_check(&mut self, cx: &mut Context<Self>) -> Option<Duration> {
        let over_dock = match (self.mouse_pos, self.dock_bounds.get()) {
            (Some(p), Some(b)) => b.contains(&p),
            _ => false,
        };
        if self.fullscreen && self.chrome_visible && !self.dir_popup && !self.seeking && !over_dock
        {
            let idle = self.last_activity.elapsed();
            if idle < HIDE_AFTER {
                return Some(HIDE_AFTER - idle);
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

    fn close_popup(&mut self, cx: &mut Context<Self>) {
        self.dir_popup = false;
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

    fn on_event(&mut self, event: SessionEvent, cx: &mut Context<Self>) {
        self.viewports[0].session.apply(&event);
        if self.last_save.elapsed() >= SAVE_EVERY {
            self.save_progress();
        }
        match event {
            SessionEvent::Position(t) => {
                let tick = (t * 4.).floor() as i64;
                if tick != self.tick {
                    self.tick = tick;
                    if self.chrome_shown() {
                        cx.notify();
                    }
                }
            }
            SessionEvent::Error(code) => self.show_toast(format!("无法播放 · 错误 {code}"), cx),
            _ => cx.notify(),
        }
    }

    // ---- actions ----

    fn toggle_pause(&mut self, _: &TogglePause, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::TogglePlayback, cx);
    }
    fn seek_back(&mut self, _: &SeekBackwardExact, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::SeekRelative(-5.), cx);
        self.bump_activity(cx);
    }
    fn seek_forward(&mut self, _: &SeekForwardExact, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::SeekRelative(5.), cx);
        self.bump_activity(cx);
    }
    fn volume_up(&mut self, _: &VolumeUp, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::VolumeBy(2.), cx);
        self.bump_activity(cx);
    }
    fn volume_down(&mut self, _: &VolumeDown, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::VolumeBy(-2.), cx);
        self.bump_activity(cx);
    }
    fn speed_down(&mut self, _: &SpeedDown, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::SpeedBy(1. / 1.1), cx);
    }
    fn speed_up(&mut self, _: &SpeedUp, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::SpeedBy(1.1), cx);
    }
    /// Windowed: show or collapse the docked directory. Fullscreen: open or
    /// close the directory popup over the video.
    fn toggle_directory(&mut self, _: &ToggleDirectory, _: &mut Window, cx: &mut Context<Self>) {
        if self.fullscreen {
            self.dir_popup = !self.dir_popup;
            self.bump_activity(cx);
            cx.notify();
            return;
        }
        self.show_directory = !self.show_directory;
        if self.show_directory {
            cx.notify();
        } else {
            self.show_toast("目录已收起 · Tab 展开", cx);
        }
    }
    fn toggle_fullscreen(
        &mut self,
        _: &ToggleFullscreen,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.toggle_fullscreen();
    }
    /// Esc closes the directory popup first, then leaves fullscreen.
    fn exit_fullscreen(&mut self, _: &ExitFullscreen, window: &mut Window, cx: &mut Context<Self>) {
        if self.dir_popup {
            self.close_popup(cx);
        } else if window.is_fullscreen() {
            window.toggle_fullscreen();
        }
    }
    fn toggle_subs(&mut self, _: &ToggleSubs, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::ToggleSubtitles, cx);
    }
    fn cycle_sub(&mut self, _: &CycleSub, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::NextSubtitle, cx);
    }

    fn cycle_audio(&mut self, _: &CycleAudio, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::NextAudio, cx);
    }

    fn image_geometry(&self) -> ([f64; 2], [f64; 2]) {
        let rect = self.viewports[0].rect.get().map(|r| r.0).unwrap_or([0.; 4]);
        let state = &self.viewports[0].session.state;
        let (mut w, mut h) = state.video_size;
        if state.rotation.rem_euclid(180) == 90 {
            std::mem::swap(&mut w, &mut h);
        }
        ([rect[2], rect[3]], [w.max(1) as f64, h.max(1) as f64])
    }

    fn apply_image_view(&mut self, cx: &mut Context<Self>) {
        let view = &self.viewports[0];
        if let Err(e) = view.session.transform(view.image.zoom, view.image.pan) {
            self.show_toast(format!("缩放失败 · {e}"), cx);
        }
        cx.notify();
    }

    fn toggle_image_size(
        &mut self,
        _: &ToggleImageSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (viewport, media) = self.image_geometry();
        self.viewports[0]
            .image
            .toggle_actual_size(viewport, media, window.scale_factor() as f64);
        self.apply_image_view(cx);
    }

    fn fit_image(&mut self, _: &FitImage, _: &mut Window, cx: &mut Context<Self>) {
        self.viewports[0].image.reset();
        self.apply_image_view(cx);
    }

    fn zoom_at(&mut self, delta: f64, point: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let (viewport, media) = self.image_geometry();
        let rect = self.viewports[0].rect.get().map(|r| r.0).unwrap_or([0.; 4]);
        let point = point
            .map(|p| {
                [
                    f32::from(p.x) as f64 - rect[0],
                    f32::from(p.y) as f64 - rect[1],
                ]
            })
            .unwrap_or([viewport[0] / 2., viewport[1] / 2.]);
        self.viewports[0]
            .image
            .zoom_at(delta, point, viewport, media);
        self.apply_image_view(cx);
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_at(0.25, None, cx);
    }
    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_at(-0.25, None, cx);
    }

    fn input_context(&self) -> InputContext {
        InputContext {
            media: self.viewports[0].session.kind(),
            zoomed: self.viewports[0].image.zoomed(),
            overlay: self.dir_popup,
        }
    }
    fn next_item(&mut self, _: &OpenNextMedia, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
        self.bump_activity(cx);
    }
    fn prev_item(&mut self, _: &OpenPreviousMedia, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
        self.bump_activity(cx);
    }
    fn open_file(&mut self, _: &OpenFile, _: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some("打开".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(p) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| this.open_path(&p, cx));
            }
        })
        .detach();
    }

    /// Picture size, plus the decoder for videos: `1920×1080 · 硬解 videotoolbox`.
    fn media_info(&self) -> Option<String> {
        let session = &self.viewports[0].session;
        let kind = session.kind()?;
        let state = &session.state;
        let (w, h) = state.video_size;
        let mut parts = Vec::new();
        if w > 0 && h > 0 {
            parts.push(format!("{w}×{h}"));
        }
        if kind == Kind::Video {
            parts.push(if state.hwdec.is_empty() || state.hwdec == "no" {
                "软解".into()
            } else {
                format!("硬解 {}", state.hwdec)
            });
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    fn seek_to_x(&self, x: Pixels) {
        let Some(b) = self.progress_bounds.get() else {
            return;
        };
        if self.viewports[0].session.state.duration <= 0. || b.size.width <= px(0.) {
            return;
        }
        let ratio = ((x - b.origin.x) / b.size.width).clamp(0., 1.) as f64;
        if let Err(e) = self.viewports[0]
            .session
            .execute(Command::SeekPercent(ratio * 100.))
        {
            log::error!("seek: {e}");
        }
    }
}

fn parse_mouse_command(line: &str) -> Option<(&str, f32, f32)> {
    let (cmd, arg) = line.split_once(' ')?;
    if cmd != "move" && cmd != "click" {
        return None;
    }
    let mut it = arg.split_whitespace().filter_map(|s| s.parse::<f32>().ok());
    Some((cmd, it.next()?, it.next()?))
}

impl Render for IrisWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_fullscreen(window, cx);
        let fullscreen = self.fullscreen;
        if let Some(v) = self.viewports[0].video.borrow_mut().as_mut() {
            v.set_scale(window.scale_factor() as f64);
        }
        if !fullscreen {
            platform::hide_traffic_lights(window);
        }
        // Keeps subtitles above the floating bar while it is shown.
        let inset = if fullscreen && self.chrome_visible {
            (DOCK_H + FLOAT_GAP + 8.) as f64
        } else {
            0.
        };
        // Fullscreen changes layout this frame; the canvas probe still holds
        // the previous windowed rectangle until prepaint.
        let height = if fullscreen {
            f32::from(window.viewport_size().height) as f64
        } else {
            self.viewports[0].rect.get().map(|r| r.0[3]).unwrap_or(0.)
        };
        self.viewports[0].session.subtitle_inset(inset, height);

        // The root paints nothing: wherever no panel is drawn, mpv's layer
        // underneath shows through.
        div()
            .id("root")
            .key_context("Workspace")
            .capture_any_mouse_up(cx.listener(|this, _, window, _| this.focus.focus(window)))
            .on_any_mouse_down(cx.listener(|this, _, window, _| this.focus.focus(window)))
            .size_full()
            .flex()
            .flex_col()
            .font_family(FONT_UI)
            .text_color(text())
            .on_action(cx.listener(Self::toggle_pause))
            .on_action(cx.listener(Self::seek_back))
            .on_action(cx.listener(Self::seek_forward))
            .on_action(cx.listener(Self::volume_up))
            .on_action(cx.listener(Self::volume_down))
            .on_action(cx.listener(Self::speed_down))
            .on_action(cx.listener(Self::speed_up))
            .on_action(cx.listener(Self::toggle_directory))
            .on_action(cx.listener(Self::toggle_fullscreen))
            .on_action(cx.listener(Self::exit_fullscreen))
            .on_action(cx.listener(Self::toggle_subs))
            .on_action(cx.listener(Self::cycle_sub))
            .on_action(cx.listener(Self::cycle_audio))
            .on_action(cx.listener(Self::toggle_image_size))
            .on_action(cx.listener(Self::fit_image))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::next_item))
            .on_action(cx.listener(Self::prev_item))
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::toggle_favorite))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                if let Some(p) = paths.paths().first() {
                    this.open_path(p, cx);
                }
            }))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                this.mouse_pos = Some(ev.position);
                if this.seeking {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        this.seek_to_x(ev.position.x);
                    } else {
                        this.seeking = false;
                    }
                    cx.notify();
                } else if this.timeline_hover {
                    // The hover time label follows the pointer.
                    cx.notify();
                }
                if ev.pressed_button == Some(MouseButton::Left) {
                    if let Some(origin) = this.viewports[0].drag_origin {
                        this.viewports[0].drag_origin = Some(ev.position);
                        let delta = [
                            f32::from(ev.position.x - origin.x) as f64,
                            f32::from(ev.position.y - origin.y) as f64,
                        ];
                        let (viewport, media) = this.image_geometry();
                        this.viewports[0].image.pan_by(delta, viewport, media);
                        this.apply_image_view(cx);
                    }
                } else {
                    this.viewports[0].drag_origin = None;
                }
                if this.fullscreen {
                    this.bump_activity(cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.viewports[0].drag_origin = None;
                    if this.seeking {
                        this.seeking = false;
                        cx.notify();
                    }
                }),
            )
            // Hidden until the mouse moves (macOS) / no cursor (Windows).
            .when(!self.chrome_shown(), |d| d.cursor(CursorStyle::None))
            .when(!fullscreen, |d| d.child(self.render_titlebar(cx)))
            // Two modules side by side: the directory, full height, and the
            // player (picture over its control bar).
            .child({
                let directory = (!fullscreen && self.show_directory)
                    .then(|| self.render_directory(false, cx).into_any_element());
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
    }
}
