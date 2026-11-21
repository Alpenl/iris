//! The spike's main window: title bar, docked directory, video area, dock,
//! and overlays drawn by GPUI on top of mpv's video.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    AnyElement, App, Bounds, Context, CursorStyle, Div, ExternalPaths, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement, PathPromptOptions, Pixels, Point, Render, Rgba, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, actions, canvas, div,
    prelude::FluentBuilder, px, relative,
};

use crate::geometry::ImageView;
use crate::input::{self, InputContext, PointerInput, PointerCommand};
use crate::library::Kind;
use crate::platform;
use crate::player::{Command, Session, SessionEvent};
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
        Quit
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("space", TogglePause, Some("Viewport && media == video")),
        KeyBinding::new("left", SeekBackwardExact, Some("Viewport && media == video")),
        KeyBinding::new("right", SeekForwardExact, Some("Viewport && media == video")),
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
        KeyBinding::new("left", OpenPreviousMedia, Some("Viewport && media == image")),
        KeyBinding::new("right", OpenNextMedia, Some("Viewport && media == image")),
        KeyBinding::new("tab", ToggleDirectory, Some("Workspace")),
        KeyBinding::new("f", ToggleFullscreen, Some("Workspace")),
        KeyBinding::new("escape", ExitFullscreen, Some("Workspace")),
        KeyBinding::new("pagedown", OpenNextMedia, Some("Viewport")),
        KeyBinding::new("pageup", OpenPreviousMedia, Some("Viewport")),
        KeyBinding::new("secondary-o", OpenFile, Some("Workspace")),
        KeyBinding::new("secondary-q", Quit, Some("Workspace")),
    ]);
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct VideoRect([f64; 4]);

struct Viewport {
    session: Session,
    video: Rc<RefCell<Option<VideoSurface>>>,
    rect: Rc<Cell<Option<VideoRect>>>,
    image: ImageView,
    drag_origin: Option<Point<Pixels>>,
}

impl Drop for Viewport {
    fn drop(&mut self) {
        drop(self.video.borrow_mut().take());
        self.session.shutdown();
    }
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
        // Free the render context before mpv goes away, then stop mpv.
        drop(self.viewports[0].video.borrow_mut().take());
        self.viewports[0].session.shutdown();
    }

    fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Err(e) = self.viewports[0].session.open(path) {
            self.show_toast(format!("无法打开 · {e}"), cx);
        }
        self.viewports[0].image.reset();
        cx.notify();
    }

    fn play(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Err(e) = self.viewports[0].session.play(index) {
            self.show_toast(format!("无法播放 · {e}"), cx);
        }
        self.viewports[0].image.reset();
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Err(e) = self.viewports[0].session.step(delta) {
            self.show_toast(format!("无法切换 · {e}"), cx);
        }
        self.viewports[0].image.reset();
        cx.notify();
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

    fn on_event(&mut self, event: SessionEvent, cx: &mut Context<Self>) {
        self.viewports[0].session.apply(&event);
        match event {
            SessionEvent::Position(t) => {
                let tick = (t * 4.).floor() as i64;
                if tick != self.tick {
                    self.tick = tick;
                    if self.chrome_shown() { cx.notify(); }
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
        let rect = self.viewports[0].rect.get().map(|r| r.0).unwrap_or([0.;4]);
        let state = &self.viewports[0].session.state;
        let (mut w,mut h) = state.video_size;
        if state.rotation.rem_euclid(180) == 90 { std::mem::swap(&mut w, &mut h); }
        ([rect[2],rect[3]], [w.max(1) as f64,h.max(1) as f64])
    }

    fn apply_image_view(&mut self, cx: &mut Context<Self>) {
        let view = &self.viewports[0];
        if let Err(e) = view.session.transform(view.image.zoom, view.image.pan) {
            self.show_toast(format!("缩放失败 · {e}"), cx);
        }
        cx.notify();
    }

    fn toggle_image_size(&mut self, _: &ToggleImageSize, window: &mut Window, cx: &mut Context<Self>) {
        let (viewport, media) = self.image_geometry();
        self.viewports[0].image.toggle_actual_size(viewport, media, window.scale_factor() as f64);
        self.apply_image_view(cx);
    }

    fn fit_image(&mut self, _: &FitImage, _: &mut Window, cx: &mut Context<Self>) {
        self.viewports[0].image.reset();
        self.apply_image_view(cx);
    }

    fn zoom_at(&mut self, delta: f64, point: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let (viewport, media) = self.image_geometry();
        let rect = self.viewports[0].rect.get().map(|r|r.0).unwrap_or([0.;4]);
        let point = point.map(|p| [f32::from(p.x) as f64-rect[0], f32::from(p.y) as f64-rect[1]])
            .unwrap_or([viewport[0]/2.,viewport[1]/2.]);
        self.viewports[0].image.zoom_at(delta, point, viewport, media);
        self.apply_image_view(cx);
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) { self.zoom_at(0.25, None, cx); }
    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) { self.zoom_at(-0.25, None, cx); }

    fn input_context(&self) -> InputContext {
        InputContext { media: self.viewports[0].session.kind(), zoomed: self.viewports[0].image.zoomed(), overlay: self.dir_popup }
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
        if self.viewports[0].session.state.duration <= 0. || b.size.width <= px(0.) {
            return;
        }
        let ratio = ((x - b.origin.x) / b.size.width).clamp(0., 1.) as f64;
        if let Err(e) = self.viewports[0].session.execute(Command::SeekPercent(ratio * 100.)) {
            log::error!("seek: {e}");
        }
    }

    // ---- view pieces ----

    fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let file = self.viewports[0].session.current.as_ref()
            .and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let folder = self.viewports[0].session.folder.as_ref()
            .and_then(|f| f.file_name()).map(|n| n.to_string_lossy().into_owned())
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
        let videos = self.viewports[0].session.entries
            .iter()
            .filter(|e| e.kind == Kind::Video)
            .count();
        let photos = self.viewports[0].session.entries.len() - videos;
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
        let items = self.viewports[0].session.entries.iter().enumerate().map(|(i, e)| {
            let is_current = self.viewports[0].session.current.as_ref() == Some(&e.path);
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
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
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
                    .child(format!("{}", self.viewports[0].session.entries.len())),
            )
    }

    fn render_video_area(&self, fullscreen: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let last_rect = self.viewports[0].rect.clone();
        let video = self.viewports[0].video.clone();
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
            let (w, h) = self.viewports[0].session.state.video_size;
            let mut parts = Vec::new();
            if w > 0 {
                parts.push(format!("{w}×{h}"));
            }
            parts.push(if self.viewports[0].session.state.hwdec.is_empty() || self.viewports[0].session.state.hwdec == "no" {
                "软解".into()
            } else {
                format!("硬解 {}", self.viewports[0].session.state.hwdec)
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
            .key_context({
                let mut context = gpui::KeyContext::default();
                context.add("Viewport");
                context.set("media", match self.viewports[0].session.kind() {
                    Some(Kind::Photo) => "image", Some(Kind::Video) => "video", None => "none",
                });
                if self.viewports[0].image.zoomed() { context.add("zoomed"); }
                context
            })
            .track_focus(&self.focus)
            .on_scroll_wheel(cx.listener(|this, ev: &gpui::ScrollWheelEvent, _, cx| {
                let delta = f32::from(ev.delta.pixel_delta(px(20.)).y) as f64 / 240.;
                if let Some(PointerCommand::Zoom(delta)) = input::resolve_pointer_input(PointerInput::Wheel(delta), this.input_context()) {
                    this.zoom_at(delta, Some(ev.position), cx);
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                let input = if ev.click_count == 2 { PointerInput::DoubleClick } else { PointerInput::BeginDrag };
                match input::resolve_pointer_input(input, this.input_context()) {
                    Some(PointerCommand::CloseOverlay) => this.close_popup(cx),
                    Some(PointerCommand::ToggleFullscreen) => window.toggle_fullscreen(),
                    Some(PointerCommand::BeginPan) => this.viewports[0].drag_origin = Some(ev.position),
                    _ => {}
                }
            }))
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
                d.child(
                    div()
                        .absolute()
                        .top(px(44.))
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
            .when(self.viewports[0].session.current.is_none(), |d| {
                d.child(
                    div()
                        .absolute()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(text_dim())
                        .text_size(px(13.))
                        .child(if cfg!(target_os = "macos") {
                            "拖入文件或文件夹，或按 ⌘O 打开"
                        } else {
                            "拖入文件或文件夹，或按 Ctrl+O 打开"
                        }),
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
                        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
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
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .child(self.render_directory(true, cx));
                d.child(if self.settings.directory_side == Side::Left {
                    popup.left(px(16.))
                } else {
                    popup.right(px(16.))
                })
            })
    }

    fn render_dock_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pct = if self.viewports[0].session.state.duration > 0. {
            (self.viewports[0].session.state.time_pos / self.viewports[0].session.state.duration).clamp(0., 1.) as f32
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
        let vol_segments = (self.viewports[0].session.state.volume / 10.).round().clamp(0., 10.) as usize;
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
                    .child(fmt_time(self.viewports[0].session.state.time_pos))
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
                    .child(fmt_time(self.viewports[0].session.state.duration)),
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
                                    .child(if self.viewports[0].session.kind() == Some(Kind::Photo) { "1:1" } else if self.viewports[0].session.state.paused { "▶" } else { "❚❚" })
                                    .border_color(accent())
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        if this.viewports[0].session.kind() == Some(Kind::Photo) {
                                            this.toggle_image_size(&ToggleImageSize, w, cx);
                                        } else { this.toggle_pause(&TogglePause, w, cx); }
                                    })),
                            )
                            .child(
                                square("next")
                                    .child("⏭")
                                    .on_click(cx.listener(|this, _, _, cx| this.step(1, cx))),
                            )
                            .child(div().pl(px(12.)).font_family(FONT_MONO).text_size(px(22.)).child(
                                if self.viewports[0].session.kind() == Some(Kind::Photo) {
                                    format!("{:.0}%", self.viewports[0].image.zoom.exp2() * 100.)
                                } else { fmt_time(self.viewports[0].session.state.time_pos) }
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(24.))
                            .child(field(
                                "字幕",
                                if self.viewports[0].session.state.subs_visible {
                                    "开".into()
                                } else {
                                    "关".into()
                                },
                            ))
                            .child(div().id("audio-track").cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| this.command(Command::NextAudio, cx)))
                                .child(field("音轨", self.viewports[0].session.state.audio.clone())))
                            .child(field("速度", format!("{:.2}×", self.viewports[0].session.state.speed)))
                            .when(self.viewports[0].session.kind() == Some(Kind::Photo), |d| {
                                d.child(div().id("fit-image").cursor_pointer().child("适应窗口")
                                    .on_click(cx.listener(|this, _, window, cx| this.fit_image(&FitImage, window, cx))))
                            })
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
                                            .child(format!("音量 {:.0}", self.viewports[0].session.state.volume)),
                                    )
                                    .child(div().flex().gap(px(2.)).children((0..10).map(|i| {
                                        div()
                                            .id(("vol", i))
                                            .w(px(5.))
                                            .h(px(12.))
                                            .bg(if i < vol_segments { accent() } else { line() })
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.command(Command::SetVolume(((i + 1) * 10) as f64), cx);
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
        if let Some(v) = self.viewports[0].video.borrow_mut().as_mut() {
            v.set_scale(window.scale_factor() as f64);
        }
        if !fullscreen {
            platform::hide_traffic_lights(window);
        }
        let inset = if fullscreen && self.chrome_visible { DOCK_H as f64 + 12. } else { 0. };
        // Fullscreen changes layout this frame; the canvas probe still holds
        // the previous windowed rectangle until prepaint.
        let height = if fullscreen { f32::from(window.viewport_size().height) as f64 }
            else { self.viewports[0].rect.get().map(|r| r.0[3]).unwrap_or(0.) };
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
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                if let Some(p) = paths.paths().first() {
                    this.open_path(p, cx);
                }
            }))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                this.mouse_pos = Some(ev.position);
                if ev.pressed_button == Some(MouseButton::Left) {
                    if let Some(origin) = this.viewports[0].drag_origin {
                        this.viewports[0].drag_origin = Some(ev.position);
                        let delta = [f32::from(ev.position.x-origin.x) as f64, f32::from(ev.position.y-origin.y) as f64];
                        let (viewport, media) = this.image_geometry();
                        this.viewports[0].image.pan_by(delta, viewport, media);
                        this.apply_image_view(cx);
                    }
                } else { this.viewports[0].drag_origin = None; }
                if this.fullscreen {
                    this.bump_activity(cx);
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.viewports[0].drag_origin = None))
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
