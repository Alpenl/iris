//! Floating menus anchored directly to their trigger buttons (`t.radius_lg`),
//! plus playback speed and stream quality variant popovers.

mod more_and_presets;
mod tracks;

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use iris_core::player::{Command, SPEED_STEPS};

use super::IrisWindow;
use super::kit::icon;
use super::state::PopoverKind;
use crate::assets::Icon;
use crate::theme::{FLOAT_GAP, FONT_MONO, ThemeTokens};

pub(super) fn popover_header(
    title: &'static str,
    hint: Option<SharedString>,
    t: ThemeTokens,
) -> impl IntoElement {
    div()
        .h(px(30.))
        .flex_none()
        .px(px(10.))
        .flex()
        .items_center()
        .justify_between()
        .border_b_1()
        .border_color(t.line)
        .child(div().text_size(px(11.)).text_color(t.text_dim).child(title))
        .when_some(hint, |d, hint| {
            d.child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(px(10.))
                    .text_color(t.text_faint)
                    .child(hint),
            )
        })
}

impl IrisWindow {
    pub(super) fn render_popover(
        &self,
        kind: PopoverKind,
        fullscreen: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        let floating_dock = fullscreen || self.effective_floating_dock();
        let is_top_anchored = kind == PopoverKind::Presets;

        let pop_w = match kind {
            PopoverKind::Speed => 172.,
            PopoverKind::Variants => 204.,
            PopoverKind::More => 276.,
            PopoverKind::Subtitles | PopoverKind::Audio => 284.,
            PopoverKind::Presets => 304.,
        };
        let max_h = match kind {
            PopoverKind::More | PopoverKind::Audio | PopoverKind::Presets => 440.,
            _ => 360.,
        };

        let vp_rect = self.active().rect.get().map(|r| r.0);
        let anchor = if is_top_anchored && fullscreen {
            None
        } else {
            self.popover_anchor(kind)
        };
        let left_px = match (vp_rect, anchor) {
            (Some([vx, _, vw, _]), Some(ab)) if vw > 0. => {
                let btn_cx = f32::from(ab.origin.x) + f32::from(ab.size.width) * 0.5 - vx as f32;
                let max_l = (vw as f32 - pop_w - 8.).max(8.);
                Some((btn_cx - pop_w * 0.5).clamp(8., max_l))
            }
            _ => None,
        };

        let content = match kind {
            PopoverKind::Subtitles => self.render_sub_popover(t, cx).into_any_element(),
            PopoverKind::Audio => self.render_audio_popover(t, cx).into_any_element(),
            PopoverKind::Speed => self.render_speed_popover(t, cx).into_any_element(),
            PopoverKind::More => self.render_more_popover(t, cx).into_any_element(),
            PopoverKind::Presets => self.render_presets_popover(t, cx).into_any_element(),
            PopoverKind::Variants => self.render_variants_popover(t, cx).into_any_element(),
        };

        let mut base = div()
            .id("popover")
            .absolute()
            .w(px(pop_w))
            .max_h(px(max_h))
            .overflow_hidden()
            .flex()
            .flex_col()
            .rounded(t.radius_lg)
            .bg(t.panel)
            .border_1()
            .border_color(t.line_strong)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(content);

        if is_top_anchored {
            let top = if fullscreen {
                FLOAT_GAP
            } else if self.effective_hide_titlebar() {
                f32::from(t.titlebar_h) + 6.
            } else {
                8.
            };
            base = base.top(px(top));
            if let Some(left) = left_px {
                base.left(px(left))
            } else {
                base.right(px(if fullscreen { FLOAT_GAP } else { 12. }))
            }
        } else {
            let bottom = if floating_dock {
                f32::from(t.dock_h) + 8.
            } else {
                8.
            };
            base = base.bottom(px(bottom));
            if let Some(left) = left_px {
                base.left(px(left))
            } else {
                base.right(px(12.))
            }
        }
    }

    fn render_speed_popover(&self, t: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.active().session.state().speed;
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
                .when(active, |d| d.bg(t.selected).text_color(t.text))
                .when(!active, |d| {
                    d.text_color(t.text_muted)
                        .hover(|s| s.bg(t.hover).text_color(t.text))
                })
                .child(format!("{speed:.2}×"))
                .when(active, |d| d.child(icon(Icon::Check, 12., t.accent)))
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
            .child(popover_header("播放速度", self.key_hint("speed_up"), t))
            .child(div().py(px(4.)).flex().flex_col().children(rows))
    }

    fn render_variants_popover(&self, t: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let (variants, active_var) = match &self.catalog.active_stream_meta {
            Some(meta) => (meta.variants.clone(), meta.active_variant.clone()),
            None => (Vec::new(), None),
        };
        let has_variants = !variants.is_empty();

        let rows = variants.into_iter().enumerate().map(|(i, v)| {
            let active = match active_var.as_deref() {
                Some(cur) => cur == v.id,
                None => i == 0,
            };
            let vid = v.id.clone();
            let vlabel = v.label.clone();
            div()
                .id(("variant-opt", i))
                .h(t.row_h)
                .px(px(10.))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .cursor_pointer()
                .font_family(FONT_MONO)
                .text_size(px(12.))
                .when(active, |d| d.bg(t.selected).text_color(t.text))
                .when(!active, |d| {
                    d.text_color(t.text_muted)
                        .hover(|s| s.bg(t.hover).text_color(t.text))
                })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .child(vlabel.clone()),
                )
                .when(active, |d| d.child(icon(Icon::Check, 12., t.accent)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.switch_stream_variant(&vid, cx);
                    this.show_osd(format!("清晰度 · {vlabel}"), cx);
                }))
        });

        div()
            .w_full()
            .flex()
            .flex_col()
            .child(popover_header("清晰度与线路", None, t))
            .child(
                div()
                    .id("variants-list")
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .py(px(4.))
                    .flex()
                    .flex_col()
                    .children(rows)
                    .when(!has_variants, |d| {
                        d.child(
                            div()
                                .px(px(10.))
                                .py(px(8.))
                                .text_size(px(11.))
                                .text_color(t.text_faint)
                                .child("当前流没有可切换的清晰度档位"),
                        )
                    }),
            )
    }
}
