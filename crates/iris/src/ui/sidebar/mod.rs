//! Docked and fullscreen-floating workbench sidebar entity (`Sidebar`): header (`t.titlebar_h`),
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
use iris_core::library::{SortBy, SortOrder};
use iris_core::preset::SidebarMode;
use iris_core::settings::Side;

use super::AppModel;
use super::actions::OpenFile;
use super::kit::{ButtonSize, ButtonVariant, button, icon_button, tab_strip, tab_strip_item, tip};
use super::state::{BuiltinDialog, SettingsTab};
use super::views::{DirtyRegions, Sidebar, model_listener};
use crate::assets::Icon;
use crate::theme::ThemeTokens;

impl Sidebar {
    pub(in crate::ui) fn render_sidebar_entity(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let floating = self.floating;
        let model = self.model.clone();
        let m = model.read(cx);
        let t = ThemeTokens::of(cx);
        let mode = m.catalog.sidebar_mode;
        let vw = f32::from(window.viewport_size().width);
        let max_w = if floating {
            (vw - 48.).max(180.)
        } else {
            (vw - 220.).max(180.)
        };
        let width = m.settings.directory_width.min(max_w);
        let side = m.settings.directory_side;
        let queue_len = m.queue.len();

        let sort_by_label = match m.settings.sort_by {
            SortBy::Name => "名称",
            SortBy::Modified => "修改时间",
            SortBy::Size => "文件大小",
        };
        let sort_order_label = match m.settings.sort_order {
            SortOrder::Asc => "升序",
            SortOrder::Desc => "降序",
        };
        let sort_tip = format!("排序：按{sort_by_label} · {sort_order_label}（点击切换）");
        let confirm_clear_queue = m.is_confirming("clear-queue");
        let search_open = m.text_inputs.search_open;
        let title = m.sidebar_header_title();
        let hint_search = m.key_hint("toggle_search");
        let hint_refresh = m.key_hint("refresh_directory");
        let hint_open = m.key_hint("open_file");
        let hint_url = m.key_hint("open_url");

        let header_actions = match mode {
            SidebarMode::Files => div()
                .flex()
                .items_center()
                .gap(px(2.))
                .child(
                    icon_button("sb-search", Icon::Search, search_open, t)
                        .tooltip(tip(
                            super::strings::actions::action_title("toggle_search"),
                            hint_search,
                        ))
                        .on_click(model_listener(&model, |this, _, w, cx| {
                            this.toggle_search(&super::actions::ToggleSearch, w, cx);
                        })),
                )
                .child(
                    icon_button("sb-sort", Icon::Sort, false, t)
                        .tooltip(tip(sort_tip, None))
                        .on_click(model_listener(&model, |this, _, _, cx| this.cycle_sort(cx))),
                )
                .child(
                    icon_button("sb-pin", Icon::Pin, false, t)
                        .tooltip(tip("固定当前目录为书签", None))
                        .on_click(model_listener(&model, |this, _, _, cx| {
                            this.pin_current_location(cx);
                        })),
                )
                .child(
                    icon_button("sb-refresh", Icon::Refresh, false, t)
                        .tooltip(tip(
                            super::strings::actions::action_title("refresh_directory"),
                            hint_refresh,
                        ))
                        .on_click(model_listener(&model, |this, _, w, cx| {
                            this.refresh_directory(&super::actions::RefreshDirectory, w, cx);
                        })),
                )
                .child(
                    icon_button("sb-open", Icon::FolderOpen, false, t)
                        .tooltip(tip(
                            super::strings::actions::action_title("open_file"),
                            hint_open,
                        ))
                        .on_click(model_listener(&model, |this, _, w, cx| {
                            this.open_file(&OpenFile, w, cx);
                        })),
                ),
            SidebarMode::Sources => div().flex().items_center().gap(px(2.)).child(
                icon_button("sb-open-url", Icon::Link, false, t)
                    .tooltip(tip(super::strings::chrome::OPEN_STREAM_LABEL, hint_url))
                    .on_click(model_listener(&model, |this, _, w, cx| {
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
                        .on_click(model_listener(&model, |this, _, w, cx| {
                            this.prompt_append_queue(w, cx);
                        })),
                )
                .when(queue_len > 0, |d| {
                    if confirm_clear_queue {
                        d.child(
                            button(
                                "sb-q-clear",
                                "确认清空",
                                ButtonVariant::Danger,
                                ButtonSize::Xs,
                                t,
                            )
                            .tooltip(tip("再次点击确认清空播放队列", None))
                            .on_click(model_listener(
                                &model,
                                |this, _, _, cx| {
                                    if this.confirm_destructive("clear-queue", cx) {
                                        this.queue.clear();
                                        this.notify_regions(
                                            DirtyRegions {
                                                sidebar: true,
                                                dock: true,
                                                ..DirtyRegions::NONE
                                            },
                                            cx,
                                        );
                                        this.show_toast("已清空播放队列", cx);
                                    }
                                },
                            )),
                        )
                    } else {
                        d.child(
                            icon_button("sb-q-clear", Icon::Close, false, t)
                                .tooltip(tip("清空播放队列", None))
                                .on_click(model_listener(&model, |this, _, _, cx| {
                                    if this.confirm_destructive("clear-queue", cx) {
                                        this.queue.clear();
                                        this.notify_regions(
                                            DirtyRegions {
                                                sidebar: true,
                                                dock: true,
                                                ..DirtyRegions::NONE
                                            },
                                            cx,
                                        );
                                        this.show_toast("已清空播放队列", cx);
                                    }
                                })),
                        )
                    }
                }),
            SidebarMode::Plugins => div()
                .flex()
                .items_center()
                .gap(px(2.))
                .child(
                    icon_button("sb-pl-reload", Icon::Refresh, false, t)
                        .tooltip(tip("重新扫描插件", None))
                        .on_click(model_listener(&model, |this, _, _, cx| {
                            this.reload_plugins(cx);
                        })),
                )
                .child(
                    icon_button("sb-pl-settings", Icon::Settings, false, t)
                        .tooltip(tip("插件管理设置", None))
                        .on_click(model_listener(&model, |this, _, w, cx| {
                            this.overlays.open_settings(Some(SettingsTab::Plugins));
                            this.reconcile_input_focus(w, cx);
                            this.notify_regions(
                                DirtyRegions {
                                    root: true,
                                    settings: true,
                                    ..DirtyRegions::NONE
                                },
                                cx,
                            );
                        })),
                ),
        };

        let modes = [
            SidebarMode::Files,
            SidebarMode::Sources,
            SidebarMode::Queue,
            SidebarMode::Plugins,
        ];

        let body = match mode {
            SidebarMode::Files => self.render_sidebar_files(window, cx).into_any_element(),
            SidebarMode::Sources => sources::render_sidebar_sources(&model, cx).into_any_element(),
            SidebarMode::Queue => self.render_sidebar_queue(cx).into_any_element(),
            SidebarMode::Plugins => self.plugin_panel.clone().into_any_element(),
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
                            .text_size(t.text_md)
                            .text_color(t.text)
                            .child(title),
                    )
                    .child(header_actions),
            )
            .child(
                tab_strip(t).children(modes.into_iter().enumerate().map(|(i, target_mode)| {
                    let active = target_mode == mode;
                    let badge = (target_mode == SidebarMode::Queue && queue_len > 0)
                        .then(|| queue_len.to_string().into());
                    tab_strip_item(
                        ("sb-mode", i),
                        super::strings::presets::sidebar_mode_label(target_mode),
                        badge,
                        active,
                        t,
                    )
                    .on_click(model_listener(&model, move |this, _, w, cx| {
                        this.catalog.sidebar_mode = target_mode;
                        this.sync_view_timer(cx);
                        this.reconcile_input_focus(w, cx);
                        this.notify_sidebar(cx);
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
                        model_listener(&model, move |this, ev: &MouseDownEvent, _, cx| {
                            this.chrome.resizing_dir = Some((f32::from(ev.position.x), width));
                            cx.stop_propagation();
                            this.notify_root(cx);
                        }),
                    );
                d.child(if side == Side::Left {
                    handle.right_0()
                } else {
                    handle.left_0()
                })
            })
    }
}

impl AppModel {
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
