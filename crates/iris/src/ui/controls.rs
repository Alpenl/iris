//! The dock: progress bar, transport buttons and playback fields.
use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement, StatefulInteractiveElement, Styled, canvas, div, prelude::FluentBuilder, px,
    relative,
};

use super::directory::panel_icon;
use super::*;

impl IrisWindow {
    pub(super) fn render_dock_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pct = if self.viewports[0].session.state.duration > 0. {
            (self.viewports[0].session.state.time_pos / self.viewports[0].session.state.duration)
                .clamp(0., 1.) as f32
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
        let vol_segments = (self.viewports[0].session.state.volume / 10.)
            .round()
            .clamp(0., 10.) as usize;
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
                                    .child(
                                        if self.viewports[0].session.kind() == Some(Kind::Photo) {
                                            "1:1"
                                        } else if self.viewports[0].session.state.paused {
                                            "▶"
                                        } else {
                                            "❚❚"
                                        },
                                    )
                                    .border_color(accent())
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        if this.viewports[0].session.kind() == Some(Kind::Photo) {
                                            this.toggle_image_size(&ToggleImageSize, w, cx);
                                        } else {
                                            this.toggle_pause(&TogglePause, w, cx);
                                        }
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
                                    .child(
                                        if self.viewports[0].session.kind() == Some(Kind::Photo) {
                                            format!(
                                                "{:.0}%",
                                                self.viewports[0].image.zoom.exp2() * 100.
                                            )
                                        } else {
                                            fmt_time(self.viewports[0].session.state.time_pos)
                                        },
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(24.))
                            .child(
                                square("favorite")
                                    .child(
                                        if self.viewports[0]
                                            .session
                                            .current
                                            .as_deref()
                                            .is_some_and(|p| self.store.favorite(p))
                                        {
                                            "★"
                                        } else {
                                            "☆"
                                        },
                                    )
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.toggle_favorite(&ToggleFavorite, w, cx)
                                    })),
                            )
                            .child(field(
                                "字幕",
                                if self.viewports[0].session.state.subs_visible {
                                    "开".into()
                                } else {
                                    "关".into()
                                },
                            ))
                            .child(
                                div()
                                    .id("audio-track")
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.command(Command::NextAudio, cx)
                                    }))
                                    .child(field(
                                        "音轨",
                                        self.viewports[0].session.state.audio.clone(),
                                    )),
                            )
                            .child(field(
                                "速度",
                                format!("{:.2}×", self.viewports[0].session.state.speed),
                            ))
                            .when(self.viewports[0].session.kind() == Some(Kind::Photo), |d| {
                                d.child(
                                    div()
                                        .id("fit-image")
                                        .cursor_pointer()
                                        .child("适应窗口")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.fit_image(&FitImage, window, cx)
                                        })),
                                )
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
                                            .child(format!(
                                                "音量 {:.0}",
                                                self.viewports[0].session.state.volume
                                            )),
                                    )
                                    .child(div().flex().gap(px(2.)).children((0..10).map(|i| {
                                        div()
                                            .id(("vol", i))
                                            .w(px(5.))
                                            .h(px(12.))
                                            .bg(if i < vol_segments { accent() } else { line() })
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.command(
                                                    Command::SetVolume(((i + 1) * 10) as f64),
                                                    cx,
                                                );
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

pub(super) fn fmt_time(t: f64) -> String {
    let s = t.max(0.) as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}
