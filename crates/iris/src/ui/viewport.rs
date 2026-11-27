//! Viewport: pure UI surface state (`VideoSurface`, `VideoRect` bounds probe,
//! `ImageView`, pointer zoom/pan & frameless drag origins), HUD overlays, and
//! floating chrome slots.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    Context, CursorStyle, InteractiveElement, IntoElement, KeyContext, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Point, Styled, Window, canvas, div, prelude::FluentBuilder, px,
};
use iris_core::geometry::ImageView;
use iris_core::input::{self, InputContext, PointerCommand, PointerInput};
use iris_core::library::Kind;
use iris_core::settings::{Side, VideoWheel};

use super::AppModel;
use super::context_menu::{ContextMenu, ContextTarget};
use super::views::{ViewportView, model_listener};
use crate::platform;
use crate::theme::{FLOAT_GAP, FONT_MONO, ThemeTokens};
use crate::video::VideoSurface;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct VideoRect(pub(super) [f64; 4]);

/// Window-owned native surface, pixel bounds probe, and pointer gesture origins.
///
/// Playback session state and `load_generation` belong exclusively to [`iris_app::PlaybackController`],
/// and native teardown order (`VideoSurface` dropped BEFORE `Session::shutdown()`) is enforced by
/// [`AppModel::shutdown`].
pub(in crate::ui) struct Viewport {
    pub(super) video: Rc<RefCell<Option<VideoSurface>>>,
    pub(super) rect: Rc<Cell<Option<VideoRect>>>,
    pub(super) image: ImageView,
    pub(super) drag_origin: Option<Point<Pixels>>,
    pub(super) pending_win_drag_origin: Option<Point<Pixels>>,
    pub(super) pending_click_origin: Option<Point<Pixels>>,
}

impl Viewport {
    pub(super) fn new(video: Option<VideoSurface>) -> Self {
        Self {
            video: Rc::new(RefCell::new(video)),
            rect: Rc::new(Cell::new(None)),
            image: ImageView::default(),
            drag_origin: None,
            pending_win_drag_origin: None,
            pending_click_origin: None,
        }
    }

    pub(super) fn take_video_surface(&self) -> Option<VideoSurface> {
        self.video.borrow_mut().take()
    }
}

impl AppModel {
    pub(super) fn image_geometry(&self) -> ([f64; 2], [f64; 2]) {
        let rect = self.active().rect.get().map(|r| r.0).unwrap_or([0.; 4]);
        let (w, h) = self.session().display_size();
        ([rect[2], rect[3]], [w as f64, h as f64])
    }

    pub(super) fn image_geometry_ready(&self) -> bool {
        let (viewport, media) = self.image_geometry();
        viewport[0] > 0. && viewport[1] > 0. && media[0] > 0. && media[1] > 0.
    }

    pub(super) fn image_can_pan(&self) -> bool {
        let (viewport, media) = self.image_geometry();
        if !self.image_geometry_ready() {
            return false;
        }
        let [width, height] = self.active().image.displayed(viewport, media);
        width > viewport[0] + 0.5 || height > viewport[1] + 0.5
    }

    pub(super) fn apply_image_view(&mut self, cx: &mut Context<Self>) {
        let (zoom, pan) = (self.active().image.zoom, self.active().image.pan);
        if let Err(e) = self.session_mut().transform(zoom, pan) {
            log::warn!("image transform failed: {e:#}");
            self.show_error_toast("画面缩放或平移失败", cx);
        }
        self.notify_viewport_and_dock(cx);
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
            media: self.session().kind(),
            zoomed: self.image_can_pan(),
            overlay: self.has_blocking_overlay() || self.overlays.dir_popup.is_open(),
            video_wheel_volume: self.settings.video_wheel == VideoWheel::Volume,
            single_click_pause: self.settings.single_click_pause,
            photo_double_click_actual: self.settings.photo_double_click_actual,
        }
    }
}

impl ViewportView {
    pub(in crate::ui) fn render_viewport_entity(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        let m = self.model.read(cx);
        let vp = m.active();
        let last_rect = vp.rect.clone();
        let video = vp.video.clone();
        let model_weak = self.model.downgrade();
        let on_move = model_listener(&self.model, AppModel::on_root_mouse_move);
        let on_up = model_listener(&self.model, AppModel::on_root_mouse_up);
        let on_exit = model_listener(&self.model, |this, _: &gpui::MouseExitEvent, window, cx| {
            this.on_window_pointer_exit(window, cx);
        });
        let probe = canvas(
            move |bounds, window, _| {
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
                let weak = model_weak.clone();
                window.on_next_frame(move |w, cx| {
                    let _ = weak.update(cx, |this, cx| this.sync_window_and_viewport(w, cx));
                });
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

        let session = m.session();
        let kind = session.kind();
        let fullscreen = m.chrome.fullscreen;
        let chrome_visible = m.chrome.chrome_visible;
        let chrome_shown = m.chrome_shown();
        let floating_dock = fullscreen || m.effective_floating_dock();
        let dock_h = f32::from(t.dock_h);
        let has_media = session.current().is_some() || session.current_uri().is_some();
        let has_visual_surface =
            has_media && session.state().loaded && matches!(kind, Some(Kind::Video | Kind::Photo));
        let is_photo_zoomed = kind == Some(Kind::Photo) && m.image_can_pan();
        let panning = vp.drag_origin.is_some();
        let show_empty =
            !has_media && m.playback.pending_path().is_none() && !m.playback.loading_visible();
        let show_filmstrip = kind == Some(Kind::Photo)
            && m.settings.photo_filmstrip
            && (!fullscreen || chrome_visible)
            && session
                .current()
                .is_some_and(|cur| m.catalog.photo_paths.iter().any(|p| p == cur));
        let dir_side = m.settings.directory_side;
        let info_open = m.overlays.info_open;
        let dir_popup_open = m.overlays.dir_popup.is_open();
        let focus = m.focus.clone();

        let mut kc = KeyContext::default();
        if m.has_blocking_overlay() || dir_popup_open || m.text_inputs.is_any_focused(window) {
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

        let fs_chip_data = (fullscreen && chrome_visible)
            .then(|| {
                let name = session
                    .current()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .or_else(|| session.current_title().map(str::to_string))?;
                Some((name, m.playback.media_info()))
            })
            .flatten();
        let dock_bounds_slot = m.chrome.probes.dock_bounds.clone();
        let audio_card =
            (kind == Some(Kind::Audio)).then(|| super::viewport_cards::render_audio_card(m, t));

        div()
            .id("video")
            .relative()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .overflow_hidden()
            .when(!has_visual_surface, |d| d.bg(t.window_bg))
            .when(chrome_shown && is_photo_zoomed && panning, |d| {
                d.cursor(CursorStyle::ClosedHand)
            })
            .when(chrome_shown && is_photo_zoomed && !panning, |d| {
                d.cursor(CursorStyle::OpenHand)
            })
            .child(probe)
            .key_context(kc)
            .track_focus(&focus)
            .on_scroll_wheel(model_listener(
                &self.model,
                |this, ev: &gpui::ScrollWheelEvent, _, cx| {
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
                },
            ))
            .on_mouse_down(
                MouseButton::Right,
                model_listener(&self.model, |this, ev: &MouseDownEvent, window, cx| {
                    this.overlays.open_context_menu(ContextMenu {
                        pos: ev.position,
                        target: ContextTarget::Viewport,
                    });
                    this.reconcile_input_focus(window, cx);
                    cx.stop_propagation();
                    this.notify_all(cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                model_listener(&self.model, |this, ev: &MouseDownEvent, window, cx| {
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
                        this.notify_viewport(cx);
                    } else if let Some(PointerCommand::TogglePlayback) =
                        input::resolve_pointer_input(PointerInput::SingleClick, ctx)
                    {
                        this.active_mut().pending_click_origin = Some(ev.position);
                    }
                }),
            )
            // Layer 0: media surface fallbacks (audio card / empty state)
            .children(audio_card)
            .when(show_empty, |d| {
                d.child(super::viewport_cards::render_empty_state(
                    &self.model,
                    t,
                    cx,
                ))
            })
            // Layer 1: viewport overlays & floating chrome (media-normalized coords)
            .children(super::plugin_ui::overlay::render_plugin_overlay(
                &self.model,
                cx,
            ))
            .children(super::plugin_runtime::overlay::render_timed_overlay(
                &self.model,
                window,
                cx,
            ))
            .when_some(fs_chip_data, |d, (name, info)| {
                let chip = div()
                    .absolute()
                    .top(px(FLOAT_GAP))
                    .max_w(px(520.))
                    .h(t.control_h_md)
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
                            .text_size(t.text_md)
                            .child(name),
                    )
                    .when_some(info, |d, info| {
                        d.child(
                            div()
                                .flex_none()
                                .font_family(FONT_MONO)
                                .text_size(t.text_sm)
                                .text_color(t.text_dim)
                                .child(info),
                        )
                    });
                d.child(if dir_side == Side::Left {
                    chip.right(px(FLOAT_GAP))
                } else {
                    chip.left(px(FLOAT_GAP))
                })
            })
            .when(info_open && has_media, |d| {
                d.child(super::info::render_info_overlay(
                    &self.model,
                    fullscreen,
                    window,
                    cx,
                ))
            })
            .when(show_filmstrip, |d| {
                d.children(super::viewport_cards::render_photo_filmstrip(
                    &self.model,
                    fullscreen,
                    t,
                    cx,
                ))
            })
            .when(fullscreen && dir_popup_open, |d| {
                let popup = div()
                    .absolute()
                    .top_0()
                    .bottom(px(dock_h))
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .child(self.floating_sidebar.clone());
                d.child(if dir_side == Side::Left {
                    popup.left_0()
                } else {
                    popup.right_0()
                })
            })
            .when(floating_dock && chrome_visible, |d| {
                let slot = dock_bounds_slot;
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
                        .child(self.floating_dock.clone()),
                )
            })
    }
}
