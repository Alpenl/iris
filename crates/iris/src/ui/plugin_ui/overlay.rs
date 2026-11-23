//! Viewport normalized `[0..1]` HUD renderer for all 7 `OverlayNode` variants
//! (`Box`, `Line`, `Grid`, `Crosshair`, `Text`, `Bars`, `Button`).

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Rgba,
    SharedString, StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px, relative,
};
use iris_api::{Color, OverlayNode, TextAlign};

use super::super::IrisWindow;
use super::super::kit::icon;
use crate::assets::Icon;
use crate::theme::{FONT_MONO, FONT_UI, ThemeTokens, parse_hex_rgba};

fn resolve_overlay_color(color: &Color, opacity: f32, t: ThemeTokens) -> Rgba {
    let mut c = match color {
        Color::White => t.text,
        Color::Muted => t.text_muted,
        Color::Dim => t.text_dim,
        Color::Dark => t.window_bg,
        Color::Accent => t.accent,
        Color::Hex(hex) => parse_hex_rgba(hex).unwrap_or(t.accent),
    };
    let op = if opacity.is_finite() {
        opacity.clamp(0.0, 1.0)
    } else {
        0.6
    };
    c.a = (c.a * op).clamp(0.0, 1.0);
    c
}

impl IrisWindow {
    pub(in crate::ui) fn render_plugin_overlay(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let tagged_nodes: Vec<(String, OverlayNode)> = self
            .plugins
            .overlay_nodes()
            .into_iter()
            .map(|(pid, n)| (pid.to_string(), n.clone()))
            .collect();
        if tagged_nodes.is_empty() {
            return None;
        }

        let t = ThemeTokens::of(cx);
        let mut elements: Vec<AnyElement> = Vec::with_capacity(tagged_nodes.len());

        for (idx, (pid, node)) in tagged_nodes.into_iter().enumerate() {
            match node {
                OverlayNode::Box {
                    x,
                    y,
                    w,
                    h,
                    fill,
                    border,
                    border_width,
                    radius,
                    opacity,
                } => {
                    let rad = if radius.is_finite() {
                        radius.clamp(0., 32.)
                    } else {
                        0.
                    };
                    let bw = if border_width.is_finite() {
                        border_width.clamp(0., 8.)
                    } else {
                        1.
                    };
                    elements.push(
                        div()
                            .absolute()
                            .left(relative(x.clamp(0., 1.)))
                            .top(relative(y.clamp(0., 1.)))
                            .w(relative(w.clamp(0., 1.)))
                            .h(relative(h.clamp(0., 1.)))
                            .when_some(fill.as_ref(), |d, fc| {
                                d.bg(resolve_overlay_color(fc, opacity, t))
                            })
                            .when(rad > 0., |d| d.rounded(px(rad)))
                            .when_some(border.as_ref().filter(|_| bw > 0.), |d, bc| {
                                d.border(px(bw))
                                    .border_color(resolve_overlay_color(bc, opacity, t))
                            })
                            .into_any_element(),
                    );
                }
                OverlayNode::Line {
                    x1,
                    y1,
                    x2,
                    y2,
                    color,
                    width,
                    opacity,
                } => {
                    let c = resolve_overlay_color(&color, opacity, t);
                    let lw = if width.is_finite() {
                        width.clamp(1., 6.)
                    } else {
                        1.
                    };
                    let dx = (x2 - x1).abs();
                    let dy = (y2 - y1).abs();
                    if dx <= dy {
                        elements.push(
                            div()
                                .absolute()
                                .left(relative(((x1 + x2) * 0.5).clamp(0., 1.)))
                                .top(relative(y1.min(y2).clamp(0., 1.)))
                                .w(px(lw))
                                .h(relative(dy.clamp(0., 1.)))
                                .bg(c)
                                .into_any_element(),
                        );
                    } else {
                        elements.push(
                            div()
                                .absolute()
                                .left(relative(x1.min(x2).clamp(0., 1.)))
                                .top(relative(((y1 + y2) * 0.5).clamp(0., 1.)))
                                .w(relative(dx.clamp(0., 1.)))
                                .h(px(lw))
                                .bg(c)
                                .into_any_element(),
                        );
                    }
                }
                OverlayNode::Grid {
                    cols,
                    rows,
                    color,
                    opacity,
                } => {
                    let c = resolve_overlay_color(&color, opacity, t);
                    let cols = cols.clamp(2, 16);
                    let rows = rows.clamp(2, 16);
                    for i in 1..cols {
                        elements.push(
                            div()
                                .absolute()
                                .left(relative(i as f32 / cols as f32))
                                .top_0()
                                .bottom_0()
                                .w(px(1.))
                                .bg(c)
                                .into_any_element(),
                        );
                    }
                    for j in 1..rows {
                        elements.push(
                            div()
                                .absolute()
                                .top(relative(j as f32 / rows as f32))
                                .left_0()
                                .right_0()
                                .h(px(1.))
                                .bg(c)
                                .into_any_element(),
                        );
                    }
                }
                OverlayNode::Crosshair {
                    x,
                    y,
                    size,
                    color,
                    opacity,
                } => {
                    let c = resolve_overlay_color(&color, opacity, t);
                    let cx_pos = x.clamp(0., 1.);
                    let cy_pos = y.clamp(0., 1.);
                    let arm = size.clamp(0.01, 0.5);
                    elements.push(
                        div()
                            .absolute()
                            .left(relative((cx_pos - arm * 0.5).clamp(0., 1.)))
                            .top(relative(cy_pos))
                            .w(relative(arm))
                            .h(px(1.))
                            .bg(c)
                            .into_any_element(),
                    );
                    elements.push(
                        div()
                            .absolute()
                            .left(relative(cx_pos))
                            .top(relative((cy_pos - arm * 0.5).clamp(0., 1.)))
                            .w(px(1.))
                            .h(relative(arm))
                            .bg(c)
                            .into_any_element(),
                    );
                }
                OverlayNode::Text {
                    x,
                    y,
                    text: content,
                    size,
                    color,
                    bg,
                    mono,
                    align,
                    opacity,
                } => {
                    let sz = if size.is_finite() {
                        size.clamp(9., 64.)
                    } else {
                        12.
                    };
                    let fg = resolve_overlay_color(&color, opacity, t);
                    let y_clamped = y.clamp(0., 0.98);
                    let chip = div()
                        .font_family(if mono { FONT_MONO } else { FONT_UI })
                        .text_size(px(sz))
                        .text_color(fg)
                        .when_some(bg.as_ref(), |d, bg_col| {
                            d.px(px(6.))
                                .py(px(2.))
                                .rounded(t.radius_md)
                                .bg(resolve_overlay_color(
                                    bg_col,
                                    (opacity * 0.92).clamp(0.15, 0.96),
                                    t,
                                ))
                                .border_1()
                                .border_color(t.line)
                        })
                        .child(content);

                    let positioned = match align {
                        TextAlign::Left => div()
                            .absolute()
                            .left(relative(x.clamp(0., 0.98)))
                            .top(relative(y_clamped))
                            .child(chip),
                        TextAlign::Right => div()
                            .absolute()
                            .right(relative((1.0 - x.clamp(0., 1.)).clamp(0., 0.98)))
                            .top(relative(y_clamped))
                            .text_right()
                            .child(chip),
                        TextAlign::Center => {
                            let x_c = x.clamp(0., 1.);
                            let half_span = x_c.min(1.0 - x_c).max(0.02);
                            div()
                                .absolute()
                                .left(relative((x_c - half_span).clamp(0., 1.)))
                                .w(relative((half_span * 2.0).clamp(0.04, 1.0)))
                                .top(relative(y_clamped))
                                .flex()
                                .justify_center()
                                .text_center()
                                .child(chip)
                        }
                    };
                    elements.push(positioned.into_any_element());
                }
                OverlayNode::Bars {
                    x,
                    y,
                    w,
                    h,
                    values,
                    color,
                    opacity,
                } => {
                    if !values.is_empty() {
                        let c = resolve_overlay_color(&color, opacity, t);
                        elements.push(
                            div()
                                .absolute()
                                .left(relative(x.clamp(0., 1.)))
                                .top(relative(y.clamp(0., 1.)))
                                .w(relative(w.clamp(0., 1.)))
                                .h(relative(h.clamp(0., 1.)))
                                .flex()
                                .items_end()
                                .gap(px(1.))
                                .children(values.into_iter().take(128).map(|v| {
                                    let ratio = if v.is_finite() { v.clamp(0., 1.) } else { 0. };
                                    div().flex_1().min_w(px(1.)).h(relative(ratio)).bg(c)
                                }))
                                .into_any_element(),
                        );
                    }
                }
                OverlayNode::Button {
                    id,
                    x,
                    y,
                    label,
                    icon: icon_name,
                    accent: hi,
                    action_id,
                } => {
                    let btn_id = SharedString::from(format!("hud-btn:{idx}:{pid}:{id}"));
                    let glyph = icon_name.as_deref().and_then(Icon::from_name);
                    elements.push(
                        div()
                            .id(btn_id)
                            .absolute()
                            .left(relative(x.clamp(0., 0.95)))
                            .top(relative(y.clamp(0., 0.95)))
                            .h(px(26.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .rounded(t.radius_md)
                            .cursor_pointer()
                            .bg(if hi { t.selected } else { t.overlay })
                            .border_1()
                            .border_color(if hi { t.accent } else { t.line_strong })
                            .text_size(px(11.))
                            .text_color(if hi { t.text } else { t.text_muted })
                            .hover(|s| s.bg(t.hover).text_color(t.text).border_color(t.accent))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.invoke_plugin_action(&pid, &action_id, w, cx);
                            }))
                            .when_some(glyph, |d, g| {
                                d.child(icon(g, 12., if hi { t.accent } else { t.text_dim }))
                            })
                            .when(glyph.is_none() && hi, |d| {
                                d.child(div().size(px(5.)).bg(t.accent))
                            })
                            .when(!label.is_empty(), |d| d.child(label))
                            .into_any_element(),
                    );
                }
            }
        }
        Some(div().absolute().inset_0().children(elements))
    }
}
