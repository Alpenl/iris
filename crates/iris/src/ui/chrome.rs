//! Fullscreen chrome auto-hide, OSD/Toast pills, layout measurement probes,
//! sidebar drag-resizing, and timeline/volume pointer math.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    Bounds, Context, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent,
    SharedString, Size, Window, px,
};
use iris_core::player::Command;
use iris_core::settings::{MAX_DIRECTORY_WIDTH, MIN_DIRECTORY_WIDTH, Side};

use super::IrisWindow;
use super::state::{PopoverKind, ScrubState};
use crate::platform;
use crate::theme::ThemeTokens;

const SEEK_DRAG_INTERVAL: Duration = Duration::from_millis(40);
const FULLSCREEN_WAKE_DISTANCE: f32 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DirectoryPopup {
    Hidden,
    Hover,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectoryZone {
    Outside,
    Edge,
    Panel,
}

impl DirectoryZone {
    fn at(
        position: Point<Pixels>,
        viewport: Size<Pixels>,
        dock_h: Pixels,
        side: Side,
        panel_w: Pixels,
    ) -> Self {
        // GPUI positions and sizes are local logical pixels, including on secondary displays.
        if position.x < px(0.)
            || position.x >= viewport.width
            || position.y < px(0.)
            || position.y >= viewport.height - dock_h
        {
            return Self::Outside;
        }
        let distance = match side {
            Side::Left => position.x,
            Side::Right => viewport.width - position.x,
        };
        if distance <= px(4.) {
            Self::Edge
        } else if distance <= panel_w {
            Self::Panel
        } else {
            Self::Outside
        }
    }
}

impl DirectoryPopup {
    pub(super) fn is_open(self) -> bool {
        self != Self::Hidden
    }

    fn on_pointer(self, previous: DirectoryZone, current: DirectoryZone) -> Self {
        match self {
            Self::Hidden if current == DirectoryZone::Edge && previous != DirectoryZone::Edge => {
                Self::Hover
            }
            Self::Hover if current == DirectoryZone::Outside => Self::Hidden,
            // A keyboard or toolbar toggle owns its panel until explicitly dismissed.
            _ => self,
        }
    }
}

/// Shared layout probes written during GPUI paint/prepaint for hit-testing and popovers.
#[derive(Default)]
pub(super) struct LayoutProbes {
    pub(super) progress_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(super) vol_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(super) dock_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(super) anchor_sub: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(super) anchor_audio: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(super) anchor_speed: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(super) anchor_more: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(super) anchor_presets: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(super) anchor_variants: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl LayoutProbes {
    pub(super) fn popover_anchor(&self, kind: PopoverKind) -> Option<Bounds<Pixels>> {
        match kind {
            PopoverKind::Subtitles => self.anchor_sub.get(),
            PopoverKind::Audio => self.anchor_audio.get(),
            PopoverKind::Speed => self.anchor_speed.get(),
            PopoverKind::More => self.anchor_more.get(),
            PopoverKind::Presets => self.anchor_presets.get(),
            PopoverKind::Variants => self.anchor_variants.get(),
        }
    }
}

/// Window chrome state: fullscreen visibility, hover hotzones, scrub state, OSD/Toast.
pub(super) struct WindowChromeState {
    pub(super) probes: LayoutProbes,
    pub(super) scrub: ScrubState,
    pub(super) fullscreen: bool,
    pub(super) chrome_visible: bool,
    pub(super) titlebar_hover: bool,
    pub(super) show_remaining: bool,
    pub(super) resizing_dir: Option<(f32, f32)>,
    pub(super) last_window_size: Option<(f32, f32)>,
    pub(super) pending_window_size: Option<(f32, f32)>,
    pub(super) last_activity: Instant,
    pub(super) hide_armed: bool,
    pub(super) mouse_pos: Option<Point<Pixels>>,
    pub(super) last_pointer_pos: Option<Point<Pixels>>,
    pub(super) toast: Option<SharedString>,
    pub(super) toast_seq: u64,
    pub(super) osd: Option<SharedString>,
    pub(super) osd_seq: u64,
    pub(super) last_render_sec: i64,
    pub(super) last_render_bar_px: i32,
    pub(super) last_render_cache_px: i32,
    pub(super) last_cache_tick: Instant,
    pub(super) last_bar_tick: Instant,
    pub(super) last_scale: f64,
    pub(super) render_count: u64,
}

impl WindowChromeState {
    pub(super) fn new(now: Instant, initial_scale: f64) -> Self {
        Self {
            probes: LayoutProbes::default(),
            scrub: ScrubState::default(),
            fullscreen: false,
            chrome_visible: true,
            titlebar_hover: false,
            show_remaining: false,
            resizing_dir: None,
            pending_window_size: None,
            last_window_size: None,
            last_activity: now,
            hide_armed: false,
            mouse_pos: None,
            last_pointer_pos: None,
            toast: None,
            toast_seq: 0,
            osd: None,
            osd_seq: 0,
            last_render_sec: -1,
            last_render_bar_px: -1,
            last_render_cache_px: -1,
            last_cache_tick: now,
            last_bar_tick: now,
            last_scale: initial_scale,
            render_count: 0,
        }
    }
}

impl IrisWindow {
    pub(super) fn popover_anchor(&self, kind: PopoverKind) -> Option<Bounds<Pixels>> {
        self.chrome.probes.popover_anchor(kind)
    }

    pub(super) fn toggle_popover(&mut self, kind: PopoverKind, cx: &mut Context<Self>) {
        self.overlays.toggle_popover(kind);
        self.bump_activity(cx);
        cx.notify();
    }

    pub(super) fn close_overlays(&mut self, cx: &mut Context<Self>) {
        self.overlays.close_all();
        self.bump_activity(cx);
        cx.notify();
    }

    pub(super) fn chrome_shown(&self) -> bool {
        !self.chrome.fullscreen || self.chrome.chrome_visible
    }

    pub(super) fn sync_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fs = window.is_fullscreen();
        if fs == self.chrome.fullscreen {
            // Windows reports a move onto another monitor via window hover loss.
            if !platform::IS_MACOS && !window.is_window_hovered() {
                self.on_window_pointer_exit(window, cx);
            }
            return;
        }
        self.chrome.fullscreen = fs;
        self.overlays.dir_popup = DirectoryPopup::Hidden;
        let pos = window.mouse_position();
        self.chrome.mouse_pos = Some(pos);
        self.chrome.last_pointer_pos = Some(pos);
        if !self.sidebar_is_visible() && self.text_inputs.search_focus.is_focused(window) {
            self.focus.focus(window);
        }
        self.overlays.close_menus();
        self.chrome.chrome_visible = true;
        if fs {
            self.bump_activity(cx);
        } else {
            platform::hide_traffic_lights(window);
            if let Some((w, h)) = self.chrome.pending_window_size.take() {
                window.resize(gpui::size(px(w), px(h)));
            }
        }
    }

    pub(super) fn bump_activity(&mut self, cx: &mut Context<Self>) {
        self.chrome.last_activity = Instant::now();
        if !self.chrome.chrome_visible {
            self.chrome.chrome_visible = true;
            cx.notify();
        }
        self.arm_fullscreen_hide(cx);
    }

    fn arm_fullscreen_hide(&mut self, cx: &mut Context<Self>) {
        if self.chrome.fullscreen && !self.chrome.hide_armed {
            self.chrome.hide_armed = true;
            cx.spawn(async move |this, cx| {
                while let Ok(Some(wait)) = this.update(cx, |this, cx| this.hide_check(cx)) {
                    cx.background_executor().timer(wait).await;
                }
            })
            .detach();
        }
    }

    fn hide_check(&mut self, cx: &mut Context<Self>) -> Option<Duration> {
        let over_dock = match (self.chrome.mouse_pos, self.chrome.probes.dock_bounds.get()) {
            (Some(p), Some(b)) => b.contains(&p),
            _ => false,
        };
        if self.chrome.fullscreen
            && self.chrome.chrome_visible
            && !self.overlays.has_blocking()
            && self.plugins.active_modal().is_none()
            && self.chrome.resizing_dir.is_none()
            && !self.chrome.scrub.seeking
            && !self.chrome.scrub.dragging_volume
            && !over_dock
        {
            let delay = Duration::from_millis(self.settings.fullscreen_hide_ms.clamp(1000, 10000));
            let idle = self.chrome.last_activity.elapsed();
            if idle < delay {
                return Some(delay - idle);
            }
            self.chrome.chrome_visible = false;
            self.chrome.scrub.timeline_hover = false;
            if let Some(pos) = self.chrome.mouse_pos {
                self.chrome.last_pointer_pos = Some(pos);
            }
            cx.notify();
        }
        self.chrome.hide_armed = false;
        None
    }

    pub(super) fn show_toast(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.chrome.toast = Some(msg.into());
        self.chrome.toast_seq += 1;
        let seq = self.chrome.toast_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1800))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.chrome.toast_seq == seq {
                    this.chrome.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn show_osd(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.chrome.osd = Some(msg.into());
        self.chrome.osd_seq += 1;
        let seq = self.chrome.osd_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.chrome.osd_seq == seq {
                    this.chrome.osd = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn set_directory_side(&mut self, side: Side, cx: &mut Context<Self>) {
        if self.settings.directory_side == side {
            return;
        }
        self.settings.directory_side = side;
        self.persist_user_settings();
        self.show_toast(
            if side == Side::Left {
                "侧栏移到左侧"
            } else {
                "侧栏移到右侧"
            },
            cx,
        );
    }

    pub(super) fn drag_sidebar_width(
        &mut self,
        pointer_x: f32,
        viewport_w: f32,
        start_x: f32,
        start_w: f32,
    ) {
        let dx = pointer_x - start_x;
        let signed = if self.settings.directory_side == Side::Left {
            dx
        } else {
            -dx
        };
        let max = if viewport_w > 0. {
            (viewport_w * 0.45).clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH)
        } else {
            MAX_DIRECTORY_WIDTH
        };
        self.settings.directory_width = (start_w + signed).clamp(MIN_DIRECTORY_WIDTH, max);
    }

    pub(super) fn update_seek_preview(&mut self, x: Pixels) {
        let Some(b) = self.chrome.probes.progress_bounds.get() else {
            return;
        };
        if b.size.width > px(0.) {
            self.chrome.scrub.seek_preview_ratio =
                Some(((x - b.origin.x) / b.size.width).clamp(0., 1.));
        }
    }

    pub(super) fn seek_to_x(&mut self, x: Pixels, exact: bool) {
        let Some(b) = self.chrome.probes.progress_bounds.get() else {
            return;
        };
        if self.active().session.state().duration <= 0. || b.size.width <= px(0.) {
            return;
        }
        let pct = (((x - b.origin.x) / b.size.width).clamp(0., 1.) as f64) * 100.;
        let cmd = if exact {
            Command::SeekPercent(pct)
        } else {
            Command::SeekPercentFast(pct)
        };
        if let Err(e) = self.active_mut().session.execute(cmd) {
            log::error!("seek: {e}");
        }
    }

    pub(super) fn set_volume_from_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some(b) = self.chrome.probes.vol_bounds.get() else {
            return;
        };
        if b.size.width <= px(0.) {
            return;
        }
        let vol = ((((x - b.origin.x) / b.size.width).clamp(0., 1.) as f64) * 100.).round();
        self.command(Command::SetVolume(vol), cx);
        cx.notify();
    }

    pub(super) fn damped_volume_delta(&mut self, ev: &ScrollWheelEvent) -> f64 {
        let raw_y = f32::from(ev.delta.pixel_delta(px(20.)).y) as f64;
        if ev.delta.precise() {
            self.chrome.scrub.wheel_accum += raw_y * 0.14;
            let whole = self.chrome.scrub.wheel_accum.trunc();
            if whole.abs() >= 1. {
                self.chrome.scrub.wheel_accum -= whole;
                whole.clamp(-8., 8.)
            } else {
                0.
            }
        } else {
            (raw_y / 10.).clamp(-10., 10.)
        }
    }

    pub(super) fn damped_wheel_steps(&mut self, ev: &ScrollWheelEvent, threshold: f64) -> isize {
        let raw_y = f32::from(ev.delta.pixel_delta(px(20.)).y) as f64;
        if ev.delta.precise() {
            self.chrome.scrub.wheel_accum += raw_y;
            if self.chrome.scrub.wheel_accum.abs() >= threshold {
                let dir = if self.chrome.scrub.wheel_accum > 0. {
                    1
                } else {
                    -1
                };
                self.chrome.scrub.wheel_accum = 0.;
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

    fn directory_hover_allowed(&self, window: &Window) -> bool {
        !self.overlays.has_menu_or_modal()
            && self.plugins.active_modal().is_none()
            && !self.text_inputs.is_any_focused(window)
    }

    pub(super) fn on_window_pointer_exit(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.chrome.mouse_pos = None;
        let changed = std::mem::take(&mut self.chrome.titlebar_hover)
            | std::mem::take(&mut self.chrome.scrub.timeline_hover);
        if changed {
            cx.notify();
        }
        if self.overlays.dir_popup == DirectoryPopup::Hover && self.directory_hover_allowed(window)
        {
            self.overlays.dir_popup = DirectoryPopup::Hidden;
            self.bump_activity(cx);
            cx.notify();
        } else if self.chrome.fullscreen && self.chrome.chrome_visible {
            self.arm_fullscreen_hide(cx);
        }
    }

    pub(super) fn on_root_mouse_move(
        &mut self,
        ev: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self.chrome.mouse_pos.replace(ev.position);
        let wake_fullscreen = should_wake_fullscreen_chrome(
            self.chrome.last_pointer_pos,
            ev.position,
            ev.pressed_button.is_some(),
        );
        if wake_fullscreen {
            self.chrome.last_pointer_pos = Some(ev.position);
        }
        let t = ThemeTokens::of(cx);
        if self.chrome.fullscreen
            && ev.pressed_button.is_none()
            && self.directory_hover_allowed(window)
        {
            let viewport = window.viewport_size();
            let zone = |position| {
                DirectoryZone::at(
                    position,
                    viewport,
                    t.dock_h,
                    self.settings.directory_side,
                    px(self.settings.directory_width),
                )
            };
            let next = self.overlays.dir_popup.on_pointer(
                previous.map_or(DirectoryZone::Outside, zone),
                zone(ev.position),
            );
            if next != self.overlays.dir_popup {
                self.overlays.dir_popup = next;
                cx.notify();
            }
        }
        let hot_h = f32::from(t.titlebar_h) + 8.;
        let in_hot = !self.chrome.fullscreen
            && ev.position.x >= px(0.)
            && ev.position.x < window.viewport_size().width
            && ev.position.y >= px(0.)
            && ev.position.y <= px(hot_h);
        if in_hot != self.chrome.titlebar_hover {
            self.chrome.titlebar_hover = in_hot;
            if self.effective_hide_titlebar() {
                cx.notify();
            }
        }
        if let Some((start_x, start_w)) = self.chrome.resizing_dir {
            if ev.pressed_button == Some(MouseButton::Left) {
                self.drag_sidebar_width(
                    f32::from(ev.position.x),
                    f32::from(window.viewport_size().width),
                    start_x,
                    start_w,
                );
                cx.notify();
            } else {
                self.chrome.resizing_dir = None;
                self.persist_user_settings();
                cx.notify();
            }
            return;
        }
        if self.chrome.scrub.dragging_volume {
            if ev.pressed_button == Some(MouseButton::Left) {
                self.set_volume_from_x(ev.position.x, cx);
            } else {
                self.chrome.scrub.dragging_volume = false;
                cx.notify();
            }
            return;
        }
        if self.chrome.scrub.seeking {
            if ev.pressed_button == Some(MouseButton::Left) {
                self.chrome.scrub.seek_dragged = true;
                self.update_seek_preview(ev.position.x);
                if self.chrome.scrub.last_seek_drag.elapsed() >= SEEK_DRAG_INTERVAL {
                    self.chrome.scrub.last_seek_drag = Instant::now();
                    self.seek_to_x(ev.position.x, false);
                }
            } else {
                self.finish_seek(ev.position.x, cx);
            }
            cx.notify();
        } else if self.chrome_shown() && self.chrome.scrub.timeline_hover {
            cx.notify();
        }
        if ev.pressed_button == Some(MouseButton::Left) {
            if self
                .active()
                .pending_click_origin
                .is_some_and(|start| pointer_distance(start, ev.position) > 5.)
            {
                self.active_mut().pending_click_origin = None;
            }
            if let Some(start) = self.active().pending_win_drag_origin
                && pointer_distance(start, ev.position) > 5.
            {
                self.active_mut().pending_win_drag_origin = None;
                self.active_mut().pending_click_origin = None;
                self.chrome.scrub.click_seq += 1;
                platform::begin_window_drag(window);
                return;
            }
            if let Some(origin) = self.active().drag_origin
                && self.image_geometry_ready()
            {
                self.active_mut().drag_origin = Some(ev.position);
                let delta = [
                    f32::from(ev.position.x - origin.x) as f64,
                    f32::from(ev.position.y - origin.y) as f64,
                ];
                let (viewport, media) = self.image_geometry();
                self.active_mut().image.pan_by(delta, viewport, media);
                self.apply_image_view(cx);
            }
        } else {
            self.active_mut().pending_win_drag_origin = None;
            self.active_mut().pending_click_origin = None;
            if self.active_mut().drag_origin.take().is_some() {
                cx.notify();
            }
        }
        if self.chrome.fullscreen && wake_fullscreen {
            self.bump_activity(cx);
        }
    }

    pub(super) fn on_root_mouse_up(
        &mut self,
        ev: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_mut().pending_win_drag_origin = None;
        if let Some(start) = self.active_mut().pending_click_origin.take()
            && pointer_distance(start, ev.position) <= 5.
            && ev.position.x >= px(0.)
            && ev.position.y >= px(0.)
            && ev.position.x < window.viewport_size().width
            && ev.position.y < window.viewport_size().height
            && !self.input_context().overlay
        {
            self.chrome.scrub.click_seq += 1;
            let seq = self.chrome.scrub.click_seq;
            let generation = self.active().load_generation;
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.chrome.scrub.click_seq == seq
                        && this.active().load_generation == generation
                        && !this.input_context().overlay
                    {
                        let state = this.active().session.state();
                        let pause = !state.paused && !state.ended;
                        this.command(Command::TogglePlayback, cx);
                        this.show_osd(if pause { "暂停" } else { "播放" }, cx);
                    }
                });
            })
            .detach();
        }
        if self.chrome.resizing_dir.take().is_some() {
            self.persist_user_settings();
            cx.notify();
        }
        if self.chrome.scrub.dragging_volume {
            self.chrome.scrub.dragging_volume = false;
            cx.notify();
        }
        if self.active_mut().drag_origin.take().is_some() {
            cx.notify();
        }
        self.finish_seek(ev.position.x, cx);
    }

    fn finish_seek(&mut self, x: Pixels, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.chrome.scrub.seeking) {
            return;
        }
        let dragged = std::mem::take(&mut self.chrome.scrub.seek_dragged);
        if dragged {
            self.update_seek_preview(x);
            self.seek_to_x(x, true);
        }
        self.chrome.scrub.seek_preview_timeout = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(350))
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.chrome.scrub.seeking
                    && this.chrome.scrub.seek_preview_ratio.take().is_some()
                {
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }
}

fn pointer_distance(a: Point<Pixels>, b: Point<Pixels>) -> f32 {
    f32::from(a.x - b.x).hypot(f32::from(a.y - b.y))
}

fn should_wake_fullscreen_chrome(
    last: Option<Point<Pixels>>,
    current: Point<Pixels>,
    pressed: bool,
) -> bool {
    pressed || last.is_none_or(|prev| pointer_distance(prev, current) >= FULLSCREEN_WAKE_DISTANCE)
}

#[cfg(test)]
mod tests {
    use super::{DirectoryPopup, DirectoryZone, should_wake_fullscreen_chrome};
    use gpui::px;
    use iris_core::settings::Side;

    #[test]
    fn fullscreen_chrome_ignores_synthetic_zero_distance_mouse_moves() {
        let origin = gpui::point(px(640.), px(360.));
        assert!(should_wake_fullscreen_chrome(None, origin, false));
        // Synthetic WM_MOUSEMOVE / cursor-hide refreshes at the same spot or sub-pixel DPI jitter.
        assert!(!should_wake_fullscreen_chrome(Some(origin), origin, false));
        assert!(!should_wake_fullscreen_chrome(
            Some(origin),
            gpui::point(px(640.6), px(360.4)),
            false,
        ));
        // Real user pointer motion or drag immediately wakes fullscreen chrome.
        assert!(should_wake_fullscreen_chrome(
            Some(origin),
            gpui::point(px(643.), px(360.)),
            false,
        ));
        assert!(should_wake_fullscreen_chrome(Some(origin), origin, true));
    }

    #[test]
    fn fullscreen_directory_edges_use_viewport_coordinates_and_exclude_the_dock() {
        use DirectoryZone::{Edge, Outside, Panel};

        for width in [960., 1920.] {
            let viewport = gpui::size(px(width), px(720.));
            for side in [Side::Left, Side::Right] {
                let zone = |distance: f32, y: f32| {
                    let x = if side == Side::Left {
                        distance
                    } else {
                        width - distance
                    };
                    DirectoryZone::at(gpui::point(px(x), px(y)), viewport, px(44.), side, px(280.))
                };
                assert_eq!(zone(1., 0.), Edge);
                assert_eq!(zone(4., 675.), Edge);
                assert_eq!(zone(5., 360.), Panel);
                assert_eq!(zone(279., 360.), Panel);
                assert_eq!(zone(281., 360.), Outside);
                assert_eq!(zone(width - 1., 360.), Outside);
                assert_eq!(zone(1., 676.), Outside);
                assert_eq!(zone(-1., 360.), Outside);
            }
        }
    }

    #[test]
    fn directory_hover_requires_fresh_edge_entry_and_respects_manual_open() {
        use DirectoryPopup::{Hidden, Hover, Manual};
        use DirectoryZone::{Edge, Outside, Panel};

        assert_eq!(Hidden.on_pointer(Outside, Panel), Hidden);
        assert_eq!(Hidden.on_pointer(Outside, Edge), Hover);
        assert_eq!(Hover.on_pointer(Edge, Panel), Hover);
        assert_eq!(Hover.on_pointer(Panel, Outside), Hidden);
        // Esc at the edge stays dismissed through further moves within the trigger strip.
        assert_eq!(Hidden.on_pointer(Edge, Edge), Hidden);
        assert_eq!(Hidden.on_pointer(Panel, Edge), Hover);
        assert_eq!(Manual.on_pointer(Panel, Outside), Manual);
    }
}
