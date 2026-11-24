//! Workspace presets management tab (switch, save new, update/delete custom)
//! and plugins management tab (rescan, reveal folder, enable/disable, source
//! provider creation, active media source actions, and permission pills).

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use iris_api::{Permission, SourceDescriptor, SourceManageAction};
use iris_core::plugin::{PluginInfo, PluginSource, PluginStatus, plugins_dir};
use iris_core::preset::{PresetCaptureScope, WorkspacePreset};

use super::super::IrisWindow;
use super::super::kit::form::{chip_button, toggle};
use super::super::kit::{badge_pill, bordered_button, hbetween, hstack, vstack};
use super::super::state::BuiltinDialog;
use crate::theme::{FONT_MONO, ThemeTokens};

fn render_preset_card(
    ix: usize,
    preset: WorkspacePreset,
    is_active: bool,
    key_badge: Option<SharedString>,
    t: ThemeTokens,
    cx: &Context<IrisWindow>,
) -> AnyElement {
    let is_builtin = preset.builtin;
    let (pid_apply, pid_update, pid_delete) = (preset.id.clone(), preset.id.clone(), preset.id);
    let (has_layout, has_playback, has_plugins) = (
        preset.layout.is_some(),
        preset.playback.is_some(),
        preset.plugins.is_some(),
    );
    let update_scope = if has_layout || has_playback || has_plugins {
        PresetCaptureScope {
            layout: has_layout,
            playback: has_playback,
            plugins: has_plugins,
        }
    } else {
        PresetCaptureScope::default()
    };

    let buttons = hstack(5.)
        .flex_none()
        .child(
            chip_button(("preset-apply", ix), "应用", is_active, t).on_click(
                cx.listener(move |this, _, w, cx| this.activate_preset(&pid_apply, w, cx)),
            ),
        )
        .when(!is_builtin, |d| {
            d.child(
                chip_button(("preset-update", ix), "更新", false, t).on_click(cx.listener(
                    move |this, _, w, cx| {
                        this.update_preset_from_current(&pid_update, update_scope, w, cx);
                        cx.notify();
                    },
                )),
            )
            .child(
                chip_button(("preset-del", ix), "删除", false, t).on_click(cx.listener(
                    move |this, _, w, cx| {
                        if this.presets.delete_custom(&pid_delete) {
                            if this.settings.active_preset == pid_delete {
                                this.activate_preset("default", w, cx);
                            }
                            this.show_toast("已删除自定义工作区预设", cx);
                            cx.notify();
                        }
                    },
                )),
            )
        });

    vstack(6.)
        .p(px(10.))
        .rounded(t.radius_md)
        .bg(t.window_bg)
        .border_1()
        .border_color(if is_active { t.accent } else { t.line })
        .child(
            hbetween(8.)
                .child(
                    hstack(6.)
                        .flex_1()
                        .min_w(px(0.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(t.text)
                                .truncate()
                                .child(preset.name),
                        )
                        .child(badge_pill(
                            if is_builtin { "内置" } else { "自定义" },
                            false,
                            t,
                        ))
                        .when(is_active, |d| d.child(badge_pill("当前使用", true, t)))
                        .when_some(key_badge, |d, kb| d.child(badge_pill(kb, false, t))),
                )
                .child(buttons),
        )
        .when(!preset.description.is_empty(), |d| {
            d.child(
                div()
                    .text_size(px(11.))
                    .text_color(t.text_dim)
                    .child(preset.description),
            )
        })
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(4.))
                .when(has_layout, |d| d.child(badge_pill("布局", false, t)))
                .when(has_playback, |d| d.child(badge_pill("播放策略", false, t)))
                .when(has_plugins, |d| d.child(badge_pill("插件组合", false, t))),
        )
        .into_any_element()
}

impl IrisWindow {
    pub(super) fn settings_presets(
        &self,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let presets = self.presets.list().to_vec();
        let active_id = self.settings.active_preset.clone();
        let cards: Vec<AnyElement> = presets
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let is_active = p.id == active_id;
                let kb = self.key_hint(&format!("preset:{}", p.id));
                render_preset_card(i, p, is_active, kb, t, cx)
            })
            .collect();

        vstack(12.)
            .child(
                hbetween(10.)
                    .pb(px(10.))
                    .border_b_1()
                    .border_color(t.line)
                    .child(
                        vstack(2.)
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(t.text)
                                    .child("工作区与场景预设"),
                            )
                            .child(
                                div().text_size(px(11.)).text_color(t.text_faint).child(
                                    "稀疏补丁仅修改预设声明字段，切回默认时自动还原个人基线",
                                ),
                            ),
                    )
                    .child(
                        bordered_button("save-new-preset-btn", "+ 保存为新预设…", true, t)
                            .on_click(cx.listener(|this, _, w, cx| {
                                this.open_builtin_dialog(
                                    BuiltinDialog::SavePreset {
                                        scope: PresetCaptureScope::default(),
                                    },
                                    w,
                                    cx,
                                );
                            })),
                    ),
            )
            .children(cards)
    }

    pub(super) fn settings_plugins(
        &self,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let list = self.plugins.list();
        let active_sources: Vec<(String, SourceDescriptor)> = self
            .plugins
            .media_sources()
            .into_iter()
            .map(|(pid, _, desc)| (pid.to_string(), desc.clone()))
            .collect();
        let dir = plugins_dir();
        let dir_text = dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "未知".into());

        let cards: Vec<AnyElement> = list
            .into_iter()
            .enumerate()
            .map(|(i, info)| {
                let plugin_sources: Vec<SourceDescriptor> = active_sources
                    .iter()
                    .filter(|(p, _)| p == &info.manifest.id)
                    .map(|(_, d)| d.clone())
                    .collect();
                Self::render_plugin_card(i, info, plugin_sources, t, cx)
            })
            .collect();

        vstack(12.)
            .child(
                hbetween(8.)
                    .pb(px(10.))
                    .border_b_1()
                    .border_color(t.line)
                    .child(
                        vstack(2.)
                            .flex_1()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(t.text)
                                    .child("控制面、虚拟数据源与界面主题插件"),
                            )
                            .child(
                                div()
                                    .font_family(FONT_MONO)
                                    .text_size(px(10.))
                                    .text_color(t.text_faint)
                                    .truncate()
                                    .child(dir_text),
                            ),
                    )
                    .child(
                        hstack(6.)
                            .flex_none()
                            .child(
                                bordered_button("reload-plugins", "重新扫描", false, t).on_click(
                                    cx.listener(|this, _, _, cx| this.reload_plugins(cx)),
                                ),
                            )
                            .when_some(dir, |d, p| {
                                d.child(
                                    bordered_button("open-plugins-dir", "打开插件目录", false, t)
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            let _ = std::fs::create_dir_all(&p);
                                            cx.reveal_path(&p);
                                        })),
                                )
                            }),
                    ),
            )
            .children(cards)
    }

    fn render_plugin_card(
        i: usize,
        info: PluginInfo,
        plugin_sources: Vec<SourceDescriptor>,
        t: ThemeTokens,
        cx: &Context<Self>,
    ) -> AnyElement {
        let pid = info.manifest.id;
        let pid_toggle = pid.clone();
        let enabled = info.enabled;
        let is_declarative =
            info.manifest.entry.is_none() && matches!(info.source, PluginSource::External(_));
        let has_theme_spec = info.manifest.theme.is_some() || info.manifest.chrome.is_some();
        let source_tag = match info.source {
            PluginSource::Builtin => "内置",
            PluginSource::External(_) if is_declarative => "声明式主题",
            PluginSource::External(_) => "外部进程",
        };
        let (status_text, status_accent) = match &info.status {
            PluginStatus::Running => ("运行中".to_string(), false),
            PluginStatus::Disabled => ("已禁用".to_string(), false),
            PluginStatus::Error(err) => (format!("错误: {err}"), true),
        };
        let provider = info.manifest.source_provider;

        let source_rows: Vec<AnyElement> = plugin_sources
            .into_iter()
            .enumerate()
            .map(|(si, src)| Self::render_plugin_source_row(pid.clone(), si, enabled, src, t, cx))
            .collect();
        let perm_pills = info.manifest.permissions.into_iter().map(|perm| {
            let hi = matches!(
                perm,
                Permission::UiTheme | Permission::MediaSource | Permission::Secrets
            );
            badge_pill(perm.label(), hi, t)
        });

        vstack(6.)
            .id(("plugin-card", i))
            .p(px(10.))
            .rounded(t.radius_md)
            .bg(t.window_bg)
            .border_1()
            .border_color(t.line)
            .child(
                hbetween(10.)
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(t.text)
                                    .child(info.manifest.name),
                            )
                            .child(
                                div()
                                    .font_family(FONT_MONO)
                                    .text_size(px(10.))
                                    .text_color(t.text_faint)
                                    .child(format!("v{}", info.manifest.version)),
                            )
                            .child(badge_pill(source_tag, false, t))
                            .when(has_theme_spec, |d| d.child(badge_pill("主题皮肤", true, t)))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(if status_accent {
                                        t.accent
                                    } else {
                                        t.text_faint
                                    })
                                    .child(status_text),
                            ),
                    )
                    .child(
                        hstack(6.)
                            .when_some(provider.filter(|_| enabled), |d, sp| {
                                let (pid_add, cmd_id) = (pid.clone(), sp.add_command_id);
                                d.child(
                                    chip_button(
                                        ("pl-prov-add", i),
                                        format!("+ 添加{}", sp.label),
                                        true,
                                        t,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, w, cx| {
                                            this.invoke_plugin_command(&pid_add, &cmd_id, w, cx);
                                        },
                                    )),
                                )
                            })
                            .child(toggle(
                                ("plugin-toggle", i),
                                enabled,
                                t,
                                cx.listener(move |this, _, _, cx| {
                                    this.toggle_plugin_enabled(&pid_toggle, !enabled, cx);
                                }),
                            )),
                    ),
            )
            .when(!info.manifest.description.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(11.))
                        .text_color(t.text_dim)
                        .child(info.manifest.description),
                )
            })
            .when(!source_rows.is_empty(), |d| {
                d.child(vstack(4.).children(source_rows))
            })
            .child(div().flex().flex_wrap().gap(px(4.)).children(perm_pills))
            .into_any_element()
    }

    fn render_plugin_source_row(
        pid: String,
        row_ix: usize,
        enabled: bool,
        src: SourceDescriptor,
        t: ThemeTokens,
        cx: &Context<Self>,
    ) -> AnyElement {
        let (pid_s, sid_s) = (pid.clone(), src.id.clone());
        let (pid_cfg, sid_cfg) = (pid.clone(), src.id.clone());
        let (pid_rm, sid_rm) = (pid, src.id);
        let auth_cmd = src.auth_command_id;

        hbetween(8.)
            .px(px(8.))
            .py(px(5.))
            .rounded(t.radius_sm)
            .bg(t.panel)
            .border_1()
            .border_color(t.line)
            .child(
                hstack(6.)
                    .min_w(px(0.))
                    .child(badge_pill("数据源", true, t))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(t.text)
                            .truncate()
                            .child(src.name),
                    )
                    .when(src.supports_search, |el| {
                        el.child(badge_pill("支持搜索", false, t))
                    }),
            )
            .when(enabled, |el| {
                el.child(
                    hstack(5.)
                        .flex_none()
                        .when_some(auth_cmd, |row, cmd_id| {
                            let pid_auth = pid_s.clone();
                            row.child(
                                chip_button(("pl-src-auth", row_ix), "登录", true, t).on_click(
                                    cx.listener(move |this, _, w, cx| {
                                        this.invoke_plugin_command(&pid_auth, &cmd_id, w, cx);
                                    }),
                                ),
                            )
                        })
                        .when(src.configurable, |row| {
                            row.child(
                                chip_button(("pl-src-cfg", row_ix), "配置", false, t).on_click(
                                    cx.listener(move |this, _, w, cx| {
                                        this.manage_plugin_source(
                                            &pid_cfg,
                                            &sid_cfg,
                                            SourceManageAction::Configure,
                                            w,
                                            cx,
                                        );
                                    }),
                                ),
                            )
                        })
                        .when(src.removable, |row| {
                            row.child(
                                chip_button(("pl-src-rm", row_ix), "移除", false, t).on_click(
                                    cx.listener(move |this, _, w, cx| {
                                        this.manage_plugin_source(
                                            &pid_rm,
                                            &sid_rm,
                                            SourceManageAction::Remove,
                                            w,
                                            cx,
                                        );
                                    }),
                                ),
                            )
                        })
                        .child(
                            chip_button(("pl-src-open", row_ix), "浏览", false, t).on_click(
                                cx.listener(move |this, _, w, cx| {
                                    this.overlays.settings_open = false;
                                    this.focus.focus(w);
                                    this.browse_plugin_source(&pid_s, &sid_s, None, None, None, cx);
                                }),
                            ),
                        ),
                )
            })
            .into_any_element()
    }
}
