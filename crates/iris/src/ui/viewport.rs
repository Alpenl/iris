//! Viewport: per-session state, native video surface bounds probe, pointer
//! zoom/pan & frameless drag, HUD overlays, and floating chrome slots.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    Context, CursorStyle, InteractiveElement, IntoElement, KeyContext, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Point, SharedString, Styled, Window, canvas, div,
    prelude::FluentBuilder, px,
};
use iris_core::geometry::ImageView;
use iris_core::input::{self, InputContext, PointerCommand, PointerInput};
use iris_core::library::Kind;
use iris_core::player::{MediaStats, Session};
use iris_core::settings::{Side, VideoWheel};

use super::IrisWindow;
use super::context_menu::{ContextMenu, ContextTarget};
use crate::platform;
use crate::theme::{FLOAT_GAP, FONT_MONO, ThemeTokens};
use crate::video::VideoSurface;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct VideoRect(pub(super) [f64; 4]);

pub(super) struct Viewport {
    pub(super) session: Session,
    pub(super) video: Rc<RefCell<Option<VideoSurface>>>,
    pub(super) rect: Rc<Cell<Option<VideoRect>>>,
    pub(super) image: ImageView,
    pub(super) drag_origin: Option<Point<Pixels>>,
    pub(super) pending_win_drag_origin: Option<Point<Pixels>>,
    pub(super) pending_click_origin: Option<Point<Pixels>>,
    pub(super) load_generation: u64,
    pub(super) pending_path: Option<PathBuf>,
    pub(super) pending_subtitles: super::events::PendingSubtitles,
    pub(super) loading_visible: bool,
    pub(super) pending_photo_actual: bool,
    pub(super) cached_stats: Option<MediaStats>,
    pub(super) last_stats_refresh: Instant,
    pub(super) announce_audio: bool,
    pub(super) announce_volume: Option<Instant>,
    pub(super) announce_sub: bool,
}

impl Viewport {
    pub(super) fn new(session: Session, video: Option<VideoSurface>, now: Instant) -> Self {
        Self {
            session,
            video: Rc::new(RefCell::new(video)),
            rect: Rc::new(Cell::new(None)),
            image: ImageView::default(),
            drag_origin: None,
            pending_win_drag_origin: None,
            load_generation: 0,
            pending_click_origin: None,
            pending_path: None,
            pending_subtitles: Default::default(),
            loading_visible: false,
            pending_photo_actual: false,
            cached_stats: None,
            last_stats_refresh: now,
            announce_audio: false,
            announce_volume: None,
            announce_sub: false,
        }
    }

    pub(super) fn refresh_cached_stats(&mut self, full: bool) {
        if full || self.cached_stats.is_none() {
            self.cached_stats = Some(self.session.media_stats());
        } else if let Some(stats) = &mut self.cached_stats {
            self.session.refresh_dynamic_stats(stats);
        }
        self.last_stats_refresh = Instant::now();
    }
}

impl Drop for Viewport {
    fn drop(&mut self) {
        drop(self.video.borrow_mut().take());
        self.session.shutdown();
    }
}

impl IrisWindow {
    pub(super) fn image_geometry(&self) -> ([f64; 2], [f64; 2]) {
        let rect = self.active().rect.get().map(|r| r.0).unwrap_or([0.; 4]);
        let (w, h) = self.active().session.display_size();
        ([rect[2], rect[3]], [w as f64, h as f64])
    }

    pub(super) fn image_geometry_ready(&self) -> bool {
        let (viewport, media) = self.image_geometry();
        viewport[0] > 0. && viewport[1] > 0. && media[0] > 0. && media[1] > 0.
    }

    fn image_can_pan(&self) -> bool {
        let (viewport, media) = self.image_geometry();
        if !self.image_geometry_ready() {
            return false;
        }
        let [width, height] = self.active().image.displayed(viewport, media);
        width > viewport[0] + 0.5 || height > viewport[1] + 0.5
    }

    pub(super) fn apply_image_view(&mut self, cx: &mut Context<Self>) {
        let vp = self.active();
        if let Err(e) = vp.session.transform(vp.image.zoom, vp.image.pan) {
            self.show_toast(format!("缩放失败 · {e}"), cx);
        }
        cx.notify();
    }

    pub(super) fn zoom_at(
        &mut self,
        delta: f64,
        point: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if !self.image_geometry_ready() {
            return;
        }
        let (viewport, media) = self.image_geometry();
        let rect = self.active().rect.get().map(|r| r.0).unwrap_or([0.; 4]);
        let pt = point
            .map(|p| {
                [
                    f32::from(p.x) as f64 - rect[0],
                    f32::from(p.y) as f64 - rect[1],
                ]
            })
            .unwrap_or([viewport[0] / 2., viewport[1] / 2.]);
        self.active_mut().image.zoom_at(delta, pt, viewport, media);
        self.apply_image_view(cx);
    }

    pub(super) fn input_context(&self) -> InputContext {
        InputContext {
            media: self.active().session.kind(),
            zoomed: self.image_can_pan(),
            overlay: self.overlays.has_blocking() || self.plugins.active_modal().is_some(),
            video_wheel_volume: self.settings.video_wheel == VideoWheel::Volume,
            single_click_pause: self.settings.single_click_pause,
            photo_double_click_actual: self.settings.photo_double_click_actual,
        }
    }

    pub(super) fn media_info(&self) -> Option<String> {
        let session = &self.active().session;
        let kind = session.kind()?;
        let st = session.state();
        let (w, h) = session.display_size();
        let mut parts = Vec::new();
        if st.is_live {
            parts.push("LIVE".into());
        }
        if w > 0 && h > 0 && kind != Kind::Audio {
            parts.push(format!("{w}×{h}"));
        }
        match kind {
            Kind::Video => parts.push(if st.hwdec.is_empty() || st.hwdec == "no" {
                "软解".into()
            } else {
                format!("硬解 {}", st.hwdec)
            }),
            Kind::Photo => {
                if let Some(colour) = st.photo_colour {
                    parts.push(colour.to_string());
                }
            }
            Kind::Audio => parts.push("音频".into()),
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    pub(super) fn render_video_area(
        &self,
        fullscreen: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        let vp = self.active();
        let last_rect = vp.rect.clone();
        let video = vp.video.clone();
        let on_move = cx.listener(Self::on_root_mouse_move);
        let on_up = cx.listener(Self::on_root_mouse_up);
        let on_exit = cx.listener(|this, _: &gpui::MouseExitEvent, window, cx| {
            this.on_window_pointer_exit(window, cx);
        });
        let probe = canvas(
            move |bounds, _, _| {
                let r = VideoRect([
                    f32::from(bounds.origin.x) as f64,
                    f32::from(bounds.origin.y) as f64,
                    f32::from(bounds.size.width) as f64,
                    f32::from(bounds.size.height) as f64,
                ]);
                if last_rect.get() == Some(r) {
                    return;
                }
                last_rect.set(Some(r));
                if let Some(v) = video.borrow().as_ref() {
                    let [x, y, w, h] = r.0;
                    v.set_frame(x, y, w, h);
                }
            },
            move |_, _, window, _| {
                // Scrims occlude sibling hitboxes and native drag release may be outside
                // the client area. Both still belong to the window pointer state machine.
                window.on_mouse_event(move |event, phase, window, cx| {
                    if phase.capture() {
                        on_move(event, window, cx);
                    }
                });
                window.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, window, cx| {
                    if phase.capture() && event.button == MouseButton::Left {
                        on_up(event, window, cx);
                    }
                });
                window.on_mouse_event(move |event, phase, window, cx| {
                    if phase.capture() {
                        on_exit(event, window, cx);
                    }
                });
            },
        )
        .absolute()
        .size_full();

        let session = &vp.session;
        let kind = session.kind();
        let floating_dock = fullscreen || self.effective_floating_dock();
        let dock_h = f32::from(t.dock_h);
        let has_media = session.current().is_some() || session.current_uri().is_some();
        let has_visual_surface =
            has_media && session.state().loaded && matches!(kind, Some(Kind::Video | Kind::Photo));
        let is_photo_zoomed = kind == Some(Kind::Photo) && self.image_can_pan();
        let panning = vp.drag_origin.is_some();
        let show_filmstrip = kind == Some(Kind::Photo)
            && self.settings.photo_filmstrip
            && (!fullscreen || self.chrome.chrome_visible)
            && session
                .current()
                .is_some_and(|cur| self.catalog.photo_paths.iter().any(|p| p == cur));

        let top_pill = self
            .chrome
            .toast
            .clone()
            .or_else(|| self.chrome.osd.clone())
            .or_else(|| {
                if session.state().buffering {
                    Some(SharedString::from("缓冲中…"))
                } else if vp.loading_visible {
                    Some(SharedString::from("加载中…"))
                } else {
                    None
                }
            });

        div()
            .id("video")
            .relative()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .overflow_hidden()
            .when(!has_visual_surface, |d| d.bg(t.window_bg))
            .when(self.chrome_shown() && is_photo_zoomed && panning, |d| {
                d.cursor(CursorStyle::ClosedHand)
            })
            .when(self.chrome_shown() && is_photo_zoomed && !panning, |d| {
                d.cursor(CursorStyle::OpenHand)
            })
            .child(probe)
            .key_context({
                let mut kc = KeyContext::default();
                if self.overlays.settings_open
                    || self.overlays.context_menu.is_some()
                    || self.overlays.palette_open
                    || self.overlays.dialog.is_some()
                    || self.plugins.active_modal().is_some()
                    || self.text_inputs.is_any_focused(window)
                {
                    kc.add("Overlay");
                } else {
                    kc.add("Viewport");
                    kc.set(
                        "media",
                        match kind {
                            Some(Kind::Photo) => "image",
                            Some(Kind::Video) => "video",
                            Some(Kind::Audio) => "audio",
                            None => "none",
                        },
                    );
                    if vp.image.zoomed() {
                        kc.add("zoomed");
                    }
                }
                kc
            })
            .track_focus(&self.focus)
            .on_scroll_wheel(cx.listener(|this, ev: &gpui::ScrollWheelEvent, _, cx| {
                let raw_y = f32::from(ev.delta.pixel_delta(px(20.)).y) as f64;
                if raw_y == 0. {
                    return;
                }
                match input::resolve_pointer_input(
                    PointerInput::Wheel(raw_y / 240.),
                    this.input_context(),
                ) {
                    Some(PointerCommand::Zoom(delta)) => {
                        let scale = if ev.delta.precise() { 0.45 } else { 1.0 };
                        this.zoom_at(delta * scale, Some(ev.position), cx);
                        cx.stop_propagation();
                    }
                    Some(PointerCommand::VolumeBy(_)) => {
                        let delta = this.damped_volume_delta(ev);
                        if delta.abs() >= 0.5 {
                            this.adjust_volume_by(delta, cx);
                        }
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.overlays.open_context_menu(ContextMenu {
                        pos: ev.position,
                        target: ContextTarget::Viewport,
                    });
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    if !this.chrome.fullscreen && (ev.modifiers.alt || ev.modifiers.secondary()) {
                        this.chrome.scrub.click_seq += 1;
                        let vp = this.active_mut();
                        vp.drag_origin = None;
                        vp.pending_win_drag_origin = None;
                        vp.pending_click_origin = None;
                        platform::begin_window_drag(window);
                        cx.stop_propagation();
                        return;
                    }
                    let ctx = this.input_context();
                    if ev.click_count >= 2 {
                        this.chrome.scrub.click_seq += 1;
                        this.active_mut().pending_win_drag_origin = None;
                        this.active_mut().pending_click_origin = None;
                        match input::resolve_pointer_input(PointerInput::DoubleClick, ctx) {
                            Some(PointerCommand::CloseOverlay) => this.close_overlays(cx),
                            Some(PointerCommand::ToggleFullscreen) => window.toggle_fullscreen(),
                            Some(PointerCommand::ToggleActualAtCursor) => {
                                this.toggle_image_size_at(ev.position, window, cx);
                            }
                            _ => {}
                        }
                        return;
                    }
                    if !this.chrome.fullscreen
                        && this.effective_hide_titlebar()
                        && !ctx.zoomed
                        && !ctx.overlay
                    {
                        this.active_mut().pending_win_drag_origin = Some(ev.position);
                    }
                    if let Some(PointerCommand::CloseOverlay) =
                        input::resolve_pointer_input(PointerInput::BeginDrag, ctx)
                    {
                        this.close_overlays(cx);
                    } else if let Some(PointerCommand::BeginPan) =
                        input::resolve_pointer_input(PointerInput::BeginDrag, ctx)
                    {
                        this.active_mut().drag_origin = Some(ev.position);
                        cx.notify();
                    } else if let Some(PointerCommand::TogglePlayback) =
                        input::resolve_pointer_input(PointerInput::SingleClick, ctx)
                    {
                        this.active_mut().pending_click_origin = Some(ev.position);
                    }
                }),
            )
            .when(kind == Some(Kind::Audio), |d| {
                d.child(self.render_audio_card(t))
            })
            .children(self.render_plugin_overlay(cx))
            .children(self.render_timed_overlay(cx))
            .when(fullscreen && self.chrome.chrome_visible, |d| {
                let name = session
                    .current()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .or_else(|| session.current_title().map(str::to_string));
                d.when_some(name, |d, name| {
                    let chip = div()
                        .absolute()
                        .top(px(FLOAT_GAP))
                        .max_w(px(520.))
                        .h(px(28.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .rounded(t.radius_lg)
                        .bg(t.overlay)
                        .border_1()
                        .border_color(t.line)
                        .child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .text_size(px(12.))
                                .child(name),
                        )
                        .when_some(self.media_info(), |d, info| {
                            d.child(
                                div()
                                    .flex_none()
                                    .font_family(FONT_MONO)
                                    .text_size(px(11.))
                                    .text_color(t.text_dim)
                                    .child(info),
                            )
                        });
                    d.child(if self.settings.directory_side == Side::Left {
                        chip.right(px(FLOAT_GAP))
                    } else {
                        chip.left(px(FLOAT_GAP))
                    })
                })
            })
            .when_some(top_pill, |d, msg| {
                d.child(
                    div()
                        .absolute()
                        .top(px(if fullscreen { FLOAT_GAP } else { 12. }))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .h(px(28.))
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .rounded(t.radius_lg)
                                .bg(t.overlay)
                                .border_1()
                                .border_color(t.line)
                                .child(div().flex_none().size(px(6.)).bg(t.accent))
                                .child(div().font_family(FONT_MONO).text_size(px(12.)).child(msg)),
                        ),
                )
            })
            .when(self.overlays.info_open && has_media, |d| {
                d.child(self.render_info_overlay(fullscreen, cx))
            })
            .when(
                !has_media && vp.pending_path.is_none() && !vp.loading_visible,
                |d| d.child(self.render_empty_state(t, cx)),
            )
            .when(show_filmstrip, |d| {
                d.children(self.render_photo_filmstrip(fullscreen, t, cx))
            })
            .when(fullscreen && self.overlays.dir_popup.is_open(), |d| {
                let popup = div()
                    .absolute()
                    .top_0()
                    .bottom(px(dock_h))
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .child(self.render_sidebar(true, window, cx));
                d.child(if self.settings.directory_side == Side::Left {
                    popup.left_0()
                } else {
                    popup.right_0()
                })
            })
            .when(floating_dock && self.chrome.chrome_visible, |d| {
                let slot = self.chrome.probes.dock_bounds.clone();
                let seam = matches!(kind, Some(Kind::Video | Kind::Audio));
                d.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .h(t.dock_h)
                        .bg(t.overlay)
                        .border_color(t.line)
                        .when(!seam, |d| d.border_t_1())
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                        .child(
                            canvas(move |b, _, _| slot.set(Some(b)), |_, _, _, _| {})
                                .absolute()
                                .size_full(),
                        )
                        .child(self.render_dock(true, window, cx)),
                )
            })
            .when_some(self.overlays.popover, |d, pop| {
                d.child(self.render_popover(pop, fullscreen, window, cx))
            })
    }
}
