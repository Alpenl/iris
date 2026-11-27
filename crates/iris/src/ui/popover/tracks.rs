//! Subtitle track & delay popover (`kit::form::stepper`) and audio track /
//! output device selection popover, unified on `kit::list_row::select_row`.

use gpui::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, prelude::FluentBuilder, px,
};
use iris_core::player::{Command, TrackKind};

use super::super::AppModel;
use super::super::kit::form::stepper;
use super::super::kit::list_row::select_row;
use super::super::views::{DirtyRegions, model_listener};
use super::popover_header;
use crate::theme::ThemeTokens;

pub(super) fn render_sub_popover(
    model: &Entity<AppModel>,
    t: ThemeTokens,
    cx: &App,
) -> impl IntoElement {
    let m = model.read(cx);
    let session = m.session();
    let state = session.state();
    let visible = state.subs_visible;
    let delay = state.sub_delay;
    let tracks = session.tracks_of(TrackKind::Sub);
    let has_tracks = !tracks.is_empty();
    let sub_hint = m.key_hint("cycle_sub");

    let rows = tracks.into_iter().map(|tr| {
        let active = visible && tr.selected;
        let id = tr.id;
        let label = super::super::strings::chrome::track_label(tr);
        select_row(
            ("sub-track", id as usize),
            label.clone(),
            None,
            active,
            false,
            false,
            t,
        )
        .on_click(model_listener(model, move |this, _, _, cx| {
            this.command(Command::SelectSubtitle(Some(id)), cx);
            this.overlays.popover = None;
            this.show_osd(format!("字幕 · {label}"), cx);
            this.notify_dock(cx);
        }))
    });

    div()
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .child(popover_header("字幕轨道", sub_hint, t))
        .child(
            select_row(
                "sub-toggle-row",
                if visible {
                    "隐藏字幕"
                } else {
                    "字幕已关闭"
                },
                None,
                !visible,
                false,
                false,
                t,
            )
            .border_b_1()
            .border_color(t.line)
            .on_click(model_listener(model, |this, _, _, cx| {
                this.command(Command::ToggleSubtitles, cx);
                this.overlays.popover = None;
                this.notify_regions(
                    DirtyRegions {
                        root: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
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
                        .text_size(t.text_sm)
                        .text_color(t.text_dim)
                        .child("时间偏移"),
                )
                .child(stepper(
                    "sub-delay",
                    format!("{delay:+.1}s"),
                    delay.abs() > 0.001,
                    t,
                    model_listener(model, |this, _, _, cx| {
                        this.command(Command::SubDelayBy(-0.1), cx);
                        this.notify_root(cx);
                    }),
                    model_listener(model, |this, _, _, cx| {
                        this.command(Command::SubDelayBy(0.1), cx);
                        this.notify_root(cx);
                    }),
                    model_listener(model, |this, _, _, cx| {
                        this.command(Command::SetSubDelay(0.), cx);
                        this.notify_root(cx);
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
                            .text_size(t.text_sm)
                            .text_color(t.text_faint)
                            .child("无内嵌字幕 · 拖入 .srt / .ass 可加载外挂字幕"),
                    )
                }),
        )
}

pub(super) fn render_audio_popover(
    model: &Entity<AppModel>,
    t: ThemeTokens,
    cx: &App,
) -> impl IntoElement {
    let m = model.read(cx);
    let session = m.session();
    let tracks = session.tracks_of(TrackKind::Audio);
    let has_tracks = !tracks.is_empty();
    let audio_hint = m.key_hint("cycle_audio");

    let track_rows = tracks.into_iter().map(|tr| {
        let active = tr.selected;
        let id = tr.id;
        let label = super::super::strings::chrome::track_label(tr);
        select_row(
            ("audio-track", id as usize),
            label.clone(),
            None,
            active,
            false,
            false,
            t,
        )
        .on_click(model_listener(model, move |this, _, _, cx| {
            this.command(Command::SelectAudio(Some(id)), cx);
            this.overlays.popover = None;
            this.show_osd(format!("音轨 · {label}"), cx);
            this.notify_dock(cx);
        }))
    });

    let cur_dev = if m.settings.audio_device.trim().is_empty() {
        "auto"
    } else {
        m.settings.audio_device.trim()
    };
    let device_rows = session
        .audio_devices()
        .into_iter()
        .enumerate()
        .map(|(i, dev)| {
            let active = dev.name == cur_dev;
            let dev_name = dev.name.clone();
            let dev_label = super::super::strings::chrome::audio_device_label(&dev, false);
            let osd_label = dev_label.clone();
            select_row(("audio-dev", i), dev_label, None, active, false, false, t)
                .text_size(t.text_sm)
                .on_click(model_listener(model, move |this, _, _, cx| {
                    this.settings.audio_device = dev_name.clone();
                    this.persist_user_settings();
                    this.command(Command::SetAudioDevice(dev_name.clone()), cx);
                    this.overlays.popover = None;
                    this.show_osd(format!("音频输出 · {osd_label}"), cx);
                    this.notify_regions(
                        DirtyRegions {
                            root: true,
                            settings: true,
                            dock: true,
                            ..DirtyRegions::NONE
                        },
                        cx,
                    );
                }))
        });

    div()
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .child(popover_header("音轨", audio_hint, t))
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
                                    .text_size(t.text_sm)
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
                                .min_h(t.control_h_sm)
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .text_size(t.text_xs)
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
