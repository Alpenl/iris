//! Subtitle track & delay popover (`kit::form::stepper`) and audio track /
//! output device selection popover.

use gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    div, prelude::FluentBuilder, px,
};
use iris_core::player::{Command, TrackKind};

use super::super::IrisWindow;
use super::super::kit::form::stepper;
use super::super::kit::icon;
use super::popover_header;
use crate::assets::Icon;
use crate::theme::ThemeTokens;

impl IrisWindow {
    pub(super) fn render_sub_popover(
        &self,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let session = &self.active().session;
        let state = session.state();
        let visible = state.subs_visible;
        let delay = state.sub_delay;
        let tracks = session.tracks_of(TrackKind::Sub);
        let has_tracks = !tracks.is_empty();

        let rows = tracks.into_iter().map(|tr| {
            let active = visible && tr.selected;
            let id = tr.id;
            let label = tr.label();
            div()
                .id(("sub-track", id as usize))
                .h(t.row_h)
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .text_size(px(12.))
                .when(active, |d| d.bg(t.selected).text_color(t.text))
                .when(!active, |d| {
                    d.text_color(t.text_muted)
                        .hover(|s| s.bg(t.hover).text_color(t.text))
                })
                .child(
                    div()
                        .w(px(14.))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .when(active, |d| d.child(icon(Icon::Check, 12., t.accent))),
                )
                .child(div().flex_1().min_w(px(0.)).truncate().child(label.clone()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.command(Command::SelectSubtitle(Some(id)), cx);
                    this.overlays.popover = None;
                    this.show_osd(format!("字幕 · {label}"), cx);
                }))
        });

        div()
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(popover_header("字幕轨道", self.key_hint("cycle_sub"), t))
            .child(
                div()
                    .id("sub-toggle-row")
                    .h(t.row_h)
                    .flex_none()
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .border_b_1()
                    .border_color(t.line)
                    .when(!visible, |d| d.bg(t.selected).text_color(t.text))
                    .when(visible, |d| {
                        d.text_color(t.text_muted)
                            .hover(|s| s.bg(t.hover).text_color(t.text))
                    })
                    .child(
                        div()
                            .w(px(14.))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .when(!visible, |d| d.child(icon(Icon::Check, 12., t.accent))),
                    )
                    .child(if visible {
                        "隐藏字幕"
                    } else {
                        "字幕已关闭"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.command(Command::ToggleSubtitles, cx);
                        this.overlays.popover = None;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .py(px(6.))
                    .flex_none()
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(t.line)
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(t.text_dim)
                            .child("时间偏移"),
                    )
                    .child(stepper(
                        "sub-delay",
                        format!("{delay:+.1}s"),
                        delay.abs() > 0.001,
                        t,
                        cx.listener(|this, _, _, cx| {
                            this.command(Command::SubDelayBy(-0.1), cx);
                            cx.notify();
                        }),
                        cx.listener(|this, _, _, cx| {
                            this.command(Command::SubDelayBy(0.1), cx);
                            cx.notify();
                        }),
                        cx.listener(|this, _, _, cx| {
                            this.command(Command::SetSubDelay(0.), cx);
                            cx.notify();
                        }),
                    )),
            )
            .child(
                div()
                    .id("sub-list")
                    .flex_1()
                    .min_h(px(0.))
                    .max_h(px(220.))
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
                                .text_color(t.text_faint)
                                .child("无内嵌字幕 · 拖入 .srt / .ass 可加载外挂字幕"),
                        )
                    }),
            )
    }

    pub(super) fn render_audio_popover(
        &self,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let session = &self.active().session;
        let tracks = session.tracks_of(TrackKind::Audio);
        let has_tracks = !tracks.is_empty();

        let track_rows = tracks.into_iter().map(|tr| {
            let active = tr.selected;
            let id = tr.id;
            let label = tr.label();
            div()
                .id(("audio-track", id as usize))
                .h(t.row_h)
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .text_size(px(12.))
                .when(active, |d| d.bg(t.selected).text_color(t.text))
                .when(!active, |d| {
                    d.text_color(t.text_muted)
                        .hover(|s| s.bg(t.hover).text_color(t.text))
                })
                .child(
                    div()
                        .w(px(14.))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .when(active, |d| d.child(icon(Icon::Check, 12., t.accent))),
                )
                .child(div().flex_1().min_w(px(0.)).truncate().child(label.clone()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.command(Command::SelectAudio(Some(id)), cx);
                    this.overlays.popover = None;
                    this.show_osd(format!("音轨 · {label}"), cx);
                }))
        });

        let cur_dev = if self.settings.audio_device.trim().is_empty() {
            "auto"
        } else {
            self.settings.audio_device.trim()
        };
        let device_rows = session
            .audio_devices()
            .into_iter()
            .enumerate()
            .map(|(i, dev)| {
                let active = dev.name == cur_dev;
                let dev_name = dev.name.clone();
                let dev_label = if dev.name == "auto" {
                    "自动选择设备".to_string()
                } else {
                    dev.description.clone()
                };
                let osd_label = dev_label.clone();
                div()
                    .id(("audio-dev", i))
                    .h(px(26.))
                    .flex_none()
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .when(active, |d| d.bg(t.selected).text_color(t.text))
                    .when(!active, |d| {
                        d.text_color(t.text_muted)
                            .hover(|s| s.bg(t.hover).text_color(t.text))
                    })
                    .child(
                        div()
                            .w(px(14.))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .when(active, |d| d.child(icon(Icon::Check, 12., t.accent))),
                    )
                    .child(div().flex_1().min_w(px(0.)).truncate().child(dev_label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.audio_device = dev_name.clone();
                        this.persist_user_settings();
                        this.command(Command::SetAudioDevice(dev_name.clone()), cx);
                        this.overlays.popover = None;
                        this.show_osd(format!("音频输出 · {osd_label}"), cx);
                    }))
            });

        div()
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(popover_header("音轨", self.key_hint("cycle_audio"), t))
            .child(
                div()
                    .id("audio-popover-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("audio-list")
                            .max_h(px(160.))
                            .overflow_y_scroll()
                            .py(px(4.))
                            .flex()
                            .flex_col()
                            .children(track_rows)
                            .when(!has_tracks, |d| {
                                d.child(
                                    div()
                                        .px(px(10.))
                                        .py(px(8.))
                                        .text_size(px(11.))
                                        .text_color(t.text_faint)
                                        .child("当前媒体没有音轨"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .border_t_1()
                            .border_color(t.line)
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .h(px(26.))
                                    .px(px(10.))
                                    .flex()
                                    .items_center()
                                    .text_size(px(10.))
                                    .text_color(t.text_dim)
                                    .child("音频输出设备"),
                            )
                            .child(
                                div()
                                    .id("audio-dev-list")
                                    .max_h(px(196.))
                                    .overflow_y_scroll()
                                    .pb(px(4.))
                                    .flex()
                                    .flex_col()
                                    .children(device_rows),
                            ),
                    ),
            )
    }
}
