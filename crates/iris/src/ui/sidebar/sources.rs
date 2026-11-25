//! Single canonical Data Sources & Bookmarks sidebar panel: quick open actions,
//! dynamic `plugins.source_providers()` `+ 添加…` buttons, pinned bookmarks,
//! and `plugins.media_sources()` instance cards (`Configure` / `Remove`).

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use iris_api::SourceManageAction;
use iris_api::v2::source::SourceDescriptor;
use iris_core::media_uri::MediaUri;
use iris_core::vfs::PinnedBookmark;

use super::super::IrisWindow;
use super::super::actions::OpenFile;
use super::super::kit::{
    ButtonSize, ButtonVariant, badge_pill, bordered_button, button, icon, icon_button,
    section_label, surface_card, tip,
};
use super::super::state::BuiltinDialog;
use crate::assets::Icon;
use crate::theme::{FONT_MONO, ThemeTokens};

impl IrisWindow {
    pub(super) fn render_sidebar_sources(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        let providers = self.plugins.source_providers();
        let sources: Vec<(String, String, SourceDescriptor)> = self
            .plugins
            .media_sources()
            .into_iter()
            .map(|(pid, pname, desc)| (pid.to_string(), pname.to_string(), desc.clone()))
            .collect();
        let bookmarks: Vec<PinnedBookmark> = self.locations.bookmarks.clone();

        div()
            .id("sidebar-sources-scroll")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p(px(10.))
            .flex()
            .flex_col()
            .gap(px(14.))
            // 1. Quick open actions + dynamic `+ 添加…` providers
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(section_label("快速打开与连接创建", t))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(6.))
                            .child(
                                bordered_button("src-open-local", "本地文件 / 目录…", false, t)
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.open_file(&OpenFile, w, cx);
                                    })),
                            )
                            .child(
                                bordered_button("src-open-http", "网络流 URL…", false, t).on_click(
                                    cx.listener(|this, _, w, cx| {
                                        this.open_builtin_dialog(BuiltinDialog::OpenUrl, w, cx);
                                    }),
                                ),
                            )
                            .children(providers.into_iter().enumerate().map(|(i, (pid, spec))| {
                                let cmd = spec.add_command_id;
                                bordered_button(
                                    ("src-provider-add", i),
                                    format!("+ 添加{}", spec.label),
                                    true,
                                    t,
                                )
                                .on_click(cx.listener(
                                    move |this, _, w, cx| {
                                        this.invoke_plugin_command(&pid, &cmd, w, cx);
                                    },
                                ))
                            })),
                    ),
            )
            // 2. Pinned bookmarks
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(section_label("常驻书签", t))
                            .child(badge_pill(bookmarks.len().to_string(), false, t)),
                    )
                    .when(bookmarks.is_empty(), |d| {
                        d.child(
                            surface_card(0., t)
                                .p(px(8.))
                                .text_size(t.text_sm)
                                .text_color(t.text_faint)
                                .child("在目录顶栏点击大头针可将常用文件夹固定在此处"),
                        )
                    })
                    .children(bookmarks.into_iter().enumerate().map(|(i, bm)| {
                        let bm_open = bm.clone();
                        let bm_id = bm.id.clone();
                        let confirm_key = format!("rm-bm:{bm_id}");
                        let confirming = self.is_confirming(&confirm_key);
                        let glyph = match &bm.uri {
                            MediaUri::Local(_) => Icon::Folder,
                            MediaUri::Plugin {
                                plugin_id,
                                source_id,
                                ..
                            } => self.plugin_source_icon(plugin_id, source_id),
                            MediaUri::HttpUrl(_) => Icon::Link,
                        };
                        let remove_el = if confirming {
                            button(
                                ("bm-del", i),
                                "确认移除",
                                ButtonVariant::Danger,
                                ButtonSize::Xs,
                                t,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if this.confirm_destructive(confirm_key.clone(), cx) {
                                        this.locations.remove_bookmark(&bm_id);
                                        this.show_toast("已移除书签", cx);
                                    }
                                },
                            ))
                        } else {
                            icon_button(("bm-del", i), Icon::Close, false, t)
                                .tooltip(tip("移除书签", None))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if this.confirm_destructive(confirm_key.clone(), cx) {
                                        this.locations.remove_bookmark(&bm_id);
                                        this.show_toast("已移除书签", cx);
                                    }
                                }))
                        };
                        div()
                            .id(("bm-card", i))
                            .min_h(t.row_h)
                            .px(px(8.))
                            .py(px(2.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(6.))
                            .rounded(t.radius_md)
                            .bg(t.window_bg)
                            .border_1()
                            .border_color(t.line)
                            .cursor_pointer()
                            .hover(|s| s.bg(t.hover).border_color(t.line_strong))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .min_w(px(0.))
                                    .child(icon(glyph, 13., t.accent))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(t.text_md)
                                            .text_color(t.text)
                                            .child(bm.name),
                                    ),
                            )
                            .child(remove_el)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_bookmark(&bm_open, cx);
                            }))
                    })),
            )
            // 3. Connected MediaSource instances
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(section_label("已连接数据源", t))
                            .child(badge_pill(sources.len().to_string(), false, t)),
                    )
                    .when(sources.is_empty(), |d| {
                        d.child(
                            surface_card(0., t)
                                .p(px(8.))
                                .text_size(t.text_sm)
                                .text_color(t.text_faint)
                                .child("尚未添加远程存储或云盘连接"),
                        )
                    })
                    .children(
                        sources
                            .into_iter()
                            .enumerate()
                            .map(|(i, (pid, pname, desc))| {
                                let (pid_browse, sid_browse) = (pid.clone(), desc.id.clone());
                                let (pid_cfg, sid_cfg) = (pid.clone(), desc.id.clone());
                                let (pid_rm, sid_rm) = (pid.clone(), desc.id.clone());
                                let rm_key = format!("rm-src:{pid}:{}", desc.id);
                                let rm_confirming = self.is_confirming(&rm_key);
                                let pid_auth = pid.clone();
                                let auth_cmd = desc.auth_command_id.clone();
                                let glyph = super::super::plugin_runtime::source_icon_for_name(
                                    desc.icon.as_deref(),
                                );
                                surface_card(6., t)
                                    .id(("src-inst", i))
                                    .p(px(9.))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .gap(px(6.))
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap(px(6.))
                                                    .min_w(px(0.))
                                                    .child(icon(glyph, 13., t.accent))
                                                    .child(
                                                        div()
                                                            .truncate()
                                                            .text_size(t.text_md)
                                                            .text_color(t.text)
                                                            .child(desc.name),
                                                    ),
                                            )
                                            .child(badge_pill(pname, false, t)),
                                    )
                                    .when_some(desc.subtitle.filter(|s| !s.is_empty()), |d, sub| {
                                        d.child(
                                            div()
                                                .font_family(FONT_MONO)
                                                .text_size(t.text_xs)
                                                .text_color(t.text_faint)
                                                .truncate()
                                                .child(sub),
                                        )
                                    })
                                    .child(
                                        div()
                                            .flex()
                                            .flex_wrap()
                                            .items_center()
                                            .gap(px(6.))
                                            .child(
                                                bordered_button(("src-go", i), "浏览", true, t)
                                                    .on_click(cx.listener(
                                                        move |this, _, _, cx| {
                                                            this.browse_plugin_source(
                                                                &pid_browse,
                                                                &sid_browse,
                                                                None,
                                                                None,
                                                                None,
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                            )
                                            .when_some(
                                                auth_cmd.filter(|_| desc.requires_auth),
                                                |r, cmd| {
                                                    r.child(
                                                        bordered_button(
                                                            ("src-login", i),
                                                            "登录",
                                                            false,
                                                            t,
                                                        )
                                                        .on_click(cx.listener(
                                                            move |this, _, w, cx| {
                                                                this.invoke_plugin_command(
                                                                    &pid_auth, &cmd, w, cx,
                                                                );
                                                            },
                                                        )),
                                                    )
                                                },
                                            )
                                            .when(desc.configurable, |r| {
                                                r.child(
                                                    bordered_button(
                                                        ("src-cfg", i),
                                                        "配置",
                                                        false,
                                                        t,
                                                    )
                                                    .on_click(cx.listener(move |this, _, w, cx| {
                                                        this.manage_plugin_source(
                                                            &pid_cfg,
                                                            &sid_cfg,
                                                            SourceManageAction::Configure,
                                                            w,
                                                            cx,
                                                        );
                                                    })),
                                                )
                                            })
                                            .when(desc.removable, |r| {
                                                r.child(
                                                    button(
                                                        ("src-rm", i),
                                                        if rm_confirming {
                                                            "再次点击确认"
                                                        } else {
                                                            "移除"
                                                        },
                                                        if rm_confirming {
                                                            ButtonVariant::Danger
                                                        } else {
                                                            ButtonVariant::Secondary
                                                        },
                                                        ButtonSize::Md,
                                                        t,
                                                    )
                                                    .on_click(cx.listener(move |this, _, w, cx| {
                                                        if this
                                                            .confirm_destructive(rm_key.clone(), cx)
                                                        {
                                                            this.manage_plugin_source(
                                                                &pid_rm,
                                                                &sid_rm,
                                                                SourceManageAction::Remove,
                                                                w,
                                                                cx,
                                                            );
                                                        }
                                                    })),
                                                )
                                            }),
                                    )
                            }),
                    ),
            )
    }
}
