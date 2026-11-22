//! Upward-floating menus anchored directly above their trigger buttons on the
//! control bar: subtitles (with delay adjustment), audio tracks, speed presets,
//! and the "More" video/chapter/transform menu.
use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};

use super::widgets::icon;
use super::*;
use crate::assets::Icon;
use crate::player::{AspectMode, SPEED_STEPS, TrackKind, format_time};

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
        let pop_w = match kind {
            PopoverKind::Speed => 172.,
            PopoverKind::More => 260.,
            PopoverKind::Subtitles | PopoverKind::Audio => 272.,
        };
        // Position directly above the trigger button inside the viewport.
        let vp_rect = self.active().rect.get().map(|r| r.0);
        let anchor = self.popover_anchor(kind);
        let left_px = match (vp_rect, anchor) {
            (Some([vx, _, vw, _]), Some(ab)) if vw > 0. => {
                let btn_cx = f32::from(ab.origin.x) + f32::from(ab.size.width) * 0.5 - vx as f32;
                let max_l = (vw as f32 - pop_w - 8.).max(8.);
                Some((btn_cx - pop_w * 0.5).clamp(8., max_l))
            }
            _ => None,
        };

        let content = match kind {
            PopoverKind::Subtitles => self.render_sub_popover(cx).into_any_element(),
            PopoverKind::Audio => self.render_audio_popover(cx).into_any_element(),
            PopoverKind::Speed => self.render_speed_popover(cx).into_any_element(),
            PopoverKind::More => self.render_more_popover(cx).into_any_element(),
        };

        let base = div()
            .id("popover")
            .absolute()
            .bottom(px(bottom))
            .w(px(pop_w))
            .max_h(px(360.))
            .flex()
            .flex_col()
            .bg(panel())
            .border_1()
            .border_color(line_strong())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(content);

        if let Some(left) = left_px {
            base.left(px(left))
        } else {
            base.right(px(if fullscreen { FLOAT_GAP + 12. } else { 12. }))
        }
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
            .child(div().text_size(px(11.)).text_color(text_dim()).child(title))
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
        let session = &self.active().session;
        let visible = session.state.subs_visible;
        let delay = session.state.sub_delay;
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
                .child(
                    div()
                        .w(px(14.))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .when(active, |d| d.child(icon(Icon::Check, 12., accent()))),
                )
                .child(div().flex_1().min_w(px(0.)).truncate().child(label.clone()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.command(Command::SelectSubtitle(Some(id)), cx);
                    this.overlays.popover = None;
                    this.show_osd(format!("字幕 · {label}"), cx);
                }))
        });

        let has_tracks = !session.tracks_of(TrackKind::Sub).is_empty();

        div()
            .w_full()
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
                    .child(
                        div()
                            .w(px(14.))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .when(!visible, |d| d.child(icon(Icon::Check, 12., accent()))),
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
                // Subtitle timing delay fine adjustment (`Z` / `X`)
                div()
                    .h(px(32.))
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
                            .child("时间偏移 (Z/X)"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .border_1()
                            .border_color(line_strong())
                            .bg(window_bg())
                            .child(
                                div()
                                    .id("sub-delay-dec")
                                    .size(px(22.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .border_r_1()
                                    .border_color(line())
                                    .hover(|s| s.bg(hover()))
                                    .child(icon(Icon::Minus, 11., text_dim()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.command(Command::SubDelayBy(-0.1), cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("sub-delay-reset")
                                    .min_w(px(52.))
                                    .px(px(4.))
                                    .h(px(22.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .font_family(FONT_MONO)
                                    .text_size(px(10.))
                                    .text_color(if delay.abs() > 0.001 {
                                        text()
                                    } else {
                                        text_dim()
                                    })
                                    .hover(|s| s.bg(hover()))
                                    .child(format!("{delay:+.1}s"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.command(Command::SetSubDelay(0.), cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("sub-delay-inc")
                                    .size(px(22.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .border_l_1()
                                    .border_color(line())
                                    .hover(|s| s.bg(hover()))
                                    .child(icon(Icon::Plus, 11., text_dim()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.command(Command::SubDelayBy(0.1), cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
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
                                .text_color(text_faint())
                                .child("无内嵌字幕 · 拖入 .srt / .ass 可加载外挂字幕"),
                        )
                    }),
            )
    }

    fn render_audio_popover(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = &self.active().session;
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
                .child(
                    div()
                        .w(px(14.))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .when(active, |d| d.child(icon(Icon::Check, 12., accent()))),
                )
                .child(div().flex_1().min_w(px(0.)).truncate().child(label.clone()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.command(Command::SelectAudio(Some(id)), cx);
                    this.overlays.popover = None;
                    this.show_osd(format!("音轨 · {label}"), cx);
                }))
        });

        div()
            .w_full()
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
        let current = self.active().session.state.speed;
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
                    this.overlays.popover = None;
                    this.show_osd(format!("倍速 · {speed:.2}×"), cx);
                }))
        });

        div()
            .w_full()
            .flex()
            .flex_col()
            .child(Self::popover_header("播放速度", Some("[ ]")))
            .child(div().py(px(4.)).flex().flex_col().children(rows))
    }

    fn render_more_popover(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = &self.active().session;
        let state = &session.state;
        let aspect = state.aspect;
        let chapters = state.chapters.clone();
        let current_ch = state.current_chapter;
        let is_video = session.kind() == Some(Kind::Video);
        let shot_key = if cfg!(target_os = "macos") {
            "⌘S"
        } else {
            "Ctrl+S"
        };

        let aspect_modes = [
            AspectMode::Original,
            AspectMode::Ratio16x9,
            AspectMode::Ratio4x3,
            AspectMode::Ratio21x9,
            AspectMode::FillCrop,
        ];

        type ClickHandler = Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App)>;

        let row = |id: &'static str,
                   label: &'static str,
                   key: &'static str,
                   active: bool,
                   on_click: ClickHandler| {
            div()
                .id(id)
                .h(px(26.))
                .px(px(10.))
                .flex()
                .items_center()
                .justify_between()
                .cursor_pointer()
                .text_size(px(12.))
                .when(active, |d| d.bg(selected()).text_color(text()))
                .when(!active, |d| {
                    d.text_color(text_muted())
                        .hover(|s| s.bg(hover()).text_color(text()))
                })
                .child(label)
                .child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(10.))
                        .text_color(text_faint())
                        .child(key),
                )
                .on_click(move |ev, w, cx| on_click(ev, w, cx))
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .child(Self::popover_header("画面与工具", None))
            .when(is_video, |d| {
                d.child(
                    div()
                        .px(px(10.))
                        .py(px(8.))
                        .border_b_1()
                        .border_color(line())
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_size(px(10.))
                                .text_color(text_dim())
                                .child(icon(Icon::Aspect, 12., text_faint()))
                                .child("画面比例 (C)"),
                        )
                        .child(div().flex().flex_wrap().gap(px(4.)).children(
                            aspect_modes.into_iter().enumerate().map(|(i, m)| {
                                let on = aspect == m;
                                div()
                                    .id(("aspect-opt", i))
                                    .h(px(22.))
                                    .px(px(7.))
                                    .flex()
                                    .items_center()
                                    .cursor_pointer()
                                    .border_1()
                                    .border_color(if on { accent() } else { line_strong() })
                                    .when(on, |d| d.bg(selected()).text_color(text()))
                                    .when(!on, |d| {
                                        d.bg(window_bg())
                                            .text_color(text_dim())
                                            .hover(|s| s.bg(hover()).text_color(text()))
                                    })
                                    .text_size(px(11.))
                                    .child(m.label())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.command(Command::SetAspect(m), cx);
                                        this.show_osd(format!("画面比例 · {}", m.label()), cx);
                                    }))
                            }),
                        )),
                )
            })
            .child(
                div()
                    .py(px(4.))
                    .flex()
                    .flex_col()
                    .child(row(
                        "more-rot",
                        "顺时针旋转 90°",
                        "R",
                        false,
                        Box::new(cx.listener(|this, _, w, cx| {
                            this.rotate_cw(&RotateCw, w, cx);
                        })),
                    ))
                    .child(row(
                        "more-hflip",
                        "水平镜像翻转",
                        "H",
                        state.hflip,
                        Box::new(cx.listener(|this, _, w, cx| {
                            this.toggle_hflip(&ToggleHFlip, w, cx);
                        })),
                    ))
                    .child(row(
                        "more-vflip",
                        "垂直镜像翻转",
                        "Shift+H",
                        state.vflip,
                        Box::new(cx.listener(|this, _, w, cx| {
                            this.toggle_vflip(&ToggleVFlip, w, cx);
                        })),
                    ))
                    .when(is_video, |d| {
                        d.child(row(
                            "more-shot",
                            "保存当前帧截图",
                            shot_key,
                            false,
                            Box::new(cx.listener(|this, _, w, cx| {
                                this.overlays.popover = None;
                                this.take_screenshot(&TakeScreenshot, w, cx);
                            })),
                        ))
                    })
                    .child(row(
                        "more-info",
                        "媒体详情与统计",
                        "I",
                        self.overlays.info_open,
                        Box::new(cx.listener(|this, _, w, cx| {
                            this.overlays.popover = None;
                            this.toggle_info(&ToggleInfo, w, cx);
                        })),
                    )),
            )
            .when(!chapters.is_empty(), |d| {
                d.child(
                    div()
                        .border_t_1()
                        .border_color(line())
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .px(px(10.))
                                .pt(px(6.))
                                .pb(px(4.))
                                .text_size(px(10.))
                                .text_color(text_dim())
                                .child("章节"),
                        )
                        .child(
                            div()
                                .id("chapter-list")
                                .max_h(px(120.))
                                .overflow_y_scroll()
                                .pb(px(4.))
                                .flex()
                                .flex_col()
                                .children(chapters.into_iter().enumerate().map(|(i, ch)| {
                                    let active = current_ch == i as i64;
                                    let title = ch.title.clone();
                                    div()
                                        .id(("ch-row", i))
                                        .h(px(24.))
                                        .px(px(10.))
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .gap(px(8.))
                                        .cursor_pointer()
                                        .text_size(px(11.))
                                        .when(active, |d| d.bg(selected()).text_color(text()))
                                        .when(!active, |d| {
                                            d.text_color(text_muted())
                                                .hover(|s| s.bg(hover()).text_color(text()))
                                        })
                                        .child(div().flex_1().min_w(px(0.)).truncate().child(title))
                                        .child(
                                            div()
                                                .flex_none()
                                                .font_family(FONT_MONO)
                                                .text_size(px(10.))
                                                .text_color(text_faint())
                                                .child(format_time(ch.time)),
                                        )
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.command(Command::SelectChapter(i), cx);
                                            this.overlays.popover = None;
                                            cx.notify();
                                        }))
                                })),
                        ),
                )
            })
    }
}
