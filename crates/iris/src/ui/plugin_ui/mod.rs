//! Host-owned renderers for the plugin UI workbench:
//! - Sidebar panel container (`render_plugin_sidebar`)
//! - Control bar status badge slot (`render_plugin_status_slot`)
//! - Declarative `PluginModal` built on `kit::modal::modal_shell` with async validation

mod overlay;
mod widgets;

use std::rc::Rc;

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use iris_api::{SidebarPanel, StatusBadge};

use super::IrisWindow;
use super::kit::modal::{ModalCallback, ModalShellSpec, modal_shell};
use super::kit::tip;
use crate::theme::{FONT_MONO, ThemeTokens};
use widgets::{WidgetSurface, render_plugin_widget};

impl IrisWindow {
    pub(super) fn render_plugin_status_slot(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let badges: Vec<(String, StatusBadge)> = self
            .plugins
            .status_badges()
            .into_iter()
            .map(|(pid, b)| (pid.to_string(), b.clone()))
            .collect();
        if badges.is_empty() {
            return None;
        }
        let t = ThemeTokens::of(cx);
        Some(div().flex().items_center().gap(px(4.)).pr(px(4.)).children(
            badges.into_iter().enumerate().map(|(i, (pid, badge))| {
                let action = badge.action_id.clone();
                let label = badge.text.clone();
                let tip_str = badge
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
                    .rounded(t.radius_md)
                    .bg(t.panel)
                    .border_1()
                    .border_color(if badge.accent {
                        t.accent
                    } else {
                        t.line_strong
                    })
                    .font_family(FONT_MONO)
                    .text_size(px(10.))
                    .text_color(if badge.accent { t.text } else { t.text_muted })
                    .when(action.is_some(), |d| {
                        d.cursor_pointer().hover(|s| s.bg(t.hover))
                    })
                    .tooltip(tip(tip_str, None))
                    .when_some(action, |d, act| {
                        d.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.invoke_plugin_action(&pid, &act, w, cx);
                            }))
                    })
                    .when(badge.accent, |d| d.child(div().size(px(5.)).bg(t.accent)))
                    .child(label)
            }),
        ))
    }

    pub(super) fn render_plugin_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let panels: Vec<(String, SidebarPanel)> = self
            .plugins
            .sidebar_panels()
            .into_iter()
            .map(|(pid, p)| (pid.to_string(), p.clone()))
            .collect();
        let empty = panels.is_empty();
        let t = ThemeTokens::of(cx);

        let mut panel_els: Vec<AnyElement> = Vec::with_capacity(panels.len());
        for (pi, (pid, panel_data)) in panels.into_iter().enumerate() {
            let mut section_els: Vec<AnyElement> = Vec::with_capacity(panel_data.sections.len());
            for (si, section) in panel_data.sections.into_iter().enumerate() {
                let mut widget_els: Vec<AnyElement> = Vec::with_capacity(section.widgets.len());
                for (wi, widget) in section.widgets.into_iter().enumerate() {
                    widget_els.push(render_plugin_widget(
                        self,
                        &pid,
                        pi * 10000 + si * 100 + wi,
                        widget,
                        WidgetSurface::Sidebar,
                        t,
                        cx,
                    ));
                }
                section_els.push(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .when(!section.heading.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(t.text_faint)
                                    .child(section.heading),
                            )
                        })
                        .children(widget_els)
                        .into_any_element(),
                );
            }
            panel_els.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(10.))
                    .rounded(t.radius_lg)
                    .bg(t.window_bg)
                    .border_1()
                    .border_color(t.line)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .pb(px(6.))
                            .border_b_1()
                            .border_color(t.line)
                            .child(div().size(px(6.)).bg(t.accent))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(t.text)
                                    .child(panel_data.title),
                            ),
                    )
                    .children(section_els)
                    .into_any_element(),
            );
        }

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
                                .text_color(t.text_dim)
                                .child("没有提供侧栏面板的启用插件"),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(t.text_faint)
                                .child("在 设置 → 插件 中启用或添加外部插件"),
                        ),
                )
            })
            .children(panel_els)
    }

    pub(super) fn render_plugin_modal(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (pid, modal) = self
            .plugins
            .active_modal()
            .map(|(p, m)| (p.to_string(), m.clone()))?;

        let t = ThemeTokens::of(cx);
        let vp = window.viewport_size();
        let modal_w = (f32::from(vp.width) - 32.).clamp(340., 460.);
        let max_h = (f32::from(vp.height) - 32.).clamp(240., 540.);

        let is_submitting = self
            .overlays
            .submitting_plugin_modal
            .as_ref()
            .is_some_and(|(p, m)| p == &pid && m == &modal.id);

        let submit_label = if is_submitting {
            "处理中…".into()
        } else {
            modal
                .submit_label
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "确认".into())
        };
        let cancel_label = modal
            .cancel_label
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "取消".into());

        let submit_cb: ModalCallback = Rc::new(|this, w, cx| {
            this.submit_active_plugin_modal(w, cx);
        });
        let cancel_cb: ModalCallback = Rc::new(|this, w, cx| {
            this.cancel_active_plugin_modal(Some(w), cx);
        });

        let mut widget_nodes: Vec<AnyElement> = Vec::with_capacity(modal.widgets.len());
        for (wi, widget) in modal.widgets.into_iter().enumerate() {
            widget_nodes.push(render_plugin_widget(
                self,
                &pid,
                1_000_000 + wi,
                widget,
                WidgetSurface::Modal {
                    modal_id: &modal.id,
                    on_submit: submit_cb.clone(),
                    window,
                },
                t,
                cx,
            ));
        }

        let body = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .when_some(modal.subtitle.filter(|s| !s.trim().is_empty()), |d, sub| {
                d.child(div().text_size(px(11.)).text_color(t.text_dim).child(sub))
            })
            .children(widget_nodes)
            .into_any_element();

        Some(modal_shell(
            ModalShellSpec::dialog(
                "plugin-modal",
                modal.title,
                modal_w,
                max_h,
                cancel_label,
                submit_label,
                body,
                cancel_cb,
                submit_cb,
            ),
            t,
            cx,
        ))
    }
}
