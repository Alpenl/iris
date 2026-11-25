//! Docked and fullscreen-floating workbench sidebar shell: header (`t.titlebar_h`),
//! 4-mode switcher (`Files`, `Sources`, `Queue`, `Plugins`), and drag-resize handle.

mod catalog_sync;
mod files;
mod queue;
mod sources;

pub(in crate::ui) use files::DirRow;

use gpui::{
    Context, CursorStyle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use iris_core::preset::SidebarMode;
use iris_core::settings::Side;

use super::IrisWindow;
use super::actions::OpenFile;
use super::kit::{badge_pill, icon_button, tip};
use super::state::{BuiltinDialog, SettingsTab};
use crate::assets::Icon;
use crate::theme::ThemeTokens;

impl IrisWindow {
    pub(super) fn render_sidebar(
        &self,
        floating: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        let mode = self.catalog.sidebar_mode;
        let vw = f32::from(window.viewport_size().width);
        let max_w = if floating {
            (vw - 48.).max(180.)
        } else {
            (vw - 220.).max(180.)
        };
        let width = self.settings.directory_width.min(max_w);
        let side = self.settings.directory_side;
        let queue_len = self.queue.len();

        let header_actions = match mode {
            SidebarMode::Files => div()
                .flex()
                .items_center()
                .gap(px(2.))
                .child(
                    icon_button("sb-search", Icon::Search, self.text_inputs.search_open, t)
                        .tooltip(tip("搜索目录", self.key_hint("toggle_search")))
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.toggle_search(&super::actions::ToggleSearch, w, cx);
                        })),
                )
                .child(
                    icon_button("sb-sort", Icon::Sort, false, t)
                        .tooltip(tip("切换排序方式", None))
                        .on_click(cx.listener(|this, _, _, cx| this.cycle_sort(cx))),
                )
                .child(
                    icon_button("sb-pin", Icon::Pin, false, t)
                        .tooltip(tip("固定当前目录为书签", None))
                        .on_click(cx.listener(|this, _, _, cx| this.pin_current_location(cx))),
                )
                .child(
                    icon_button("sb-refresh", Icon::Refresh, false, t)
                        .tooltip(tip("刷新目录", self.key_hint("refresh_directory")))
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.refresh_directory(&super::actions::RefreshDirectory, w, cx);
                        })),
                )
                .child(
                    icon_button("sb-open", Icon::FolderOpen, false, t)
                        .tooltip(tip("打开本地文件…", self.key_hint("open_file")))
                        .on_click(cx.listener(|this, _, w, cx| this.open_file(&OpenFile, w, cx))),
                ),
            SidebarMode::Sources => div().flex().items_center().gap(px(2.)).child(
                icon_button("sb-open-url", Icon::Link, false, t)
                    .tooltip(tip("打开网络流地址…", self.key_hint("open_stream_url")))
                    .on_click(cx.listener(|this, _, w, cx| {
                        this.open_builtin_dialog(BuiltinDialog::OpenUrl, w, cx);
                    })),
            ),
            SidebarMode::Queue => div()
                .flex()
                .items_center()
                .gap(px(2.))
                .child(
                    icon_button("sb-q-add", Icon::Plus, false, t)
                        .tooltip(tip("追加本地媒体到队列…", None))
                        .on_click(cx.listener(|this, _, w, cx| this.prompt_append_queue(w, cx))),
                )
                .when(queue_len > 0, |d| {
                    d.child(
                        icon_button("sb-q-clear", Icon::Close, false, t)
                            .tooltip(tip("清空播放队列", None))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.queue.clear();
                                cx.notify();
                            })),
                    )
                }),
            SidebarMode::Plugins => div()
                .flex()
                .items_center()
                .gap(px(2.))
                .child(
                    icon_button("sb-pl-reload", Icon::Refresh, false, t)
                        .tooltip(tip("重新扫描插件", None))
                        .on_click(cx.listener(|this, _, _, cx| this.reload_plugins(cx))),
                )
                .child(
                    icon_button("sb-pl-settings", Icon::Settings, false, t)
                        .tooltip(tip("插件管理设置", None))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.overlays.open_settings(Some(SettingsTab::Plugins));
                            cx.notify();
                        })),
                ),
        };

        let modes = [
            (SidebarMode::Files, "目录"),
            (SidebarMode::Sources, "数据源"),
            (SidebarMode::Queue, "队列"),
            (SidebarMode::Plugins, "插件"),
        ];

        let body = match mode {
            SidebarMode::Files => self.render_sidebar_files(window, cx).into_any_element(),
            SidebarMode::Sources => self.render_sidebar_sources(cx).into_any_element(),
            SidebarMode::Queue => self.render_sidebar_queue(cx).into_any_element(),
            SidebarMode::Plugins => self.render_plugin_sidebar(window, cx).into_any_element(),
        };

        div()
            .id("sidebar")
            .relative()
            .w(px(width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(if floating { t.overlay } else { t.panel })
            .when(side == Side::Left, |d| d.border_r_1().border_color(t.line))
            .when(side == Side::Right, |d| d.border_l_1().border_color(t.line))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(t.titlebar_h)
                    .flex_none()
                    .pl(px(10.))
                    .pr(px(6.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(t.line)
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(px(12.))
                            .text_color(t.text)
                            .child(self.sidebar_header_title()),
                    )
                    .child(header_actions),
            )
            .child(
                div()
                    .h(px(30.))
                    .flex_none()
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .border_b_1()
                    .border_color(t.line)
                    .children(modes.into_iter().enumerate().map(|(i, (m, label))| {
                        let active = m == mode;
                        div()
                            .id(("sb-mode", i))
                            .flex_1()
                            .h(px(22.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap(px(4.))
                            .rounded(t.radius_sm)
                            .cursor_pointer()
                            .text_size(px(11.))
                            .when(active, |d| d.bg(t.selected).text_color(t.text))
                            .when(!active, |d| {
                                d.text_color(t.text_dim)
                                    .hover(|s| s.bg(t.hover).text_color(t.text))
                            })
                            .child(label)
                            .when(m == SidebarMode::Queue && queue_len > 0, |d| {
                                d.child(badge_pill(queue_len.to_string(), active, t))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.catalog.sidebar_mode = m;
                                cx.notify();
                            }))
                    })),
            )
            .child(body)
            .when(!floating, |d| {
                let handle = div()
                    .id("sidebar-resize-handle")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .w(px(5.))
                    .cursor(CursorStyle::ResizeLeftRight)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            this.chrome.resizing_dir = Some((f32::from(ev.position.x), width));
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    );
                d.child(if side == Side::Left {
                    handle.right_0()
                } else {
                    handle.left_0()
                })
            })
    }

    fn sidebar_header_title(&self) -> String {
        match self.catalog.sidebar_mode {
            SidebarMode::Files => match &self.catalog.active_source {
                super::state::ActiveSource::Local => self
                    .catalog
                    .local_dir
                    .folder()
                    .and_then(|f| f.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "本地目录".into()),
                super::state::ActiveSource::Plugin(ps) => ps.title.clone(),
            },
            SidebarMode::Sources => "数据源与书签".into(),
            SidebarMode::Queue => "独立播放队列".into(),
            SidebarMode::Plugins => "插件工作台".into(),
        }
    }
}
