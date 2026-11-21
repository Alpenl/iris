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
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Styled,
    canvas, div, prelude::FluentBuilder, px,
};

use super::*;

impl IrisWindow {
    pub(super) fn render_video_area(
        &self,
        fullscreen: bool,
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

        let info = {
            let (w, h) = self.viewports[0].session.state.video_size;
            let mut parts = Vec::new();
            if w > 0 {
                parts.push(format!("{w}×{h}"));
            }
            parts.push(
                if self.viewports[0].session.state.hwdec.is_empty()
                    || self.viewports[0].session.state.hwdec == "no"
                {
                    "软解".into()
                } else {
                    format!("硬解 {}", self.viewports[0].session.state.hwdec)
                },
            );
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
}
