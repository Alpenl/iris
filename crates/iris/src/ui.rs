//! The spike's main window: title bar, docked directory, video area, dock,
//! and overlays drawn by GPUI on top of mpv's video.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    AnyElement, App, Bounds, Context, CursorStyle, Div, ExternalPaths, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement, PathPromptOptions, Pixels, Point, Render, Rgba, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, actions, canvas, div,
    prelude::FluentBuilder, px, relative,
};
use iris_mpv::{Event, Mpv, Value};

use crate::library::{self, Entry, Kind};
use crate::platform;
use crate::player::{self, prop};
use crate::settings::{Settings, Side};
use crate::theme::*;
use crate::video::VideoSurface;

/// In fullscreen, the dock, info chip and cursor hide after this long without
/// mouse movement.
const HIDE_AFTER: Duration = Duration::from_millis(2000);

actions!(
    iris,
    [
        TogglePause,
        SeekBack,
        SeekForward,
        VolumeUp,
        VolumeDown,
        SpeedDown,
        SpeedUp,
        ToggleDirectory,
        ToggleFullscreen,
        ExitFullscreen,
        ToggleSubs,
        CycleSub,
        NextItem,
        PrevItem,
        OpenFile,
        Quit
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("space", TogglePause, None),
        KeyBinding::new("left", SeekBack, None),
        KeyBinding::new("right", SeekForward, None),
        KeyBinding::new("up", VolumeUp, None),
        KeyBinding::new("down", VolumeDown, None),
        KeyBinding::new("[", SpeedDown, None),
        KeyBinding::new("]", SpeedUp, None),
        KeyBinding::new("tab", ToggleDirectory, None),
        KeyBinding::new("f", ToggleFullscreen, None),
        KeyBinding::new("escape", ExitFullscreen, None),
        KeyBinding::new("v", ToggleSubs, None),
        KeyBinding::new("j", CycleSub, None),
        KeyBinding::new("pagedown", NextItem, None),
        KeyBinding::new("pageup", PrevItem, None),
        KeyBinding::new("cmd-o", OpenFile, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct VideoRect([f64; 4]);

pub struct IrisWindow {
    focus: FocusHandle,
    mpv: Arc<Mpv>,
    video: Rc<RefCell<Option<VideoSurface>>>,
    folder: Option<PathBuf>,
    entries: Vec<Entry>,
    current: Option<usize>,
    show_directory: bool,
    time_pos: f64,
    duration: f64,
    paused: bool,
    volume: f64,
    speed: f64,
    video_size: (i64, i64),
    hwdec: String,
    subs_visible: bool,
    tick: i64,
    toast: Option<SharedString>,
    toast_seq: u64,
    video_rect: Rc<Cell<Option<VideoRect>>>,
    progress_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    settings: Settings,
    /// Mirrors `window.is_fullscreen()`, updated at the start of each render.
    fullscreen: bool,
    /// Fullscreen only: the directory floating over the video.
    dir_popup: bool,
    /// Fullscreen only: whether the dock, info chip and cursor are shown.
    chrome_visible: bool,
    last_activity: Instant,
    /// A hide timer is running.
    hide_armed: bool,
    mouse_pos: Option<Point<Pixels>>,
    dock_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    _quit: Subscription,
    _events: Task<()>,
}

impl IrisWindow {
    pub fn new(
        mpv: Arc<Mpv>,
        initial: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let video = match VideoSurface::attach(window, mpv.clone()) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("[iris] video surface failed: {e:#}");
                None
            }
        };
        platform::hide_traffic_lights(window);
        let focus = cx.focus_handle();
        focus.focus(window);

        let mut rx = player::spawn_event_pump(mpv.clone());
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
            mpv,
            video: Rc::new(RefCell::new(video)),
            folder: None,
            entries: Vec::new(),
            current: None,
            show_directory: true,
            time_pos: 0.,
            duration: 0.,
            paused: false,
            volume: 100.,
            speed: 1.,
            video_size: (0, 0),
            hwdec: String::new(),
            subs_visible: true,
            tick: -1,
            toast: None,
            toast_seq: 0,
            video_rect: Rc::new(Cell::new(None)),
            progress_bounds: Rc::new(Cell::new(None)),
            settings: Settings::load(),
            fullscreen: false,
            dir_popup: false,
            chrome_visible: true,
            last_activity: Instant::now(),
            hide_armed: false,
            mouse_pos: None,
            dock_bounds: Rc::new(Cell::new(None)),
            _quit: quit,
            _events: events,
        };
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
            "state" => eprintln!(
                "[iris/control] state pos={:.2} dur={:.2} paused={} vol={} speed={:.2} video={:?} hwdec={} dir={} side={:?} popup={} chrome={} fullscreen={} video_rect={:?} dock={:?}",
                self.time_pos,
                self.duration,
                self.paused,
                self.volume,
                self.speed,
                self.video_size,
                self.hwdec,
                self.show_directory,
                self.settings.directory_side,
                self.dir_popup,
                self.chrome_visible,
                window.is_fullscreen(),
                self.video_rect.get().map(|r| r.0),
                self.dock_bounds.get().map(|b| (
                    b.origin.x,
                    b.origin.y,
                    b.size.width,
                    b.size.height
                )),
            ),
            "get" => match self.mpv.get_property_string(arg) {
                Ok(v) => eprintln!("[iris/control] get {arg} = {v}"),
                Err(e) => eprintln!("[iris/control] get {arg} failed: {e}"),
            },
            "mpv" => {
                let args: Vec<&str> = arg.split_whitespace().collect();
                match self.mpv.command(&args) {
                    Ok(()) => eprintln!("[iris/control] mpv {arg} ok"),
                    Err(e) => eprintln!("[iris/control] mpv {arg} failed: {e}"),
                }
            }
            "quit" => cx.quit(),
            _ => eprintln!("[iris/control] unknown command: {line}"),
        }
    }

    fn shutdown(&mut self) {
        // Free the render context before mpv goes away, then stop mpv.
        drop(self.video.borrow_mut().take());
        let _ = self.mpv.command(&["quit"]);
    }

    fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        let (folder, target) = if path.is_dir() {
            (path.to_path_buf(), None)
        } else {
            (
                path.parent().map(Path::to_path_buf).unwrap_or_default(),
                Some(path.to_path_buf()),
            )
        };
        self.entries = library::scan(&folder);
        self.folder = Some(folder);
        let index = match &target {
            Some(t) => self.entries.iter().position(|e| &e.path == t),
            None => (!self.entries.is_empty()).then_some(0),
        };
        if let Some(i) = index {
            self.play(i, cx);
        } else if let Some(t) = target {
            // A file we don't list (unknown extension): still try to play it.
            let _ = self.mpv.command_async(&["loadfile", &t.to_string_lossy()]);
        }
        cx.notify();
    }

    fn play(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let _ = self
            .mpv
            .command_async(&["loadfile", &entry.path.to_string_lossy()]);
        self.current = Some(index);
        cx.notify();
    }

    fn current_kind(&self) -> Option<Kind> {
        self.current
            .and_then(|i| self.entries.get(i))
            .map(|e| e.kind)
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as isize;
        let next = self
            .current
            .map(|c| c as isize + delta)
            .unwrap_or(0)
            .rem_euclid(len);
        self.play(next as usize, cx);
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

    /// Mouse movement (or a key that changes what the dock shows): show the
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
    /// it rests on the dock or the directory popup is open. Returns how long
    /// to wait before checking again, or `None` when the timer should stop
    /// (the next `bump_activity` starts a new one).
    fn hide_check(&mut self, cx: &mut Context<Self>) -> Option<Duration> {
        let over_dock = match (self.mouse_pos, self.dock_bounds.get()) {
            (Some(p), Some(b)) => b.contains(&p),
            _ => false,
        };
        if self.fullscreen && self.chrome_visible && !self.dir_popup && !over_dock {
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

    fn on_event(&mut self, ev: Event, cx: &mut Context<Self>) {
        match ev {
            Event::PropertyChange { id, value, .. } => match (id, value) {
                (prop::TIME_POS, Value::Double(t)) => {
                    self.time_pos = t;
                    // Repaint the UI at most 4x per second for the clock; the
                    // video itself never needs a GPUI repaint.
                    // Nothing shows the clock while the fullscreen chrome is
                    // hidden; showing it again repaints anyway.
                    let tick = (t * 4.).floor() as i64;
                    if tick != self.tick {
                        self.tick = tick;
                        if self.chrome_shown() {
                            cx.notify();
                        }
                    }
                }
                (prop::DURATION, Value::Double(d)) => {
                    self.duration = d;
                    cx.notify();
                }
                (prop::PAUSE, Value::Flag(p)) => {
                    self.paused = p;
                    cx.notify();
                }
                (prop::VOLUME, Value::Double(v)) => {
                    self.volume = v;
                    cx.notify();
                }
                (prop::SPEED, Value::Double(s)) => {
                    self.speed = s;
                    cx.notify();
                }
                (prop::VIDEO_W, Value::Int(w)) => {
                    self.video_size.0 = w;
                    cx.notify();
                }
                (prop::VIDEO_H, Value::Int(h)) => {
                    self.video_size.1 = h;
                    cx.notify();
                }
                (prop::HWDEC, Value::Str(s)) => {
                    self.hwdec = s;
                    cx.notify();
                }
                (prop::HWDEC, Value::None) => {
                    self.hwdec.clear();
                    cx.notify();
                }
                (prop::SUB_VISIBLE, Value::Flag(v)) => {
                    self.subs_visible = v;
                    cx.notify();
                }
                (prop::TIME_POS, Value::None) => {
                    self.time_pos = 0.;
                    cx.notify();
                }
                _ => {}
            },
            Event::FileLoaded | Event::VideoReconfig => cx.notify(),
            Event::EndFile {
                reason: iris_mpv::EndReason::Error,
                error,
            } => {
                self.show_toast(format!("无法播放 · mpv 错误 {error}"), cx);
            }
            _ => {}
        }
    }

    // ---- actions ----

    fn toggle_pause(&mut self, _: &TogglePause, _: &mut Window, _: &mut Context<Self>) {
        let _ = self.mpv.command_async(&["cycle", "pause"]);
    }
    fn seek_back(&mut self, _: &SeekBack, _: &mut Window, cx: &mut Context<Self>) {
        if self.current_kind() == Some(Kind::Photo) {
            self.step(-1, cx);
        } else {
            let _ = self.mpv.command_async(&["seek", "-5", "relative"]);
            self.bump_activity(cx);
        }
    }
    fn seek_forward(&mut self, _: &SeekForward, _: &mut Window, cx: &mut Context<Self>) {
        if self.current_kind() == Some(Kind::Photo) {
            self.step(1, cx);
        } else {
            let _ = self.mpv.command_async(&["seek", "5", "relative"]);
            self.bump_activity(cx);
        }
    }
    fn volume_up(&mut self, _: &VolumeUp, _: &mut Window, cx: &mut Context<Self>) {
        let _ = self.mpv.command_async(&["add", "volume", "2"]);
        self.bump_activity(cx);
    }
    fn volume_down(&mut self, _: &VolumeDown, _: &mut Window, cx: &mut Context<Self>) {
        let _ = self.mpv.command_async(&["add", "volume", "-2"]);
        self.bump_activity(cx);
    }
    fn speed_down(&mut self, _: &SpeedDown, _: &mut Window, _: &mut Context<Self>) {
        let _ = self.mpv.command_async(&["multiply", "speed", "0.9091"]);
    }
    fn speed_up(&mut self, _: &SpeedUp, _: &mut Window, _: &mut Context<Self>) {
        let _ = self.mpv.command_async(&["multiply", "speed", "1.1"]);
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
            self.show_toast("目录已展开", cx);
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
    fn toggle_subs(&mut self, _: &ToggleSubs, _: &mut Window, _: &mut Context<Self>) {
        let _ = self.mpv.command_async(&["cycle", "sub-visibility"]);
    }
    fn cycle_sub(&mut self, _: &CycleSub, _: &mut Window, _: &mut Context<Self>) {
        let _ = self.mpv.command_async(&["cycle", "sub"]);
    }
    fn next_item(&mut self, _: &NextItem, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
        self.bump_activity(cx);
    }
    fn prev_item(&mut self, _: &PrevItem, _: &mut Window, cx: &mut Context<Self>) {
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
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(p) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| this.open_path(&p, cx));
                }
            }
        })
        .detach();
    }

    fn seek_to_x(&self, x: Pixels) {
        let Some(b) = self.progress_bounds.get() else {
            return;
        };
        if self.duration <= 0. || b.size.width <= px(0.) {
            return;
        }
        let ratio = ((x - b.origin.x) / b.size.width).clamp(0., 1.) as f64;
        let pct = format!("{:.3}", ratio * 100.);
        let _ = self
            .mpv
            .command_async(&["seek", &pct, "absolute-percent+keyframes"]);
    }

    // ---- view pieces ----

    fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let file = self
            .current
            .and_then(|i| self.entries.get(i))
            .map(|e| e.name.clone())
            .unwrap_or_default();
        let folder = self
            .folder
            .as_ref()
            .and_then(|f| f.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未打开".into());
        let win_button = |id: &'static str| {
            div()
                .id(id)
                .size(px(10.))
                .border_1()
                .border_color(text_dim())
                .hover(|s| s.bg(accent()).border_color(accent()))
                .cursor_pointer()
        };
        div()
            .h(px(TITLEBAR_H))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(14.))
            .bg(window_bg())
            .border_b_1()
            .border_color(line())
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(win_button("win-close").on_click(|_, window, _| window.remove_window()))
                    .child(win_button("win-min").on_click(|_, window, _| window.minimize_window()))
                    .child(
                        win_button("win-full").on_click(|_, window, _| window.toggle_fullscreen()),
                    ),
            )
            .child(
                div()
                    .id("drag")
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .pl(px(12.))
                    .on_mouse_down(MouseButton::Left, |ev: &MouseDownEvent, window, _| {
                        if ev.click_count >= 2 {
                            window.titlebar_double_click();
                        } else {
                            platform::begin_window_drag(window);
                        }
                    })
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(13.))
                            .child("IRIS"),
                    )
                    .child(div().w(px(1.)).h(px(14.)).bg(line()))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .font_family(FONT_MONO)
                            .text_size(px(11.))
                            .child(div().text_color(text_dim()).child(folder))
                            .child(div().text_color(text_dim()).child("/"))
                            .child(div().text_color(text()).child(file)),
                    ),
            )
            .child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(px(10.))
                    .text_color(text_dim())
                    .px(px(6.))
                    .py(px(2.))
                    .border_1()
                    .border_color(line())
                    .child("SPIKE · GPUI 0.2.2 + libmpv"),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
    }

    /// The directory panel. Docked (`floating == false`) it is a column beside
    /// the video; floating it fills the fullscreen popup.
    fn render_directory(&self, floating: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let side = self.settings.directory_side;
        let videos = self
            .entries
            .iter()
            .filter(|e| e.kind == Kind::Video)
            .count();
        let photos = self.entries.len() - videos;
        let tab = |label: String, active: bool| {
            div()
                .h_full()
                .flex()
                .items_center()
                .px(px(12.))
                .text_size(px(12.))
                .when(active, |d| {
                    d.text_color(text()).border_b_2().border_color(accent())
                })
                .when(!active, |d| d.text_color(text_dim()))
                .child(label)
        };
        let items = self.entries.iter().enumerate().map(|(i, e)| {
            let is_current = self.current == Some(i);
            div()
                .id(("entry", i))
                .h(px(48.))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .border_b_1()
                .border_color(line())
                .cursor_pointer()
                .hover(|s| s.bg(control()))
                .when(is_current, |d| {
                    d.bg(accent_soft()).border_l_2().border_color(accent())
                })
                .on_click(cx.listener(move |this, _, _, cx| this.play(i, cx)))
                .child(
                    div()
                        .w(px(22.))
                        .font_family(FONT_MONO)
                        .text_size(px(10.))
                        .text_color(if is_current { accent() } else { text_dim() })
                        .child(format!("{:02}", i + 1)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().text_size(px(12.)).truncate().child(e.name.clone()))
                        .child(
                            div()
                                .font_family(FONT_MONO)
                                .text_size(px(9.))
                                .text_color(if is_current { accent() } else { text_dim() })
                                .child(if is_current {
                                    "▶ 播放中".to_string()
                                } else if e.kind == Kind::Video {
                                    "VIDEO".to_string()
                                } else {
                                    "PHOTO".to_string()
                                }),
                        ),
                )
        });
        // Two small panel icons choose which edge the directory sits on.
        let side_button = |id: &'static str, s: Side| {
            let active = side == s;
            div()
                .id(id)
                .w(px(26.))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|st| st.bg(control()))
                .child(panel_icon(s, if active { accent() } else { text_dim() }))
                .on_click(cx.listener(move |this, _, _, cx| this.set_directory_side(s, cx)))
        };
        div()
            .w(px(DIRECTORY_W))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            // Opaque: a list over moving video is hard to read.
            .when(floating, |d| d.bg(panel()))
            .when(!floating, |d| {
                let d = d.bg(panel()).border_color(line());
                if side == Side::Left {
                    d.border_r_1()
                } else {
                    d.border_l_1()
                }
            })
            .child(
                div()
                    .h(px(36.))
                    .flex_none()
                    .flex()
                    .border_b_1()
                    .border_color(line())
                    .child(tab(format!("视频 {videos}"), true))
                    .child(tab(format!("照片 {photos}"), false))
                    .child(tab("收藏".into(), false))
                    .child(div().flex_1())
                    .child(side_button("side-left", Side::Left))
                    .child(side_button("side-right", Side::Right))
                    .child(
                        div()
                            .id("collapse")
                            .w(px(36.))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(text_dim())
                            .hover(|s| s.text_color(accent()))
                            .cursor_pointer()
                            // Points at the edge the panel folds into.
                            .child(if side == Side::Left { "⇤" } else { "⇥" })
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.toggle_directory(&ToggleDirectory, w, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .id("dir-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(items),
            )
    }

    fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let left = self.settings.directory_side == Side::Left;
        div()
            .w(px(RAIL_W))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .bg(panel())
            .border_color(line())
            .when(left, |d| d.border_r_1())
            .when(!left, |d| d.border_l_1())
            .child(
                div()
                    .id("expand")
                    .h(px(36.))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_b_1()
                    .border_color(line())
                    .text_color(text_dim())
                    .hover(|s| s.text_color(accent()))
                    .cursor_pointer()
                    // Points the way the panel unfolds.
                    .child(if left { "⇥" } else { "⇤" })
                    .on_click(
                        cx.listener(|this, _, w, cx| {
                            this.toggle_directory(&ToggleDirectory, w, cx)
                        }),
                    ),
            )
            .child(
                div()
                    .pt(px(10.))
                    .font_family(FONT_MONO)
                    .text_size(px(9.))
                    .text_color(text_dim())
                    .child(format!("{}", self.entries.len())),
            )
    }

    fn render_video_area(&self, fullscreen: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let last_rect = self.video_rect.clone();
        let video = self.video.clone();
        // Measures where the video area is and moves the native video view
        // there. Between layouts (live resize) AppKit's autoresizing keeps the
        // view's left/top/bottom insets fixed, which matches this layout.
        let probe = canvas(
            move |bounds, _, _| {
                let r = VideoRect([
                    f32::from(bounds.origin.x) as f64,
                    f32::from(bounds.origin.y) as f64,
                    f32::from(bounds.size.width) as f64,
                    f32::from(bounds.size.height) as f64,
                ]);
                if r.0[2] <= 0. || r.0[3] <= 0. || last_rect.get() == Some(r) {
                    return;
                }
                last_rect.set(Some(r));
                if let Some(v) = video.borrow().as_ref() {
                    let [x, y, w, h] = r.0;
                    v.set_frame(x, y, w, h);
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        let info = {
            let (w, h) = self.video_size;
            let mut parts = Vec::new();
            if w > 0 {
                parts.push(format!("{w}×{h}"));
            }
            parts.push(if self.hwdec.is_empty() || self.hwdec == "no" {
                "软解".into()
            } else {
                format!("硬解 {}", self.hwdec)
            });
            parts.join(" · ")
        };

        div()
            .id("video")
            .relative()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .child(probe)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    // A click on the picture closes the directory popup.
                    if this.dir_popup {
                        this.close_popup(cx);
                    } else if ev.click_count == 2 {
                        window.toggle_fullscreen();
                    }
                }),
            )
            // Overlay 1: info chip over the picture, on the edge away from the
            // directory so the popup never covers it.
            .when(self.chrome_shown(), |d| {
                let chip = div()
                    .absolute()
                    .top(px(12.))
                    .px(px(8.))
                    .py(px(4.))
                    .bg(overlay())
                    .border_1()
                    .border_color(line())
                    .font_family(FONT_MONO)
                    .text_size(px(10.))
                    .text_color(text_dim())
                    .child(info);
                d.child(
                    if fullscreen && self.settings.directory_side == Side::Left {
                        chip.right(px(12.))
                    } else {
                        chip.left(px(12.))
                    },
                )
            })
            // Overlay 2: toast (bottom-centre, above the floating dock).
            .when_some(self.toast.clone(), |d, msg| {
                let bottom = match (fullscreen, self.chrome_visible) {
                    (false, _) => 20.,
                    (true, true) => DOCK_H + 12.,
                    (true, false) => 40.,
                };
                d.child(
                    div()
                        .absolute()
                        .bottom(px(bottom))
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .px(px(14.))
                                .py(px(8.))
                                .bg(overlay())
                                .border_1()
                                .border_color(line())
                                .border_l_2()
                                .border_color(accent())
                                .text_size(px(12.))
                                .child(msg),
                        ),
                )
            })
            .when(self.current.is_none(), |d| {
                d.child(
                    div()
                        .absolute()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(text_dim())
                        .text_size(px(13.))
                        .child("拖入文件或文件夹，或按 ⌘O 打开"),
                )
            })
            // Overlay 3: in fullscreen the dock floats over the video and
            // hides with the rest of the chrome.
            .when(fullscreen && self.chrome_visible, |d| {
                let slot = self.dock_bounds.clone();
                d.child(
                    div()
                        .absolute()
                        .bottom(px(16.))
                        .left(px(16.))
                        .right(px(16.))
                        .h(px(DOCK_H - 16.))
                        .bg(overlay())
                        .border_1()
                        .border_color(line())
                        // Clicks here are not clicks on the picture.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            canvas(move |b, _, _| slot.set(Some(b)), |_, _, _, _| {})
                                .absolute()
                                .size_full(),
                        )
                        .child(self.render_dock_body(cx)),
                )
            })
            // Overlay 4: the directory popup, on the chosen edge, between the
            // top of the screen and the floating dock.
            .when(fullscreen && self.dir_popup, |d| {
                let popup = div()
                    .absolute()
                    .top(px(16.))
                    .bottom(px(DOCK_H + 8.))
                    .border_1()
                    .border_color(line())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(self.render_directory(true, cx));
                d.child(if self.settings.directory_side == Side::Left {
                    popup.left(px(16.))
                } else {
                    popup.right(px(16.))
                })
            })
    }

    fn render_dock_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pct = if self.duration > 0. {
            (self.time_pos / self.duration).clamp(0., 1.) as f32
        } else {
            0.
        };
        let bounds_slot = self.progress_bounds.clone();
        let square = |id: &'static str| {
            div()
                .id(id)
                .size(px(32.))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .border_color(line())
                .bg(control())
                .text_size(px(12.))
                .hover(|s| s.border_color(accent()))
                .cursor_pointer()
        };
        let dir_open = if self.fullscreen {
            self.dir_popup
        } else {
            self.show_directory
        };
        let vol_segments = (self.volume / 10.).round().clamp(0., 10.) as usize;
        let field = |label: &'static str, value: String| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(9.))
                        .text_color(text_dim())
                        .child(label),
                )
                .child(div().font_family(FONT_MONO).text_size(px(12.)).child(value))
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .px(px(16.))
            // Progress: 2px line with playhead; click or drag to seek.
            .child(
                div()
                    .h(px(34.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .font_family(FONT_MONO)
                    .text_size(px(10.))
                    .text_color(text_dim())
                    .child(fmt_time(self.time_pos))
                    .child(
                        div()
                            .id("progress")
                            .relative()
                            .flex_1()
                            .h(px(16.))
                            .cursor_pointer()
                            .child(
                                canvas(move |b, _, _| bounds_slot.set(Some(b)), |_, _, _, _| {})
                                    .absolute()
                                    .size_full(),
                            )
                            .child(div().absolute().top(px(7.)).w_full().h(px(2.)).bg(line()))
                            .child(
                                div()
                                    .absolute()
                                    .top(px(7.))
                                    .w(relative(pct))
                                    .h(px(2.))
                                    .bg(accent()),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .top(px(3.))
                                    .left(relative(pct))
                                    .w(px(2.))
                                    .h(px(10.))
                                    .bg(accent()),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, _, _| {
                                    this.seek_to_x(ev.position.x)
                                }),
                            )
                            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, _| {
                                if ev.pressed_button == Some(MouseButton::Left) {
                                    this.seek_to_x(ev.position.x);
                                }
                            })),
                    )
                    .child(fmt_time(self.duration)),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                square("prev")
                                    .child("⏮")
                                    .on_click(cx.listener(|this, _, _, cx| this.step(-1, cx))),
                            )
                            .child(
                                square("play")
                                    .child(if self.paused { "▶" } else { "❚❚" })
                                    .border_color(accent())
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.toggle_pause(&TogglePause, w, cx)
                                    })),
                            )
                            .child(
                                square("next")
                                    .child("⏭")
                                    .on_click(cx.listener(|this, _, _, cx| this.step(1, cx))),
                            )
                            .child(
                                div()
                                    .pl(px(12.))
                                    .font_family(FONT_MONO)
                                    .text_size(px(22.))
                                    .child(fmt_time(self.time_pos)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(24.))
                            .child(field(
                                "字幕",
                                if self.subs_visible {
                                    "开".into()
                                } else {
                                    "关".into()
                                },
                            ))
                            .child(field("速度", format!("{:.2}×", self.speed)))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.))
                                    .child(
                                        div()
                                            .font_family(FONT_MONO)
                                            .text_size(px(9.))
                                            .text_color(text_dim())
                                            .child(format!("音量 {:.0}", self.volume)),
                                    )
                                    .child(div().flex().gap(px(2.)).children((0..10).map(|i| {
                                        div()
                                            .id(("vol", i))
                                            .w(px(5.))
                                            .h(px(12.))
                                            .bg(if i < vol_segments { accent() } else { line() })
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, _| {
                                                let v = format!("{}", (i + 1) * 10);
                                                let _ =
                                                    this.mpv.command_async(&["set", "volume", &v]);
                                            }))
                                    }))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.))
                                    // Directory: docked panel when windowed, popup in fullscreen.
                                    .child(
                                        square("dir")
                                            .when(dir_open, |d| d.border_color(accent()))
                                            .child(panel_icon(
                                                self.settings.directory_side,
                                                if dir_open { accent() } else { text() },
                                            ))
                                            .on_click(cx.listener(|this, _, w, cx| {
                                                this.toggle_directory(&ToggleDirectory, w, cx)
                                            })),
                                    )
                                    .child(square("fs").child("⛶").on_click(cx.listener(
                                        |this, _, w, cx| {
                                            this.toggle_fullscreen(&ToggleFullscreen, w, cx)
                                        },
                                    ))),
                            ),
                    ),
            )
    }
}

/// A 16x12 outline of a window with a filled bar on `side`: the directory icon.
fn panel_icon(side: Side, color: Rgba) -> Div {
    div()
        .w(px(16.))
        .h(px(12.))
        .flex_none()
        .flex()
        .border_1()
        .border_color(color)
        .when(side == Side::Right, |d| d.justify_end())
        .child(div().w(px(5.)).h_full().bg(color))
}

fn parse_mouse_command(line: &str) -> Option<(&str, f32, f32)> {
    let (cmd, arg) = line.split_once(' ')?;
    if cmd != "move" && cmd != "click" {
        return None;
    }
    let mut it = arg.split_whitespace().filter_map(|s| s.parse::<f32>().ok());
    Some((cmd, it.next()?, it.next()?))
}

fn fmt_time(t: f64) -> String {
    let s = t.max(0.) as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

impl Render for IrisWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_fullscreen(window, cx);
        let fullscreen = self.fullscreen;
        if let Some(v) = self.video.borrow_mut().as_mut() {
            v.set_scale(window.scale_factor() as f64);
        }
        if !fullscreen {
            platform::hide_traffic_lights(window);
        }

        // The root paints nothing: wherever no panel is drawn, mpv's layer
        // underneath shows through.
        div()
            .id("root")
            .key_context("Iris")
            .track_focus(&self.focus)
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
            .on_action(cx.listener(Self::next_item))
            .on_action(cx.listener(Self::prev_item))
            .on_action(cx.listener(Self::open_file))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                if let Some(p) = paths.paths().first() {
                    this.open_path(p, cx);
                }
            }))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                this.mouse_pos = Some(ev.position);
                if this.fullscreen {
                    this.bump_activity(cx);
                }
            }))
            // Hidden until the mouse moves (macOS) / no cursor (Windows).
            .when(!self.chrome_shown(), |d| d.cursor(CursorStyle::None))
            .when(!fullscreen, |d| d.child(self.render_titlebar(cx)))
            .child({
                let side_panel: Option<AnyElement> = (!fullscreen).then(|| {
                    if self.show_directory {
                        self.render_directory(false, cx).into_any_element()
                    } else {
                        self.render_rail(cx).into_any_element()
                    }
                });
                let video = self.render_video_area(fullscreen, cx).into_any_element();
                let row = div().flex_1().min_h(px(0.)).flex();
                if self.settings.directory_side == Side::Left {
                    row.children(side_panel).child(video)
                } else {
                    row.child(video).children(side_panel)
                }
            })
            .when(!fullscreen, |d| {
                d.child(
                    div()
                        .h(px(DOCK_H))
                        .flex_none()
                        .w_full()
                        .bg(panel())
                        .border_t_1()
                        .border_color(line())
                        .child(self.render_dock_body(cx)),
                )
            })
    }
}
