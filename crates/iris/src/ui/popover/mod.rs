//! Floating menus anchored directly to their trigger buttons (`t.radius_lg`),
//! plus playback speed and stream quality variant popovers.

mod more_and_presets;
mod tracks;

use gpui::{
    App, Context, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use iris_core::player::{Command, SPEED_STEPS};

use super::AppModel;
use super::kit::list_row::select_row;
use super::state::PopoverKind;
use super::views::{DirtyRegions, model_listener};
use crate::theme::{FLOAT_GAP, FONT_MONO, ThemeTokens};

pub(super) fn popover_header(
    title: &'static str,
    hint: Option<SharedString>,
    t: ThemeTokens,
) -> impl IntoElement {
    div()
        .min_h(px((f32::from(t.control_h_sm) + 6.).max(28.)))
        .flex_none()
        .px(px(10.))
        .py(px(3.))
        .flex()
        .items_center()
        .justify_between()
        .border_b_1()
        .border_color(t.line)
        .child(
            div()
                .text_size(t.text_sm)
                .text_color(t.text_dim)
                .child(title),
        )
        .when_some(hint, |d, hint| {
            d.child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(t.text_xs)
                    .text_color(t.text_faint)
                    .child(hint),
            )
        })
}

pub(super) fn render_popover(
    model: &Entity<AppModel>,
    kind: PopoverKind,
    fullscreen: bool,
    window: &Window,
    cx: &App,
) -> impl IntoElement {
    let m = model.read(cx);
    let t = ThemeTokens::of(cx);
    let is_top_anchored = kind == PopoverKind::Presets;

    let base_pop_w: f32 = match kind {
        PopoverKind::Speed => 172.,
        PopoverKind::Variants => 204.,
        PopoverKind::More => 276.,
        PopoverKind::Subtitles | PopoverKind::Audio => 284.,
        PopoverKind::Presets => 304.,
    };
    let base_max_h: f32 = match kind {
        PopoverKind::More | PopoverKind::Audio | PopoverKind::Presets => 440.,
        _ => 360.,
    };

    let win_sz = window.viewport_size();
    let vw = f32::from(win_sz.width);
    let vh = f32::from(win_sz.height);
    let pop_w = base_pop_w.min((vw - 16.).max(140.));

    let anchor = if is_top_anchored && fullscreen {
        None
    } else {
        m.popover_anchor(kind)
    };
    let left_px = anchor.map(|ab| {
        let btn_cx = f32::from(ab.origin.x) + f32::from(ab.size.width) * 0.5;
        let max_l = (vw - pop_w - 8.).max(8.);
        (btn_cx - pop_w * 0.5).clamp(8., max_l)
    });
    let (margin_v, max_h) = if is_top_anchored {
        let top = anchor.map_or_else(
            || {
                if fullscreen {
                    FLOAT_GAP
                } else {
                    f32::from(t.titlebar_h) + 6.
                }
            },
            |ab| (f32::from(ab.origin.y) + f32::from(ab.size.height) + 6.).max(8.),
        );
        (top, base_max_h.min((vh - top - 8.).max(96.)))
    } else {
        let bottom = anchor.map_or_else(
            || f32::from(t.dock_h) + 8.,
            |ab| (vh - f32::from(ab.origin.y) + 6.).max(8.),
        );
        (bottom, base_max_h.min((vh - bottom - 8.).max(96.)))
    };

    let content = match kind {
        PopoverKind::Subtitles => tracks::render_sub_popover(model, t, cx).into_any_element(),
        PopoverKind::Audio => tracks::render_audio_popover(model, t, cx).into_any_element(),
        PopoverKind::Speed => render_speed_popover(model, t, cx).into_any_element(),
        PopoverKind::More => more_and_presets::render_more_popover(model, t, cx).into_any_element(),
        PopoverKind::Presets => {
            more_and_presets::render_presets_popover(model, t, cx).into_any_element()
        }
        PopoverKind::Variants => render_variants_popover(model, t, cx).into_any_element(),
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
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(content);

    let panel = if is_top_anchored {
        base = base.top(px(margin_v));
        if let Some(left) = left_px {
            base.left(px(left))
        } else {
            base.right(px(if fullscreen { FLOAT_GAP } else { 12. }))
        }
    } else {
        base = base.bottom(px(margin_v));
        if let Some(left) = left_px {
            base.left(px(left))
        } else {
            base.right(px(12.))
        }
    };

    div()
        .id("popover-backdrop")
        .absolute()
        .inset_0()
        .on_mouse_down(
            MouseButton::Left,
            model_listener(model, |this, _, _, cx| {
                this.overlays.popover = None;
                cx.stop_propagation();
                this.notify_regions(
                    DirtyRegions {
                        root: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            model_listener(model, |this, _, _, cx| {
                this.overlays.popover = None;
                cx.stop_propagation();
                this.notify_regions(
                    DirtyRegions {
                        root: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
            }),
        )
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(panel)
}

impl AppModel {
    pub(super) fn handle_popover_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(kind) = self.overlays.popover else {
            return false;
        };
        let len = match kind {
            PopoverKind::Speed => SPEED_STEPS.len(),
            PopoverKind::Variants => self
                .catalog
                .active_stream_meta
                .as_ref()
                .map_or(0, |m| m.variants.len()),
            _ => return false,
        };
        if len == 0 {
            return false;
        }
        match key {
            "up" | "down" => {
                let next = match self.overlays.popover_index {
                    Some(i) => {
                        let d: isize = if key == "down" { 1 } else { -1 };
                        (i as isize + d).rem_euclid(len as isize) as usize
                    }
                    None if key == "down" => 0,
                    None => len - 1,
                };
                self.overlays.popover_index = Some(next);
                self.notify_root(cx);
                true
            }
            "enter" => {
                let Some(idx) = self.overlays.popover_index else {
                    return false;
                };
                match kind {
                    PopoverKind::Speed => {
                        if let Some(&speed) = SPEED_STEPS.get(idx) {
                            self.command(Command::SetSpeed(speed), cx);
                            self.overlays.popover = None;
                            self.overlays.popover_index = None;
                            self.show_osd(format!("倍速 · {speed:.2}×"), cx);
                            self.notify_dock(cx);
                        }
                    }
                    PopoverKind::Variants => {
                        let chosen = self
                            .catalog
                            .active_stream_meta
                            .as_ref()
                            .and_then(|m| m.variants.get(idx))
                            .map(|v| (v.id.clone(), v.label.clone()));
                        if let Some((vid, vlabel)) = chosen {
                            self.switch_stream_variant(&vid, cx);
                            self.overlays.popover_index = None;
                            self.show_osd(format!("清晰度 · {vlabel}"), cx);
                            self.notify_dock(cx);
                        }
                    }
                    _ => return false,
                }
                true
            }
            _ => false,
        }
    }
}

fn render_speed_popover(model: &Entity<AppModel>, t: ThemeTokens, cx: &App) -> impl IntoElement {
    let m = model.read(cx);
    let current = m.active().session.state().speed;
    let focused_ix = m.overlays.popover_index;
    let speed_hint = m.key_hint("speed_up");
    let rows = SPEED_STEPS.iter().copied().enumerate().map(|(i, speed)| {
        let active = (current - speed).abs() < 0.02;
        let kb_focus = focused_ix == Some(i);
        select_row(
            ("speed-opt", i),
            format!("{speed:.2}×"),
            None,
            active,
            kb_focus,
            true,
            t,
        )
        .on_click(model_listener(model, move |this, _, _, cx| {
            this.command(Command::SetSpeed(speed), cx);
            this.overlays.popover = None;
            this.overlays.popover_index = None;
            this.show_osd(format!("倍速 · {speed:.2}×"), cx);
            this.notify_dock(cx);
        }))
    });

    div()
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .child(popover_header("播放速度", speed_hint, t))
        .child(
            div()
                .id("speed-list")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .py(px(4.))
                .flex()
                .flex_col()
                .children(rows),
        )
}

fn render_variants_popover(model: &Entity<AppModel>, t: ThemeTokens, cx: &App) -> impl IntoElement {
    let m = model.read(cx);
    let (variants, active_var) = match &m.catalog.active_stream_meta {
        Some(meta) => (meta.variants.clone(), meta.active_variant.clone()),
        None => (Vec::new(), None),
    };
    let has_variants = !variants.is_empty();

    let focused_ix = m.overlays.popover_index;
    let rows = variants.into_iter().enumerate().map(|(i, v)| {
        let active = match active_var.as_deref() {
            Some(cur) => cur == v.id,
            None => i == 0,
        };
        let kb_focus = focused_ix == Some(i);
        let vid = v.id.clone();
        let vlabel = v.label.clone();
        select_row(
            ("variant-opt", i),
            vlabel.clone(),
            None,
            active,
            kb_focus,
            true,
            t,
        )
        .on_click(model_listener(model, move |this, _, _, cx| {
            this.switch_stream_variant(&vid, cx);
            this.show_osd(format!("清晰度 · {vlabel}"), cx);
            this.notify_dock(cx);
        }))
    });

    div()
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .child(popover_header("清晰度与线路", None, t))
        .child(
            div()
                .id("variants-list")
                .flex_1()
                .min_h(px(0.))
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
                            .text_size(t.text_sm)
                            .text_color(t.text_faint)
                            .child("当前流没有可切换的清晰度档位"),
                    )
                }),
        )
}
