//! Upward-floating menus anchored above the control bar for subtitles, audio
//! tracks, and playback speed presets.
use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};

use super::widgets::icon;
use super::*;
use crate::assets::Icon;
use crate::player::{SPEED_STEPS, TrackKind};

impl IrisWindow {
    pub(super) fn render_popover(
        &self,
        kind: PopoverKind,
        fullscreen: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let bottom = if fullscreen {
            DOCK_H + FLOAT_GAP + 8.
        } else {
            8.
        };
        let right = if fullscreen { FLOAT_GAP + 12. } else { 12. };

        let content = match kind {
            PopoverKind::Subtitles => self.render_sub_popover(cx).into_any_element(),
            PopoverKind::Audio => self.render_audio_popover(cx).into_any_element(),
            PopoverKind::Speed => self.render_speed_popover(cx).into_any_element(),
        };

        div()
            .id("popover")
            .absolute()
            .bottom(px(bottom))
            .right(px(right))
            .when(kind == PopoverKind::Speed, |d| d.w(px(172.)))
            .when(kind != PopoverKind::Speed, |d| {
                d.min_w(px(220.)).max_w(px(340.))
            })
            .max_h(px(320.))
            .flex()
            .flex_col()
            .bg(panel())
            .border_1()
            .border_color(line_strong())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(content)
    }

    fn popover_header(title: &'static str, hint: Option<&'static str>) -> impl IntoElement {
        div()
            .h(px(30.))
            .flex_none()
            .px(px(10.))
            .flex()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(line())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(text_dim())
                    .child(title),
            )
            .when_some(hint, |d, hint| {
                d.child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(10.))
                        .text_color(text_faint())
                        .child(hint),
                )
            })
    }

    fn render_sub_popover(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = &self.viewports[0].session;
        let visible = session.state.subs_visible;
        let tracks = session.tracks_of(TrackKind::Sub);

        let rows = tracks.into_iter().map(|t| {
            let active = visible && t.selected;
            let id = t.id;
            let label = t.label();
            div()
                .id(("sub-track", id as usize))
                .h(px(ROW_H))
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .text_size(px(12.))
                .when(active, |d| d.bg(selected()).text_color(text()))
                .when(!active, |d| {
                    d.text_color(text_muted())
                        .hover(|s| s.bg(hover()).text_color(text()))
                })
                .child(div().w(px(14.)).flex_none().flex().justify_center().when(
                    active,
                    |d| d.child(icon(Icon::Check, 12., accent())),
                ))
                .child(div().flex_1().min_w(px(0.)).truncate().child(label.clone()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.command(Command::SelectSubtitle(Some(id)), cx);
                    this.popover = None;
                    this.show_toast(format!("字幕 · {label}"), cx);
                }))
        });

        let has_tracks = !session.tracks_of(TrackKind::Sub).is_empty();

        div()
            .flex()
            .flex_col()
            .child(Self::popover_header("字幕轨道", Some("J 切换 · V 显隐")))
            .child(
                div()
                    .id("sub-toggle-row")
                    .h(px(ROW_H))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .border_b_1()
                    .border_color(line())
                    .when(!visible, |d| d.bg(selected()).text_color(text()))
                    .when(visible, |d| {
                        d.text_color(text_muted())
                            .hover(|s| s.bg(hover()).text_color(text()))
                    })
                    .child(div().w(px(14.)).flex_none().flex().justify_center().when(
                        !visible,
                        |d| d.child(icon(Icon::Check, 12., accent())),
                    ))
                    .child(if visible { "隐藏字幕" } else { "字幕已关闭" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.command(Command::ToggleSubtitles, cx);
                        this.popover = None;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("sub-list")
                    .flex_1()
                    .min_h(px(0.))
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .py(px(4.))
                    .flex()
                    .flex_col()
                    .children(rows)
                    .when(!has_tracks, |d| {
                        d.child(
                            div()
                                .px(px(10.))
                                .py(px(8.))
                                .text_size(px(11.))
                                .text_color(text_faint())
                                .child("无内嵌字幕 · 拖入 .srt / .ass 可加载外挂字幕"),
                        )
                    }),
            )
    }

    fn render_audio_popover(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = &self.viewports[0].session;
        let tracks = session.tracks_of(TrackKind::Audio);
        let has_tracks = !tracks.is_empty();

        let rows = tracks.into_iter().map(|t| {
            let active = t.selected;
            let id = t.id;
            let label = t.label();
            div()
                .id(("audio-track", id as usize))
                .h(px(ROW_H))
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .text_size(px(12.))
                .when(active, |d| d.bg(selected()).text_color(text()))
                .when(!active, |d| {
                    d.text_color(text_muted())
                        .hover(|s| s.bg(hover()).text_color(text()))
                })
                .child(div().w(px(14.)).flex_none().flex().justify_center().when(
                    active,
                    |d| d.child(icon(Icon::Check, 12., accent())),
                ))
                .child(div().flex_1().min_w(px(0.)).truncate().child(label.clone()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.command(Command::SelectAudio(Some(id)), cx);
                    this.popover = None;
                    this.show_toast(format!("音轨 · {label}"), cx);
                }))
        });

        div()
            .flex()
            .flex_col()
            .child(Self::popover_header("音轨", Some("A 切换")))
            .child(
                div()
                    .id("audio-list")
                    .flex_1()
                    .min_h(px(0.))
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .py(px(4.))
                    .flex()
                    .flex_col()
                    .children(rows)
                    .when(!has_tracks, |d| {
                        d.child(
                            div()
                                .px(px(10.))
                                .py(px(8.))
                                .text_size(px(11.))
                                .text_color(text_faint())
                                .child("当前媒体没有音轨"),
                        )
                    }),
            )
    }

    fn render_speed_popover(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.viewports[0].session.state.speed;
        let rows = SPEED_STEPS.iter().copied().enumerate().map(|(i, speed)| {
            let active = (current - speed).abs() < 0.02;
            div()
                .id(("speed-opt", i))
                .h(px(26.))
                .px(px(10.))
                .flex()
                .items_center()
                .justify_between()
                .cursor_pointer()
                .font_family(FONT_MONO)
                .text_size(px(12.))
                .when(active, |d| d.bg(selected()).text_color(text()))
                .when(!active, |d| {
                    d.text_color(text_muted())
                        .hover(|s| s.bg(hover()).text_color(text()))
                })
                .child(format!("{speed:.2}×"))
                .when(active, |d| d.child(icon(Icon::Check, 12., accent())))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.command(Command::SetSpeed(speed), cx);
                    this.popover = None;
                    this.show_toast(format!("倍速 · {speed:.2}×"), cx);
                }))
        });

        div()
            .w_full()
            .flex()
            .flex_col()
            .child(Self::popover_header("播放速度", Some("[ ]")))
            .child(div().py(px(4.)).flex().flex_col().children(rows))
    }
}
