//! The main window: title bar, docked directory, video area, control bar,
//! OSD, media info, context menus, popovers, and settings modal.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use gpui::{
    App, Bounds, Context, CursorStyle, ExternalPaths, FocusHandle, InteractiveElement, IntoElement,
    KeyBinding, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, PathPromptOptions,
    Pixels, Point, Render, RenderImage, ScrollStrategy, ScrollWheelEvent, SharedString, Styled,
    Subscription, Task, UniformListScrollHandle, Window, actions, div, prelude::FluentBuilder, px,
};

use crate::geometry::ImageView;
use crate::input::{self, InputContext, PointerCommand, PointerInput};
use crate::library::{self, Entry, Kind, ScanResult, SortBy, SortOrder};
use crate::platform::{self, MediaKey, MediaKeyBridge, PinchMonitor, SleepPreventer};
use crate::player::{self, Command, Session, SessionEvent, TrackKind};
use crate::settings::{
    MAX_DIRECTORY_WIDTH, MIN_DIRECTORY_WIDTH, OnEof, PhotoDefault, Settings, Side, VideoWheel,
};
use crate::store::Store;
use crate::theme::*;
use crate::video::VideoSurface;

mod context_menu;
mod controls;
mod directory;
mod info;
mod popover;
mod settings;
mod titlebar;
mod viewport;
mod widgets;

use context_menu::ContextMenu;
use directory::DirRow;
use viewport::Viewport;

/// Which entries the directory lists. Favorites span folders.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DirectoryFilter {
    All,
    Videos,
    Photos,
    Favorites,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PopoverKind {
    Subtitles,
    Audio,
    Speed,
    More,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettingsTab {
    Appearance,
    Playback,
    Tracks,
    View,
    Shortcuts,
}

/// While a video plays, its position is written to the records this often.
const SAVE_EVERY: Duration = Duration::from_secs(5);
/// Minimum interval between fast seeks while dragging the timeline.
const SEEK_DRAG_INTERVAL: Duration = Duration::from_millis(40);
/// Cap in-memory decoded thumbnails to keep RAM usage tiny.
const MAX_THUMB_CACHE: usize = 80;

actions!(
    iris,
    [
        TogglePause,
        SeekBackwardExact,
        SeekForwardExact,
        SeekBackwardLarge,
        SeekForwardLarge,
        FrameBack,
        FrameForward,
        VolumeUp,
        VolumeDown,
        ToggleMute,
        SpeedDown,
        SpeedUp,
        ResetSpeed,
        ToggleLoop,
        ToggleDirectory,
        ToggleSearch,
        RefreshDirectory,
        ToggleFullscreen,
        ExitFullscreen,
        OpenSettings,
        ToggleAlwaysOnTop,
        ToggleInfo,
        TakeScreenshot,
        CycleAspect,
        ToggleSubs,
        CycleSub,
        SubDelayBack,
        SubDelayForward,
        SubDelayReset,
        CycleAudio,
        PrevChapter,
        NextChapter,
        ToggleImageSize,
        FitImage,
        ZoomIn,
        ZoomOut,
        RotateCw,
        RotateCcw,
        ToggleHFlip,
        ToggleVFlip,
        ToggleFilmstrip,
        OpenNextMedia,
        OpenPreviousMedia,
        OpenFile,
        ToggleFavorite,
        Quit
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        // Video & audio transport
        KeyBinding::new("space", TogglePause, Some("Viewport && media == video")),
        KeyBinding::new("space", TogglePause, Some("Viewport && media == audio")),
        KeyBinding::new(
            "left",
            SeekBackwardExact,
            Some("Viewport && media == video"),
        ),
        KeyBinding::new(
            "left",
            SeekBackwardExact,
            Some("Viewport && media == audio"),
        ),
        KeyBinding::new(
            "right",
            SeekForwardExact,
            Some("Viewport && media == video"),
        ),
        KeyBinding::new(
            "right",
            SeekForwardExact,
            Some("Viewport && media == audio"),
        ),
        KeyBinding::new(
            "shift-left",
            SeekBackwardLarge,
            Some("Viewport && media == video"),
        ),
        KeyBinding::new(
            "shift-left",
            SeekBackwardLarge,
            Some("Viewport && media == audio"),
        ),
        KeyBinding::new(
            "shift-right",
            SeekForwardLarge,
            Some("Viewport && media == video"),
        ),
        KeyBinding::new(
            "shift-right",
            SeekForwardLarge,
            Some("Viewport && media == audio"),
        ),
        KeyBinding::new(",", FrameBack, Some("Viewport && media == video")),
        KeyBinding::new(".", FrameForward, Some("Viewport && media == video")),
        KeyBinding::new("up", VolumeUp, Some("Viewport && media == video")),
        KeyBinding::new("up", VolumeUp, Some("Viewport && media == audio")),
        KeyBinding::new("down", VolumeDown, Some("Viewport && media == video")),
        KeyBinding::new("down", VolumeDown, Some("Viewport && media == audio")),
        KeyBinding::new("m", ToggleMute, Some("Viewport && media == video")),
        KeyBinding::new("m", ToggleMute, Some("Viewport && media == audio")),
        KeyBinding::new("[", SpeedDown, Some("Viewport && media == video")),
        KeyBinding::new("[", SpeedDown, Some("Viewport && media == audio")),
        KeyBinding::new("]", SpeedUp, Some("Viewport && media == video")),
        KeyBinding::new("]", SpeedUp, Some("Viewport && media == audio")),
        KeyBinding::new("backspace", ResetSpeed, Some("Viewport && media == video")),
        KeyBinding::new("backspace", ResetSpeed, Some("Viewport && media == audio")),
        KeyBinding::new("l", ToggleLoop, Some("Viewport && media == video")),
        KeyBinding::new("l", ToggleLoop, Some("Viewport && media == audio")),
        KeyBinding::new("v", ToggleSubs, Some("Viewport && media == video")),
        KeyBinding::new("j", CycleSub, Some("Viewport && media == video")),
        KeyBinding::new("z", SubDelayBack, Some("Viewport && media == video")),
        KeyBinding::new("x", SubDelayForward, Some("Viewport && media == video")),
        KeyBinding::new("shift-z", SubDelayReset, Some("Viewport && media == video")),
        KeyBinding::new("a", CycleAudio, Some("Viewport && media == video")),
        KeyBinding::new("c", CycleAspect, Some("Viewport && media == video")),
        KeyBinding::new("shift-[", PrevChapter, Some("Viewport && media == video")),
        KeyBinding::new("shift-]", NextChapter, Some("Viewport && media == video")),
        KeyBinding::new("r", RotateCw, Some("Viewport && media == video")),
        KeyBinding::new("shift-r", RotateCcw, Some("Viewport && media == video")),
        KeyBinding::new("h", ToggleHFlip, Some("Viewport && media == video")),
        KeyBinding::new("shift-h", ToggleVFlip, Some("Viewport && media == video")),
        // Photos
        KeyBinding::new("space", ToggleImageSize, Some("Viewport && media == image")),
        KeyBinding::new("0", FitImage, Some("Viewport && media == image")),
        KeyBinding::new("=", ZoomIn, Some("Viewport && media == image")),
        KeyBinding::new("-", ZoomOut, Some("Viewport && media == image")),
        KeyBinding::new("r", RotateCw, Some("Viewport && media == image")),
        KeyBinding::new("shift-r", RotateCcw, Some("Viewport && media == image")),
        KeyBinding::new("h", ToggleHFlip, Some("Viewport && media == image")),
        KeyBinding::new("shift-h", ToggleVFlip, Some("Viewport && media == image")),
        KeyBinding::new("g", ToggleFilmstrip, Some("Viewport && media == image")),
        KeyBinding::new(
            "left",
            OpenPreviousMedia,
            Some("Viewport && media == image"),
        ),
        KeyBinding::new("right", OpenNextMedia, Some("Viewport && media == image")),
        // Workspace / general
        KeyBinding::new("tab", ToggleDirectory, Some("Workspace")),
        KeyBinding::new("f", ToggleFullscreen, Some("Workspace")),
        KeyBinding::new("t", ToggleAlwaysOnTop, Some("Workspace")),
        KeyBinding::new("i", ToggleInfo, Some("Viewport")),
        KeyBinding::new("secondary-i", ToggleInfo, Some("Workspace")),
        KeyBinding::new("secondary-s", TakeScreenshot, Some("Workspace")),
        KeyBinding::new("secondary-f", ToggleSearch, Some("Workspace")),
        KeyBinding::new("escape", ExitFullscreen, Some("Workspace")),
        KeyBinding::new("pagedown", OpenNextMedia, Some("Viewport")),
        KeyBinding::new("pageup", OpenPreviousMedia, Some("Viewport")),
        KeyBinding::new("s", ToggleFavorite, Some("Viewport")),
        KeyBinding::new("secondary-o", OpenFile, Some("Workspace")),
        KeyBinding::new("secondary-r", RefreshDirectory, Some("Workspace")),
        KeyBinding::new("secondary-,", OpenSettings, Some("Workspace")),
        KeyBinding::new("secondary-q", Quit, Some("Workspace")),
    ]);
}

pub struct IrisWindow {
    focus: FocusHandle,
    search_focus: FocusHandle,
    // A window owns viewports; each viewport owns one independent session/surface.
    viewports: Vec<Viewport>,
    show_directory: bool,
    tick: i64,
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
    search_open: bool,
    search_query: String,
    dir_rows: Vec<DirRow>,
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
    /// Fullscreen only: the directory floating over the video.
    dir_popup: bool,
    /// Centered settings modal overlay.
    settings_open: bool,
    settings_tab: SettingsTab,
    /// Floating media details & stats overlay.
    info_open: bool,
    /// Upward-floating menu anchored above the control bar.
    popover: Option<PopoverKind>,
    /// Right-click context menu.
    context_menu: Option<ContextMenu>,
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
    /// The pointer is over the timeline.
    timeline_hover: bool,
    /// A seek drag started on the timeline and the button is still down.
    seeking: bool,
    seek_preview_ratio: Option<f32>,
    last_seek_drag: Instant,
    /// Dragging the volume strip continuously.
    dragging_volume: bool,
    /// Trackpad scroll accumulator for smooth damped steps.
    wheel_accum: f64,
    /// Sequence counter to cancel single-click pause on double-click.
    click_seq: u64,
    /// In-memory photo thumbnail cache for the bottom filmstrip.
    thumb_cache: HashMap<PathBuf, Arc<RenderImage>>,
    thumb_inflight: HashSet<PathBuf>,
    /// OS idle sleep prevention while video plays.
    sleep_preventer: SleepPreventer,
    /// Native trackpad pinch gesture recognizer (macOS).
    _pinch_monitor: Option<PinchMonitor>,
    /// Native OS media keys & Now Playing bridge (macOS).
    media_keys: Option<MediaKeyBridge>,
    last_now_playing: Instant,
    _quit: Subscription,
    _events: Task<()>,
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
        focus.focus(window);

        let mut rx = session.events();
        let events = cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                if this.update(cx, |this, cx| this.on_event(ev, cx)).is_err() {
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
                            if this.viewports[0].session.state.paused {
                                this.command(Command::TogglePlayback, cx);
                            }
                        }
                        MediaKey::Pause => {
                            if !this.viewports[0].session.state.paused {
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

        let show_directory = settings.directory_open;
        let mut this = Self {
            focus,
            search_focus,
            viewports: vec![Viewport {
                session,
                video: Rc::new(RefCell::new(video)),
                rect: Rc::new(Cell::new(None)),
                image: ImageView::default(),
                drag_origin: None,
            }],
            show_directory,
            tick: -1,
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
            search_open: false,
            search_query: String::new(),
            dir_rows: Vec::new(),
            load_generation: 0,
            pending_path: None,
            loading_visible: false,
            pending_photo_actual: false,
            last_save: Instant::now(),
            shutdown_done: false,
            fullscreen: false,
            dir_popup: false,
            settings_open: false,
            settings_tab: SettingsTab::Appearance,
            info_open: false,
            popover: None,
            context_menu: None,
            show_remaining: false,
            resizing_dir: None,
            dir_scroll: UniformListScrollHandle::new(),
            last_window_size: None,
            announce_audio: false,
            announce_sub: false,
            chrome_visible: true,
            last_activity: Instant::now(),
            hide_armed: false,
            mouse_pos: None,
            dock_bounds: Rc::new(Cell::new(None)),
            timeline_hover: false,
            seeking: false,
            seek_preview_ratio: None,
            last_seek_drag: Instant::now(),
            dragging_volume: false,
            wheel_accum: 0.,
            click_seq: 0,
            thumb_cache: HashMap::new(),
            thumb_inflight: HashSet::new(),
            sleep_preventer: SleepPreventer::new(),
            _pinch_monitor: pinch_monitor,
            media_keys,
            last_now_playing: Instant::now() - Duration::from_secs(10),
            _quit: quit,
            _events: events,
        };
        this.viewports[0].session.apply_settings(&this.settings);
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

    pub(super) fn popover_anchor(&self, kind: PopoverKind) -> Option<Bounds<Pixels>> {
        match kind {
            PopoverKind::Subtitles => self.anchor_sub.get(),
            PopoverKind::Audio => self.anchor_audio.get(),
            PopoverKind::Speed => self.anchor_speed.get(),
            PopoverKind::More => self.anchor_more.get(),
        }
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
                self.rebuild_directory_rows();
                eprintln!("[iris/control] filter {:?}", self.filter);
                cx.notify();
            }
            "state" => eprintln!(
                "[iris/control] state pos={:.2} dur={:.2} paused={} vol={} muted={} speed={:.2} loop={} aspect={:?} sub_delay={:.1} video={:?} hwdec={} dir={} dir_w={:.0} side={:?} popup={} settings={} info={} popover={:?} chrome={} fullscreen={} video_rect={:?} dock={:?} filter={:?} favorite={}",
                self.viewports[0].session.state.time_pos,
                self.viewports[0].session.state.duration,
                self.viewports[0].session.state.paused,
                self.viewports[0].session.state.volume,
                self.viewports[0].session.state.muted,
                self.viewports[0].session.state.speed,
                self.viewports[0].session.state.loop_file,
                self.viewports[0].session.state.aspect,
                self.viewports[0].session.state.sub_delay,
                self.viewports[0].session.state.video_size,
                self.viewports[0].session.state.hwdec,
                self.show_directory,
                self.settings.directory_width,
                self.settings.directory_side,
                self.dir_popup,
                self.settings_open,
                self.info_open,
                self.popover,
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
        self.sleep_preventer.set_active(false);
        self.save_progress();
        let state = &self.viewports[0].session.state;
        self.settings.volume = Some(state.volume.clamp(0., 100.));
        if self.settings.remember_speed {
            self.settings.speed = Some(state.speed.clamp(0.25, 4.));
        }
        self.settings.directory_open = self.show_directory;
        if self.settings.remember_window_size
            && let Some((w, h)) = self.last_window_size
        {
            self.settings.window_width = Some(w);
            self.settings.window_height = Some(h);
        }
        self.settings.save();
        drop(self.viewports[0].video.borrow_mut().take());
        self.viewports[0].session.shutdown();
    }

    fn save_progress(&mut self) {
        if !self.settings.resume_playback {
            self.last_save = Instant::now();
            return;
        }
        if let Some((path, position, duration, ended)) = self.viewports[0].session.progress() {
            self.store.remember(path, position, duration, ended);
            self.store.save();
        }
        self.last_save = Instant::now();
    }

    pub(crate) fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.save_progress();
        self.popover = None;
        self.context_menu = None;
        self.load_generation += 1;
        let generation = self.load_generation;
        self.pending_path = std::path::absolute(path).ok();
        self.loading_visible = false;
        let session = &self.viewports[0].session;
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
                    this.viewports[0].session.open(prepared, resume)
                });
                match result {
                    Ok(()) => {
                        if let Some(cur) = this.viewports[0].session.current.clone() {
                            this.store.push_recent(&cur);
                        } else if let Some(dir) = this.viewports[0].session.folder.clone() {
                            this.store.push_recent(&dir);
                        }
                        this.pending_photo_actual = this.viewports[0].session.kind()
                            == Some(Kind::Photo)
                            && this.settings.photo_default == PhotoDefault::Actual;
                        this.rebuild_directory_rows();
                        this.scroll_directory_to_current();
                        this.ensure_filmstrip_thumbnails(cx);
                        this.sync_now_playing(true);
                    }
                    Err(e) => {
                        this.show_toast(format!("无法打开 · {e}"), cx);
                    }
                }
                this.viewports[0].image.reset();
                cx.notify();
            });
        })
        .detach();
    }

    fn open_multi_queue(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.save_progress();
        self.popover = None;
        self.context_menu = None;
        let entries: Vec<Entry> = paths
            .into_iter()
            .filter_map(|p| p.canonicalize().ok())
            .filter_map(library::entry_from_path)
            .collect();
        if entries.is_empty() {
            return;
        }
        let count = entries.len();
        let resume = if self.settings.resume_playback {
            self.store.resume(&entries[0].path)
        } else {
            0.
        };
        match self.viewports[0].session.open_queue(entries, resume) {
            Ok(()) => {
                if let Some(cur) = self.viewports[0].session.current.clone() {
                    self.store.push_recent(&cur);
                }
                self.rebuild_directory_rows();
                self.scroll_directory_to_current();
                self.ensure_filmstrip_thumbnails(cx);
                self.show_toast(format!("已加入临时播放队列 · {count} 个文件"), cx);
            }
            Err(e) => self.show_toast(format!("无法打开 · {e}"), cx),
        }
        self.viewports[0].image.reset();
        cx.notify();
    }

    fn ensure_filmstrip_thumbnails(&mut self, cx: &mut Context<Self>) {
        if self.viewports[0].session.kind() != Some(Kind::Photo) || !self.settings.photo_filmstrip {
            return;
        }
        let photos: Vec<PathBuf> = self
            .display_entries()
            .into_iter()
            .filter(|e| e.kind == Kind::Photo)
            .map(|e| e.path)
            .collect();
        if photos.len() < 2 {
            return;
        }
        let cur_ix = self.viewports[0]
            .session
            .current
            .as_ref()
            .and_then(|p| photos.iter().position(|x| x == p))
            .unwrap_or(0);
        let start = cur_ix.saturating_sub(6);
        let end = (cur_ix + 7).min(photos.len());
        for path in &photos[start..end] {
            if self.thumb_cache.contains_key(path) || self.thumb_inflight.contains(path) {
                continue;
            }
            self.thumb_inflight.insert(path.clone());
            let p = path.clone();
            let task = cx
                .background_executor()
                .spawn(async move { platform::decode_thumbnail(&p, 160) });
            let p_key = path.clone();
            cx.spawn(async move |this, cx| {
                let decoded = task.await;
                let _ = this.update(cx, |this, cx| {
                    this.thumb_inflight.remove(&p_key);
                    if let Some(img) = decoded {
                        if this.thumb_cache.len() >= MAX_THUMB_CACHE {
                            this.thumb_cache.clear();
                        }
                        this.thumb_cache.insert(p_key, img);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    fn scroll_directory_to_current(&self) {
        let Some(current) = self.viewports[0].session.current.as_ref() else {
            return;
        };
        if let Some(ix) = self
            .dir_rows
            .iter()
            .position(|r| matches!(r, DirRow::Media(e) if &e.path == current))
        {
            self.dir_scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
    }

    /// The directory listing under the current filter & sort order.
    fn display_entries(&self) -> Vec<Entry> {
        let session = &self.viewports[0].session;
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
        library::sort_entries(&mut entries, self.settings.sort_by, self.settings.sort_order);
        entries
    }

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

    fn step_next_no_wrap(&mut self, cx: &mut Context<Self>) {
        let entries = self.display_entries();
        let current = self
            .viewports[0]
            .session
            .current
            .as_ref()
            .and_then(|p| entries.iter().position(|e| &e.path == p));
        if let Some(i) = current
            && i + 1 < entries.len()
        {
            self.open_path(&entries[i + 1].path, cx);
        } else {
            self.show_toast("已播完当前列表", cx);
        }
    }

    fn toggle_favorite(&mut self, _: &ToggleFavorite, _: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.viewports[0].session.current.clone() else {
            return;
        };
        let added = self.store.toggle_favorite(&path);
        self.rebuild_directory_rows();
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
            self.wheel_accum += raw_y * 0.14;
            let whole = self.wheel_accum.trunc();
            if whole.abs() >= 1. {
                self.wheel_accum -= whole;
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
            self.wheel_accum += raw_y;
            if self.wheel_accum.abs() >= threshold {
                let dir = if self.wheel_accum > 0. { 1 } else { -1 };
                self.wheel_accum = 0.;
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
            let session = &self.viewports[0].session;
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
        self.dir_popup = false;
        self.popover = None;
        self.context_menu = None;
        self.chrome_visible = true;
        if fs {
            self.bump_activity(cx);
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
            && !self.dir_popup
            && !self.settings_open
            && self.popover.is_none()
            && self.context_menu.is_none()
            && self.resizing_dir.is_none()
            && !self.seeking
            && !self.dragging_volume
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

    fn has_overlay(&self) -> bool {
        self.dir_popup
            || self.popover.is_some()
            || self.settings_open
            || self.context_menu.is_some()
    }

    fn close_overlays(&mut self, cx: &mut Context<Self>) {
        self.dir_popup = false;
        self.popover = None;
        self.settings_open = false;
        self.context_menu = None;
        self.bump_activity(cx);
        cx.notify();
    }

    fn toggle_popover(&mut self, kind: PopoverKind, cx: &mut Context<Self>) {
        self.context_menu = None;
        if self.popover == Some(kind) {
            self.popover = None;
        } else {
            self.popover = Some(kind);
            self.settings_open = false;
        }
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

    fn on_event(&mut self, event: SessionEvent, cx: &mut Context<Self>) {
        let was_ended = self.viewports[0].session.state.ended;
        self.viewports[0].session.apply(&event);
        self.sleep_preventer
            .set_active(self.viewports[0].session.is_actively_playing_video());
        if self.last_save.elapsed() >= SAVE_EVERY {
            self.save_progress();
        }
        match event {
            SessionEvent::Position(t) => {
                // Smooth 12Hz bar ticks while chrome is visible (still ~0.5% CPU when paused).
                let tick = (t * 12.).floor() as i64;
                if tick != self.tick {
                    self.tick = tick;
                    if self.chrome_shown()
                        || self.info_open
                        || self.viewports[0].session.kind() == Some(Kind::Audio)
                    {
                        cx.notify();
                    }
                }
                self.sync_now_playing(false);
            }
            SessionEvent::Paused(_) => {
                self.sync_now_playing(true);
                cx.notify();
            }
            SessionEvent::Ended(true) if !was_ended => {
                self.save_progress();
                self.sync_now_playing(true);
                if matches!(
                    self.viewports[0].session.kind(),
                    Some(Kind::Video | Kind::Audio)
                ) && self.settings.on_eof == OnEof::Next
                    && !self.viewports[0].session.state.loop_file
                {
                    self.step_next_no_wrap(cx);
                } else {
                    cx.notify();
                }
            }
            SessionEvent::Audio(_) if self.announce_audio => {
                self.announce_audio = false;
                let label = self.viewports[0]
                    .session
                    .selected_track(TrackKind::Audio)
                    .map(|t| t.label())
                    .unwrap_or_else(|| "无音轨".into());
                self.show_osd(format!("音轨 · {label}"), cx);
            }
            SessionEvent::SubtitleTrack(_) if self.announce_sub => {
                self.announce_sub = false;
                let label = self.viewports[0]
                    .session
                    .selected_track(TrackKind::Sub)
                    .map(|t| t.label())
                    .unwrap_or_else(|| "无字幕".into());
                self.show_osd(format!("字幕 · {label}"), cx);
            }
            SessionEvent::Error(code) => {
                self.show_toast(
                    format!("{}（错误 {code}）", player::friendly_error(code)),
                    cx,
                );
            }
            _ => cx.notify(),
        }
    }

    // ---- actions ----

    fn toggle_pause(&mut self, _: &TogglePause, _: &mut Window, cx: &mut Context<Self>) {
        let next_paused = !self.viewports[0].session.state.paused
            && !self.viewports[0].session.state.ended;
        self.command(Command::TogglePlayback, cx);
        self.show_osd(if next_paused { "暂停" } else { "播放" }, cx);
    }
    fn seek_back(&mut self, _: &SeekBackwardExact, _: &mut Window, cx: &mut Context<Self>) {
        let step = self.settings.seek_step;
        let state = &self.viewports[0].session.state;
        let target = (state.time_pos - step).max(0.);
        self.command(Command::SeekRelative(-step), cx);
        self.show_osd(
            format!("-{step:.0}s · {}", controls::fmt_time(target)),
            cx,
        );
    }
    fn seek_forward(&mut self, _: &SeekForwardExact, _: &mut Window, cx: &mut Context<Self>) {
        let step = self.settings.seek_step;
        let state = &self.viewports[0].session.state;
        let target = (state.time_pos + step).min(state.duration.max(0.));
        self.command(Command::SeekRelative(step), cx);
        self.show_osd(
            format!("+{step:.0}s · {}", controls::fmt_time(target)),
            cx,
        );
    }
    fn seek_back_large(&mut self, _: &SeekBackwardLarge, _: &mut Window, cx: &mut Context<Self>) {
        let step = (self.settings.seek_step * 6.).clamp(15., 120.);
        let target = (self.viewports[0].session.state.time_pos - step).max(0.);
        self.command(Command::SeekRelative(-step), cx);
        self.show_osd(
            format!("-{step:.0}s · {}", controls::fmt_time(target)),
            cx,
        );
    }
    fn seek_forward_large(
        &mut self,
        _: &SeekForwardLarge,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = (self.settings.seek_step * 6.).clamp(15., 120.);
        let state = &self.viewports[0].session.state;
        let target = (state.time_pos + step).min(state.duration.max(0.));
        self.command(Command::SeekRelative(step), cx);
        self.show_osd(
            format!("+{step:.0}s · {}", controls::fmt_time(target)),
            cx,
        );
    }
    fn frame_back(&mut self, _: &FrameBack, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::FrameBackStep, cx);
        self.show_osd("上一帧", cx);
    }
    fn frame_forward(&mut self, _: &FrameForward, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::FrameStep, cx);
        self.show_osd("下一帧", cx);
    }
    fn volume_up(&mut self, _: &VolumeUp, _: &mut Window, cx: &mut Context<Self>) {
        let next = (self.viewports[0].session.state.volume + 5.).clamp(0., 100.);
        self.command(Command::SetVolume(next), cx);
        self.show_osd(format!("音量 · {next:.0}%"), cx);
    }
    fn volume_down(&mut self, _: &VolumeDown, _: &mut Window, cx: &mut Context<Self>) {
        let next = (self.viewports[0].session.state.volume - 5.).clamp(0., 100.);
        self.command(Command::SetVolume(next), cx);
        self.show_osd(format!("音量 · {next:.0}%"), cx);
    }
    fn toggle_mute(&mut self, _: &ToggleMute, _: &mut Window, cx: &mut Context<Self>) {
        let next = !self.viewports[0].session.state.muted;
        self.command(Command::ToggleMute, cx);
        self.show_osd(if next { "已静音" } else { "已取消静音" }, cx);
    }
    fn speed_down(&mut self, _: &SpeedDown, _: &mut Window, cx: &mut Context<Self>) {
        let next = player::step_speed(self.viewports[0].session.state.speed, -1);
        self.command(Command::SetSpeed(next), cx);
        self.show_osd(format!("倍速 · {next:.2}×"), cx);
    }
    fn speed_up(&mut self, _: &SpeedUp, _: &mut Window, cx: &mut Context<Self>) {
        let next = player::step_speed(self.viewports[0].session.state.speed, 1);
        self.command(Command::SetSpeed(next), cx);
        self.show_osd(format!("倍速 · {next:.2}×"), cx);
    }
    fn reset_speed(&mut self, _: &ResetSpeed, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::SetSpeed(1.), cx);
        self.show_osd("倍速 · 1.00×", cx);
    }
    fn toggle_loop(&mut self, _: &ToggleLoop, _: &mut Window, cx: &mut Context<Self>) {
        let next = !self.viewports[0].session.state.loop_file;
        self.command(Command::SetLoop(next), cx);
        self.show_osd(
            if next {
                "单曲循环已开启"
            } else {
                "单曲循环已关闭"
            },
            cx,
        );
    }
    fn toggle_always_on_top(
        &mut self,
        _: &ToggleAlwaysOnTop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.always_on_top = !self.settings.always_on_top;
        platform::set_always_on_top(window, self.settings.always_on_top);
        self.settings.save();
        self.show_toast(
            if self.settings.always_on_top {
                "窗口已置顶 · T 取消"
            } else {
                "已取消窗口置顶"
            },
            cx,
        );
    }
    fn toggle_info(&mut self, _: &ToggleInfo, _: &mut Window, cx: &mut Context<Self>) {
        self.info_open = !self.info_open;
        cx.notify();
    }
    fn take_screenshot(&mut self, _: &TakeScreenshot, _: &mut Window, cx: &mut Context<Self>) {
        let Some(cur) = self.viewports[0].session.current.as_ref() else {
            return;
        };
        let stem = cur
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "iris".into());
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let dir = platform::default_screenshot_dir();
        let out = dir.join(format!("Iris-{stem}-{ts}.png"));
        let with_subs = self.settings.screenshot_subs;
        self.command(
            Command::Screenshot {
                path: out.clone(),
                with_subs,
            },
            cx,
        );
        self.show_toast(
            format!(
                "截图已保存 · {}",
                out.file_name().unwrap_or_default().to_string_lossy()
            ),
            cx,
        );
    }
    fn cycle_aspect(&mut self, _: &CycleAspect, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.viewports[0].session.state.aspect.next();
        self.command(Command::SetAspect(next), cx);
        self.show_osd(format!("画面比例 · {}", next.label()), cx);
    }
    fn toggle_directory(&mut self, _: &ToggleDirectory, _: &mut Window, cx: &mut Context<Self>) {
        self.popover = None;
        self.context_menu = None;
        if self.fullscreen {
            self.dir_popup = !self.dir_popup;
            self.bump_activity(cx);
            cx.notify();
            return;
        }
        self.show_directory = !self.show_directory;
        self.settings.directory_open = self.show_directory;
        self.settings.save();
        if self.show_directory {
            self.scroll_directory_to_current();
            cx.notify();
        } else {
            self.show_toast("目录已收起 · Tab 展开", cx);
        }
    }
    fn toggle_search(&mut self, _: &ToggleSearch, window: &mut Window, cx: &mut Context<Self>) {
        if self.fullscreen {
            self.dir_popup = true;
            self.bump_activity(cx);
        } else if !self.show_directory {
            self.show_directory = true;
        }
        self.search_open = !self.search_open;
        if self.search_open {
            self.search_focus.focus(window);
        } else {
            self.search_query.clear();
            self.rebuild_directory_rows();
            self.focus.focus(window);
        }
        cx.notify();
    }
    fn refresh_directory(&mut self, _: &RefreshDirectory, _: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self.viewports[0].session.folder.clone() else {
            return;
        };
        let task = cx
            .background_executor()
            .spawn(async move { library::scan_dir(&folder) });
        cx.spawn(async move |this, cx| {
            let scan = task.await;
            let _ = this.update(cx, |this, cx| {
                this.viewports[0].session.entries = scan.entries;
                this.viewports[0].session.subfolders = scan.subfolders;
                this.rebuild_directory_rows();
                this.scroll_directory_to_current();
                this.show_toast("目录已刷新", cx);
            });
        })
        .detach();
    }
    fn open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        if self.settings_open {
            self.popover = None;
            self.context_menu = None;
            self.dir_popup = false;
        }
        self.bump_activity(cx);
        cx.notify();
    }
    fn toggle_fullscreen(
        &mut self,
        _: &ToggleFullscreen,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.toggle_fullscreen();
    }
    fn exit_fullscreen(&mut self, _: &ExitFullscreen, window: &mut Window, cx: &mut Context<Self>) {
        if self.context_menu.is_some() {
            self.context_menu = None;
            cx.notify();
        } else if self.settings_open {
            self.settings_open = false;
            self.bump_activity(cx);
            cx.notify();
        } else if self.popover.is_some() {
            self.popover = None;
            self.bump_activity(cx);
            cx.notify();
        } else if self.info_open {
            self.info_open = false;
            cx.notify();
        } else if self.search_open {
            self.search_open = false;
            self.search_query.clear();
            self.rebuild_directory_rows();
            self.focus.focus(window);
            cx.notify();
        } else if self.dir_popup {
            self.dir_popup = false;
            self.bump_activity(cx);
            cx.notify();
        } else if window.is_fullscreen() {
            window.toggle_fullscreen();
        }
    }
    fn toggle_subs(&mut self, _: &ToggleSubs, _: &mut Window, cx: &mut Context<Self>) {
        let next = !self.viewports[0].session.state.subs_visible;
        self.command(Command::ToggleSubtitles, cx);
        self.show_osd(if next { "字幕已显示" } else { "字幕已隐藏" }, cx);
    }
    fn cycle_sub(&mut self, _: &CycleSub, _: &mut Window, cx: &mut Context<Self>) {
        if self.viewports[0].session.tracks_of(TrackKind::Sub).is_empty() {
            self.show_osd("当前视频没有字幕轨", cx);
            return;
        }
        self.announce_sub = true;
        self.command(Command::NextSubtitle, cx);
    }
    fn sub_delay_back(&mut self, _: &SubDelayBack, _: &mut Window, cx: &mut Context<Self>) {
        let next = ((self.viewports[0].session.state.sub_delay - 0.1) * 100.).round() / 100.;
        self.command(Command::SetSubDelay(next), cx);
        self.show_osd(format!("字幕偏移 · {next:+.1}s"), cx);
    }
    fn sub_delay_forward(&mut self, _: &SubDelayForward, _: &mut Window, cx: &mut Context<Self>) {
        let next = ((self.viewports[0].session.state.sub_delay + 0.1) * 100.).round() / 100.;
        self.command(Command::SetSubDelay(next), cx);
        self.show_osd(format!("字幕偏移 · {next:+.1}s"), cx);
    }
    fn sub_delay_reset(&mut self, _: &SubDelayReset, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::SetSubDelay(0.), cx);
        self.show_osd("字幕偏移 · 0.0s", cx);
    }
    fn cycle_audio(&mut self, _: &CycleAudio, _: &mut Window, cx: &mut Context<Self>) {
        if self.viewports[0]
            .session
            .tracks_of(TrackKind::Audio)
            .len()
            <= 1
        {
            self.show_osd("当前视频没有其他音轨", cx);
            return;
        }
        self.announce_audio = true;
        self.command(Command::NextAudio, cx);
    }
    fn prev_chapter(&mut self, _: &PrevChapter, _: &mut Window, cx: &mut Context<Self>) {
        if self.viewports[0].session.state.chapters.is_empty() {
            self.show_osd("当前视频没有章节", cx);
            return;
        }
        self.command(Command::PrevChapter, cx);
        self.show_osd("上一章节", cx);
    }
    fn next_chapter(&mut self, _: &NextChapter, _: &mut Window, cx: &mut Context<Self>) {
        if self.viewports[0].session.state.chapters.is_empty() {
            self.show_osd("当前视频没有章节", cx);
            return;
        }
        self.command(Command::NextChapter, cx);
        self.show_osd("下一章节", cx);
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

    fn toggle_image_size_at(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.viewports[0].image.zoomed() {
            self.viewports[0].image.reset();
            self.apply_image_view(cx);
        } else {
            let (viewport, media) = self.image_geometry();
            let scale = window.scale_factor() as f64;
            let current_scale = self.viewports[0]
                .image
                .pixel_scale(viewport, media, scale)
                .max(0.0001);
            let delta = (1. / current_scale).log2();
            self.zoom_at(delta, Some(point), cx);
        }
    }

    fn fit_image(&mut self, _: &FitImage, _: &mut Window, cx: &mut Context<Self>) {
        self.viewports[0].image.reset();
        self.apply_image_view(cx);
    }

    fn rotate_cw(&mut self, _: &RotateCw, _: &mut Window, cx: &mut Context<Self>) {
        self.viewports[0].image.reset();
        self.command(Command::RotateMedia(90), cx);
        self.apply_image_view(cx);
        self.show_osd("顺时针旋转 90°", cx);
    }

    fn rotate_ccw(&mut self, _: &RotateCcw, _: &mut Window, cx: &mut Context<Self>) {
        self.viewports[0].image.reset();
        self.command(Command::RotateMedia(-90), cx);
        self.apply_image_view(cx);
        self.show_osd("逆时针旋转 90°", cx);
    }

    fn toggle_hflip(&mut self, _: &ToggleHFlip, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::ToggleHFlip, cx);
        let on = self.viewports[0].session.state.hflip;
        self.show_osd(if on { "水平镜像" } else { "还原水平镜像" }, cx);
    }

    fn toggle_vflip(&mut self, _: &ToggleVFlip, _: &mut Window, cx: &mut Context<Self>) {
        self.command(Command::ToggleVFlip, cx);
        let on = self.viewports[0].session.state.vflip;
        self.show_osd(if on { "垂直镜像" } else { "还原垂直镜像" }, cx);
    }

    fn toggle_filmstrip(&mut self, _: &ToggleFilmstrip, _: &mut Window, cx: &mut Context<Self>) {
        self.settings.photo_filmstrip = !self.settings.photo_filmstrip;
        self.settings.save();
        if self.settings.photo_filmstrip {
            self.ensure_filmstrip_thumbnails(cx);
        }
        cx.notify();
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
            overlay: self.has_overlay(),
            video_wheel_volume: self.settings.video_wheel == VideoWheel::Volume,
            single_click_pause: self.settings.single_click_pause,
            photo_double_click_actual: self.settings.photo_double_click_actual,
        }
    }
    fn next_item(&mut self, _: &OpenNextMedia, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }
    fn prev_item(&mut self, _: &OpenPreviousMedia, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
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

    fn media_info(&self) -> Option<String> {
        let session = &self.viewports[0].session;
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
        self.seek_preview_ratio = Some(ratio);
    }

    fn seek_to_x(&mut self, x: Pixels, exact: bool) {
        let Some(b) = self.progress_bounds.get() else {
            return;
        };
        if self.viewports[0].session.state.duration <= 0. || b.size.width <= px(0.) {
            return;
        }
        let ratio = ((x - b.origin.x) / b.size.width).clamp(0., 1.) as f64;
        let cmd = if exact {
            Command::SeekPercent(ratio * 100.)
        } else {
            Command::SeekPercentFast(ratio * 100.)
        };
        if let Err(e) = self.viewports[0].session.execute(cmd) {
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
        if !fullscreen && !window.is_maximized() {
            let sz = window.viewport_size();
            let (w, h) = (f32::from(sz.width), f32::from(sz.height));
            if w >= 480. && h >= 300. {
                self.last_window_size = Some((w, h));
            }
        }
        if self.pending_photo_actual
            && self.viewports[0].session.kind() == Some(Kind::Photo)
            && self.viewports[0].session.state.video_size.0 > 0
            && self.viewports[0].rect.get().is_some()
        {
            self.pending_photo_actual = false;
            let (viewport, media) = self.image_geometry();
            self.viewports[0]
                .image
                .toggle_actual_size(viewport, media, window.scale_factor() as f64);
            let view = &self.viewports[0];
            let _ = view.session.transform(view.image.zoom, view.image.pan);
        }

        if let Some(v) = self.viewports[0].video.borrow_mut().as_mut() {
            v.set_scale(window.scale_factor() as f64);
        }
        if !fullscreen {
            platform::hide_traffic_lights(window);
        }
        let inset = if fullscreen && self.chrome_visible {
            (DOCK_H + FLOAT_GAP + 8.) as f64
        } else {
            0.
        };
        let height = if fullscreen {
            f32::from(window.viewport_size().height) as f64
        } else {
            self.viewports[0].rect.get().map(|r| r.0[3]).unwrap_or(0.)
        };
        self.viewports[0].session.subtitle_inset(inset, height);

        div()
            .id("root")
            .relative()
            .key_context("Workspace")
            .capture_any_mouse_up(cx.listener(|this, _, window, _| {
                // Keep keyboard focus in the search box while typing there; everywhere else returns to viewport.
                if !this.search_focus.is_focused(window) {
                    this.focus.focus(window);
                }
            }))
            .on_any_mouse_down(cx.listener(|this, _, window, _| {
                if !this.search_focus.is_focused(window) {
                    this.focus.focus(window);
                }
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
                let is_video = this.viewports[0].session.kind() == Some(Kind::Video);
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
                if this.dragging_volume {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        this.set_volume_from_x(ev.position.x, cx);
                    } else {
                        this.dragging_volume = false;
                        cx.notify();
                    }
                    return;
                }
                if this.seeking {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        this.update_seek_preview(ev.position.x);
                        if this.last_seek_drag.elapsed() >= SEEK_DRAG_INTERVAL {
                            this.last_seek_drag = Instant::now();
                            this.seek_to_x(ev.position.x, false);
                        }
                    } else {
                        this.seeking = false;
                        this.seek_preview_ratio = None;
                    }
                    cx.notify();
                } else if this.timeline_hover {
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
                } else if this.viewports[0].drag_origin.take().is_some() {
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
                    if this.dragging_volume {
                        this.dragging_volume = false;
                        cx.notify();
                    }
                    if this.viewports[0].drag_origin.take().is_some() {
                        cx.notify();
                    }
                    if this.seeking {
                        this.seeking = false;
                        this.seek_preview_ratio = None;
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
            .when(self.settings_open, |d| {
                d.child(self.render_settings_modal(window, cx))
            })
            .when_some(self.context_menu.clone(), |d, menu| {
                d.child(self.render_context_menu(&menu, window, cx))
            })
    }
}
