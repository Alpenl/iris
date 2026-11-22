//! Host-owned renderers for the three visual plugin slots:
//! 1. Viewport vector overlay slot (`OverlayShape`)
//! 2. Control bar status slot (`StatusItem`)
//! 3. Sidebar structured panel slot (`SidebarPanel`)
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Rgba,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px, relative,
};
use iris_api::{OverlayColor, OverlayShape, SidebarItem};

use super::widgets::tip;
use super::*;

fn overlay_rgba(color: OverlayColor, opacity: f32) -> Rgba {
    let mut c = match color {
        OverlayColor::White => text(),
        OverlayColor::Accent => accent(),
        OverlayColor::Dim => text_dim(),
        OverlayColor::Dark => window_bg(),
    };
    c.a = opacity.clamp(0.05, 1.0);
    c
}

impl IrisWindow {
    /// Draws normalized `[0..1]` vector shapes over the viewport without
    /// intercepting pointer events.
    pub(super) fn render_plugin_overlay(&self) -> Option<impl IntoElement> {
        let shapes = self.plugins.overlay_shapes();
        if shapes.is_empty() {
            return None;
        }
        let mut nodes: Vec<AnyElement> = Vec::new();
        for shape in shapes {
            match shape {
                OverlayShape::Rect {
                    x,
                    y,
                    w,
                    h,
                    color,
                    opacity,
                    stroke,
                } => {
                    let c = overlay_rgba(*color, *opacity);
                    let t = stroke.clamp(1., 4.);
                    nodes.push(
                        div()
                            .absolute()
                            .left(relative(x.clamp(0., 1.)))
                            .top(relative(y.clamp(0., 1.)))
                            .w(relative(w.clamp(0., 1.)))
                            .h(relative(h.clamp(0., 1.)))
                            .child(div().absolute().top_0().left_0().right_0().h(px(t)).bg(c))
                            .child(
                                div()
                                    .absolute()
                                    .bottom_0()
                                    .left_0()
                                    .right_0()
                                    .h(px(t))
                                    .bg(c),
                            )
                            .child(div().absolute().top_0().bottom_0().left_0().w(px(t)).bg(c))
                            .child(div().absolute().top_0().bottom_0().right_0().w(px(t)).bg(c))
                            .into_any_element(),
                    );
                }
                OverlayShape::Line {
                    x1,
                    y1,
                    x2,
                    y2,
                    color,
                    opacity,
                    thickness,
                } => {
                    let c = overlay_rgba(*color, *opacity);
                    let t = thickness.clamp(1., 4.);
                    let dx = (x2 - x1).abs();
                    let dy = (y2 - y1).abs();
                    if dx <= dy {
                        let x = ((x1 + x2) * 0.5).clamp(0., 1.);
                        let top = y1.min(*y2).clamp(0., 1.);
                        let h = dy.clamp(0., 1.);
                        nodes.push(
                            div()
                                .absolute()
                                .left(relative(x))
                                .top(relative(top))
                                .w(px(t))
                                .h(relative(h))
                                .bg(c)
                                .into_any_element(),
                        );
                    } else {
                        let y = ((y1 + y2) * 0.5).clamp(0., 1.);
                        let left = x1.min(*x2).clamp(0., 1.);
                        let w = dx.clamp(0., 1.);
                        nodes.push(
                            div()
                                .absolute()
                                .left(relative(left))
                                .top(relative(y))
                                .w(relative(w))
                                .h(px(t))
                                .bg(c)
                                .into_any_element(),
                        );
                    }
                }
                OverlayShape::Grid {
                    cols,
                    rows,
                    color,
                    opacity,
                } => {
                    let c = overlay_rgba(*color, *opacity);
                    let cols = (*cols).clamp(2, 12);
                    let rows = (*rows).clamp(2, 12);
                    for i in 1..cols {
                        let x = i as f32 / cols as f32;
                        nodes.push(
                            div()
                                .absolute()
                                .left(relative(x))
                                .top_0()
                                .bottom_0()
                                .w(px(1.))
                                .bg(c)
                                .into_any_element(),
                        );
                    }
                    for j in 1..rows {
                        let y = j as f32 / rows as f32;
                        nodes.push(
                            div()
                                .absolute()
                                .top(relative(y))
                                .left_0()
                                .right_0()
                                .h(px(1.))
                                .bg(c)
                                .into_any_element(),
                        );
                    }
                }
                OverlayShape::Crosshair {
                    x,
                    y,
                    size,
                    color,
                    opacity,
                } => {
                    let c = overlay_rgba(*color, *opacity);
                    let cx_pos = x.clamp(0., 1.);
                    let cy_pos = y.clamp(0., 1.);
                    let arm = size.clamp(0.01, 0.5);
                    nodes.push(
                        div()
                            .absolute()
                            .left(relative((cx_pos - arm * 0.5).clamp(0., 1.)))
                            .top(relative(cy_pos))
                            .w(relative(arm))
                            .h(px(1.))
                            .bg(c)
                            .into_any_element(),
                    );
                    nodes.push(
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
                OverlayShape::Label {
                    x,
                    y,
                    text: label,
                    color,
                } => {
                    let c = overlay_rgba(*color, 0.9);
                    nodes.push(
                        div()
                            .absolute()
                            .left(relative(x.clamp(0., 0.95)))
                            .top(relative(y.clamp(0., 0.95)))
                            .px(px(5.))
                            .py(px(2.))
                            .bg(overlay())
                            .border_1()
                            .border_color(line())
                            .font_family(FONT_MONO)
                            .text_size(px(10.))
                            .text_color(c)
                            .child(label.clone())
                            .into_any_element(),
                    );
                }
            }
        }
        Some(div().absolute().inset_0().children(nodes))
    }

    /// Compact plugin badges placed inside the control bar status slot.
    pub(super) fn render_plugin_status_slot(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let items: Vec<(String, iris_api::StatusItem)> = self
            .plugins
            .status_items()
            .into_iter()
            .map(|(pid, s)| (pid.to_string(), s.clone()))
            .collect();
        if items.is_empty() {
            return None;
        }
        Some(div().flex().items_center().gap(px(4.)).pr(px(4.)).children(
            items.into_iter().enumerate().map(|(i, (pid, item))| {
                let action = item.action_id.clone();
                let label = item.text.clone();
                let tip_str = item
                    .tooltip
                    .clone()
                    .unwrap_or_else(|| format!("插件状态 · {pid}"));
                div()
                    .id(("plugin-status", i))
                    .h(px(22.))
                    .px(px(7.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .bg(panel())
                    .border_1()
                    .border_color(if item.accent { accent() } else { line_strong() })
                    .font_family(FONT_MONO)
                    .text_size(px(10.))
                    .text_color(if item.accent { text() } else { text_muted() })
                    .when(action.is_some(), |d| {
                        d.cursor_pointer().hover(|s| s.bg(hover()))
                    })
                    .tooltip(tip(tip_str, None))
                    .when_some(action, |d, act| {
                        d.on_click(cx.listener(move |this, _, w, cx| {
                            this.invoke_plugin_action(&pid, &act, w, cx);
                        }))
                    })
                    .when(item.accent, |d| d.child(div().size(px(5.)).bg(accent())))
                    .child(label)
            }),
        ))
    }

    /// Structured plugin panels rendered inside the sidebar when switched to plugin view.
    pub(super) fn render_plugin_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let panels: Vec<(String, iris_api::SidebarPanel)> = self
            .plugins
            .sidebar_panels()
            .into_iter()
            .map(|(pid, p)| (pid.to_string(), p.clone()))
            .collect();
        let empty = panels.is_empty();

        div()
            .id("plugin-sidebar-scroll")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p(px(10.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .when(empty, |d| {
                d.child(
                    div()
                        .px(px(4.))
                        .py(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(text_dim())
                                .child("没有提供侧栏面板的启用插件"),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(text_faint())
                                .child("在 设置 → 插件 中启用或添加外部插件"),
                        ),
                )
            })
            .children(
                panels
                    .into_iter()
                    .enumerate()
                    .map(|(pi, (pid, panel_data))| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .p(px(10.))
                            .bg(window_bg())
                            .border_1()
                            .border_color(line())
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .pb(px(6.))
                                    .border_b_1()
                                    .border_color(line())
                                    .child(div().size(px(6.)).bg(accent()))
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(text())
                                            .child(panel_data.title),
                                    ),
                            )
                            .children(panel_data.sections.into_iter().enumerate().map(
                                |(si, section)| {
                                    let pid_sec = pid.clone();
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap(px(6.))
                                        .child(
                                            div()
                                                .text_size(px(10.))
                                                .text_color(text_faint())
                                                .child(section.heading),
                                        )
                                        .children(section.items.into_iter().enumerate().map(
                                            |(ii, item)| {
                                                let pid_item = pid_sec.clone();
                                                let base_id = pi * 1000 + si * 100 + ii;
                                                match item {
                                                    SidebarItem::Text { text: txt, muted } => div()
                                                        .text_size(px(11.))
                                                        .text_color(if muted {
                                                            text_dim()
                                                        } else {
                                                            text()
                                                        })
                                                        .child(txt)
                                                        .into_any_element(),
                                                    SidebarItem::KeyValue {
                                                        key,
                                                        value,
                                                        accent: hi,
                                                    } => div()
                                                        .flex()
                                                        .items_center()
                                                        .justify_between()
                                                        .text_size(px(11.))
                                                        .child(
                                                            div().text_color(text_dim()).child(key),
                                                        )
                                                        .child(
                                                            div()
                                                                .font_family(FONT_MONO)
                                                                .text_color(if hi {
                                                                    accent()
                                                                } else {
                                                                    text()
                                                                })
                                                                .child(value),
                                                        )
                                                        .into_any_element(),
                                                    SidebarItem::Progress { label, ratio } => div()
                                                        .flex()
                                                        .flex_col()
                                                        .gap(px(4.))
                                                        .child(
                                                            div()
                                                                .text_size(px(10.))
                                                                .text_color(text_dim())
                                                                .child(label),
                                                        )
                                                        .child(
                                                            div()
                                                                .h(px(4.))
                                                                .w_full()
                                                                .bg(line())
                                                                .child(
                                                                    div()
                                                                        .h_full()
                                                                        .w(relative(
                                                                            ratio.clamp(0., 1.),
                                                                        ))
                                                                        .bg(accent()),
                                                                ),
                                                        )
                                                        .into_any_element(),
                                                    SidebarItem::Button {
                                                        label,
                                                        action_id,
                                                        accent: hi,
                                                    } => div()
                                                        .id(("pl-btn", base_id))
                                                        .h(px(26.))
                                                        .px(px(8.))
                                                        .flex()
                                                        .items_center()
                                                        .cursor_pointer()
                                                        .border_1()
                                                        .border_color(if hi {
                                                            accent()
                                                        } else {
                                                            line_strong()
                                                        })
                                                        .bg(if hi { selected() } else { panel() })
                                                        .text_size(px(11.))
                                                        .text_color(if hi {
                                                            text()
                                                        } else {
                                                            text_muted()
                                                        })
                                                        .hover(|s| s.bg(hover()).text_color(text()))
                                                        .on_mouse_down(
                                                            MouseButton::Left,
                                                            |_, _, cx| cx.stop_propagation(),
                                                        )
                                                        .on_click(cx.listener(
                                                            move |this, _, w, cx| {
                                                                this.invoke_plugin_action(
                                                                    &pid_item, &action_id, w, cx,
                                                                );
                                                            },
                                                        ))
                                                        .child(label)
                                                        .into_any_element(),
                                                    SidebarItem::ButtonRow { buttons } => div()
                                                        .flex()
                                                        .flex_wrap()
                                                        .gap(px(4.))
                                                        .children(
                                                            buttons.into_iter().enumerate().map(
                                                                |(bi, btn)| {
                                                                    let pid_b = pid_item.clone();
                                                                    let act = btn.action_id;
                                                                    div()
                                                        .id(("pl-rbtn", base_id * 16 + bi))
                                                        .h(px(24.))
                                                        .px(px(8.))
                                                        .flex()
                                                        .items_center()
                                                        .cursor_pointer()
                                                        .border_1()
                                                        .border_color(if btn.accent {
                                                            accent()
                                                        } else {
                                                            line_strong()
                                                        })
                                                        .bg(if btn.accent {
                                                            selected()
                                                        } else {
                                                            panel()
                                                        })
                                                        .text_size(px(11.))
                                                        .text_color(if btn.accent {
                                                            text()
                                                        } else {
                                                            text_muted()
                                                        })
                                                        .hover(|s| {
                                                            s.bg(hover()).text_color(text())
                                                        })
                                                        .on_mouse_down(
                                                            MouseButton::Left,
                                                            |_, _, cx| cx.stop_propagation(),
                                                        )
                                                        .on_click(cx.listener(
                                                            move |this, _, w, cx| {
                                                                this.invoke_plugin_action(
                                                                    &pid_b, &act, w, cx,
                                                                );
                                                            },
                                                        ))
                                                        .child(btn.label)
                                                                },
                                                            ),
                                                        )
                                                        .into_any_element(),
                                                }
                                            },
                                        ))
                                },
                            ))
                    }),
            )
    }
}
