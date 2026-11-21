//! A viewport: one session and native video surface, plus the area that
//! measures where the surface goes and the overlays drawn over it.
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{Pixels, Point};

use crate::geometry::ImageView;
use crate::player::Session;
use crate::video::VideoSurface;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct VideoRect(pub [f64; 4]);

pub(super) struct Viewport {
    pub session: Session,
    pub video: Rc<RefCell<Option<VideoSurface>>>,
    pub rect: Rc<Cell<Option<VideoRect>>>,
    pub image: ImageView,
    pub drag_origin: Option<Point<Pixels>>,
}

impl Drop for Viewport {
    fn drop(&mut self) {
        drop(self.video.borrow_mut().take());
        self.session.shutdown();
    }
}

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    StatefulInteractiveElement, Styled, canvas, div, prelude::FluentBuilder, px,
};

use super::*;

impl IrisWindow {
    pub(super) fn render_video_area(
        &self,
        fullscreen: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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

        div()
            .id("video")
            .relative()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .overflow_hidden()
            .child(probe)
            .key_context({
                let mut context = gpui::KeyContext::default();
                context.add("Viewport");
                context.set(
                    "media",
                    match self.viewports[0].session.kind() {
                        Some(Kind::Photo) => "image",
                        Some(Kind::Video) => "video",
                        None => "none",
                    },
                );
                if self.viewports[0].image.zoomed() {
                    context.add("zoomed");
                }
                context
            })
            .track_focus(&self.focus)
            .on_scroll_wheel(cx.listener(|this, ev: &gpui::ScrollWheelEvent, _, cx| {
                let delta = f32::from(ev.delta.pixel_delta(px(20.)).y) as f64 / 240.;
                if let Some(PointerCommand::Zoom(delta)) =
                    input::resolve_pointer_input(PointerInput::Wheel(delta), this.input_context())
                {
                    this.zoom_at(delta, Some(ev.position), cx);
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    let input = if ev.click_count == 2 {
                        PointerInput::DoubleClick
                    } else {
                        PointerInput::BeginDrag
                    };
                    match input::resolve_pointer_input(input, this.input_context()) {
                        Some(PointerCommand::CloseOverlay) => this.close_popup(cx),
                        Some(PointerCommand::ToggleFullscreen) => window.toggle_fullscreen(),
                        Some(PointerCommand::BeginPan) => {
                            this.viewports[0].drag_origin = Some(ev.position)
                        }
                        _ => {}
                    }
                }),
            )
            // Fullscreen: file name and media info at the top, on the edge
            // away from the directory so the popup never covers it.
            .when(fullscreen && self.chrome_visible, |d| {
                let name = self.viewports[0]
                    .session
                    .current
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned());
                d.when_some(name, |d, name| {
                    let chip = div()
                        .absolute()
                        .top(px(FLOAT_GAP))
                        .max_w(px(480.))
                        .h(px(28.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .bg(overlay())
                        .border_1()
                        .border_color(line())
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
                                    .text_color(text_dim())
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
            // Toast: top centre, a small accent square before the message.
            .when_some(self.toast.clone(), |d, msg| {
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
                                .bg(overlay())
                                .border_1()
                                .border_color(line())
                                .child(div().flex_none().size(px(6.)).bg(accent()))
                                .child(div().text_size(px(12.)).child(msg)),
                        ),
                )
            })
            .when(
                self.viewports[0].session.current.is_none() && self.pending_path.is_none(),
                |d| {
                    d.child(
                        div()
                            .absolute()
                            .size_full()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap(px(16.))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(text_dim())
                                    .child("拖入文件或文件夹"),
                            )
                            .child(
                                div()
                                    .id("open-file")
                                    .h(px(BUTTON))
                                    .px(px(12.))
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .border_1()
                                    .border_color(line_strong())
                                    .text_size(px(12.))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(hover()))
                                    .active(|s| s.bg(selected()))
                                    .child("打开…")
                                    .child(
                                        div()
                                            .font_family(FONT_MONO)
                                            .text_size(px(11.))
                                            .text_color(text_dim())
                                            .child(if cfg!(target_os = "macos") {
                                                "⌘O"
                                            } else {
                                                "Ctrl+O"
                                            }),
                                    )
                                    // Not a click on the picture.
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.open_file(&OpenFile, w, cx)
                                    })),
                            ),
                    )
                },
            )
            // In fullscreen the control bar floats over the video and hides
            // with the rest of the chrome. A video's timeline replaces the
            // frame's top edge.
            .when(fullscreen && self.chrome_visible, |d| {
                let slot = self.dock_bounds.clone();
                let seam = self.viewports[0].session.kind() == Some(Kind::Video);
                d.child(
                    div()
                        .absolute()
                        .bottom(px(FLOAT_GAP))
                        .left(px(FLOAT_GAP))
                        .right(px(FLOAT_GAP))
                        .h(px(DOCK_H))
                        .bg(overlay())
                        .border_color(line())
                        .border_l_1()
                        .border_r_1()
                        .border_b_1()
                        .when(!seam, |d| d.border_t_1())
                        // Clicks here are not clicks on the picture.
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
            // The directory popup, on the chosen edge, between the top of the
            // screen and the floating bar.
            .when(fullscreen && self.dir_popup, |d| {
                let popup = div()
                    .absolute()
                    .top(px(FLOAT_GAP))
                    .bottom(px(DOCK_H + FLOAT_GAP + 8.))
                    .border_1()
                    .border_color(line())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .child(self.render_directory(true, cx));
                d.child(if self.settings.directory_side == Side::Left {
                    popup.left(px(FLOAT_GAP))
                } else {
                    popup.right(px(FLOAT_GAP))
                })
            })
    }
}
