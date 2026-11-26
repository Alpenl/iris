//! Workspace presets management tab (switch, save new, update/delete custom)
//! and plugins management tab (rescan, reveal folder, enable/disable, source
//! provider creation, active media source actions, and permission pills).

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use iris_api::SourceManageAction;
use iris_api::v2::{Capability, source::SourceDescriptor};
use iris_core::plugin_v2::{
    hub::{PluginInfo, PluginStatus},
    product::plugins_dir,
};
use iris_core::preset::{PresetCaptureScope, WorkspacePreset};

use super::super::IrisWindow;
use super::super::kit::form::{chip_button, toggle};
use super::super::kit::{
    ButtonSize, ButtonVariant, badge_pill, bordered_button, button, hbetween, hstack, surface_card,
    vstack,
};
use super::super::state::BuiltinDialog;
use crate::theme::{FONT_MONO, ThemeTokens};

fn render_preset_card(
    ix: usize,
    preset: WorkspacePreset,
    is_active: bool,
    confirm_delete: bool,
    key_badge: Option<SharedString>,
    t: ThemeTokens,
    cx: &Context<IrisWindow>,
) -> AnyElement {
    let is_builtin = preset.builtin;
    let display_name = super::super::strings::presets::preset_name(&preset).to_string();
    let display_desc = super::super::strings::presets::preset_description(&preset).to_string();
    let (pid_apply, pid_update, pid_delete) = (preset.id.clone(), preset.id.clone(), preset.id);
    let del_key = format!("del-preset:{pid_delete}");
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
                button(
                    ("preset-del", ix),
                    if confirm_delete {
                        "再次点击确认"
                    } else {
                        "删除"
                    },
                    if confirm_delete {
                        ButtonVariant::Danger
                    } else {
                        ButtonVariant::Secondary
                    },
                    ButtonSize::Sm,
                    t,
                )
                .on_click(cx.listener(move |this, _, w, cx| {
                    if this.confirm_destructive(del_key.clone(), cx)
                        && this.presets.delete_custom(&pid_delete)
                    {
                        this.keymap.reset_binding(&format!("preset:{pid_delete}"));
                        crate::ui::rebind_keys(cx, &this.keymap);
                        if this.settings.active_preset == pid_delete {
                            this.activate_preset("default", w, cx);
                        }
                        this.show_toast("已删除自定义工作区预设", cx);
                        cx.notify();
                    }
                })),
            )
        });

    surface_card(6., t)
        .border_color(if is_active { t.accent } else { t.line })
        .child(
            hbetween(8.)
                .child(
                    hstack(6.)
                        .flex_1()
                        .min_w(px(0.))
                        .child(
                            div()
                                .text_size(t.text_md)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(t.text)
                                .truncate()
                                .child(display_name),
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
        .when(!display_desc.is_empty(), |d| {
            d.child(
                div()
                    .text_size(t.text_sm)
                    .text_color(t.text_dim)
                    .child(display_desc),
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
                let confirm_del = self.is_confirming(&format!("del-preset:{}", p.id));
                let kb = self.key_hint(&format!("preset:{}", p.id));
                render_preset_card(i, p, is_active, confirm_del, kb, t, cx)
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
                                    .text_size(t.text_md)
                                    .text_color(t.text)
                                    .child("工作区与场景预设"),
                            )
                            .child(
                                div().text_size(t.text_sm).text_color(t.text_faint).child(
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
            .iter()
            .enumerate()
            .map(|(i, info)| {
                let plugin_sources: Vec<SourceDescriptor> = active_sources
                    .iter()
                    .filter(|(p, _)| p == &info.id)
                    .map(|(_, d)| d.clone())
                    .collect();
                self.render_plugin_card(i, info.clone(), plugin_sources, t, cx)
            })
            .collect();

        vstack(12.)
            .child(
                hbetween(8.)
                    .flex_wrap()
                    .pb(px(10.))
                    .border_b_1()
                    .border_color(t.line)
                    .child(
                        vstack(2.)
                            .flex_1()
                            .min_w(px(160.))
                            .child(
                                div()
                                    .text_size(t.text_md)
                                    .text_color(t.text)
                                    .child("控制面、虚拟数据源与界面主题插件"),
                            )
                            .child(
                                div()
                                    .font_family(FONT_MONO)
                                    .text_size(t.text_xs)
                                    .text_color(t.text_faint)
                                    .truncate()
                                    .child(dir_text),
                            ),
                    )
                    .child(
                        hstack(6.)
                            .flex_none()
                            .child(
                                bordered_button("install-plugin", "安装插件包", true, t).on_click(
                                    cx.listener(|this, _, _, cx| this.choose_plugin_package(cx)),
                                ),
                            )
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
        &self,
        i: usize,
        info: PluginInfo,
        plugin_sources: Vec<SourceDescriptor>,
        t: ThemeTokens,
        cx: &Context<Self>,
    ) -> AnyElement {
        let pid = info.id.clone();
        let pid_toggle = pid.clone();
        let is_official = pid.to_ascii_lowercase().starts_with("iris.");
        let enabled = info.enabled;
        let manifest = info.binding.as_ref().map(|b| b.manifest.as_ref());
        let has_theme_spec = manifest.is_some_and(|m| m.theme.is_some() || m.chrome.is_some());
        let source_tag = if is_official {
            "官方 Wasm"
        } else if manifest.is_some_and(|m| m.entry.is_none()) {
            "声明式主题"
        } else {
            "Wasm 沙箱"
        };
        let (status_text, status_error) = match &info.status {
            PluginStatus::Dormant => ("等待窗口".to_owned(), false),
            PluginStatus::Loading => ("加载中".to_owned(), false),
            PluginStatus::Ready => ("运行中".to_owned(), false),
            PluginStatus::Stopping => ("正在停止".to_owned(), false),
            PluginStatus::Disabled => ("已禁用".to_owned(), false),
            PluginStatus::Failed(error) => (
                format!(
                    "异常：{}",
                    super::super::plugin_runtime::describe_error_code(*error)
                ),
                true,
            ),
        };
        let provider = manifest.and_then(|m| m.source_provider.clone());
        let name = manifest.map_or_else(|| pid.clone(), |m| m.name.clone());
        let name_for_del = name.clone();
        let version = manifest.map_or_else(String::new, |m| m.version.clone());
        let description = manifest.map_or_else(String::new, |m| m.description.clone());
        let source_rows: Vec<AnyElement> = plugin_sources
            .into_iter()
            .enumerate()
            .map(|(si, src)| {
                let confirm_rm = self.is_confirming(&format!("rm-src:{pid}:{}", src.id));
                Self::render_plugin_source_row(pid.clone(), si, enabled, confirm_rm, src, t, cx)
            })
            .collect();
        let perm_pills = info
            .binding
            .iter()
            .flat_map(|b| b.installed.grants.iter().copied())
            .map(|perm| {
                let hi = matches!(
                    perm,
                    Capability::UiTheme | Capability::MediaSource | Capability::Secrets
                );
                badge_pill(super::super::strings::chrome::capability_label(perm), hi, t)
            });

        surface_card(6., t)
            .id(("plugin-card", i))
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
                                    .text_size(t.text_md)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(t.text)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .font_family(FONT_MONO)
                                    .text_size(t.text_xs)
                                    .text_color(t.text_faint)
                                    .child(format!("v{}", version)),
                            )
                            .child(badge_pill(source_tag, false, t))
                            .when(has_theme_spec, |d| d.child(badge_pill("主题皮肤", true, t)))
                            .child(
                                div()
                                    .text_size(t.text_xs)
                                    .text_color(if status_error { t.danger } else { t.text_faint })
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
                            .when(has_theme_spec && enabled, |d| {
                                let theme = pid.clone();
                                d.child(
                                    chip_button(("plugin-theme", i), "应用主题", false, t)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.settings.theme_id = format!("plugin:{theme}");
                                            this.sync_theme_tokens(cx);
                                            this.persist_user_settings();
                                            cx.notify();
                                        })),
                                )
                            })
                            .when(
                                !is_official && !matches!(info.status, PluginStatus::Stopping),
                                |d| {
                                    let pid_del = pid.clone();
                                    d.child(
                                        chip_button(("plugin-del", i), "删除", false, t).on_click(
                                            cx.listener(move |this, _, w, cx| {
                                                this.open_builtin_dialog(
                                                    BuiltinDialog::ConfirmUninstallPlugin {
                                                        plugin_id: pid_del.clone(),
                                                        plugin_name: name_for_del.clone(),
                                                        keep_data: true,
                                                    },
                                                    w,
                                                    cx,
                                                );
                                            }),
                                        ),
                                    )
                                },
                            )
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
            .when(!description.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(t.text_sm)
                        .text_color(t.text_dim)
                        .child(description),
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
        confirm_rm: bool,
        src: SourceDescriptor,
        t: ThemeTokens,
        cx: &Context<Self>,
    ) -> AnyElement {
        let (pid_s, sid_s) = (pid.clone(), src.id.clone());
        let (pid_cfg, sid_cfg) = (pid.clone(), src.id.clone());
        let (pid_rm, sid_rm) = (pid.clone(), src.id.clone());
        let auth_id = plugin_source_row_id("pl-src-auth", &pid, row_ix);
        let cfg_id = plugin_source_row_id("pl-src-cfg", &pid, row_ix);
        let rm_id = plugin_source_row_id("pl-src-rm", &pid, row_ix);
        let open_id = plugin_source_row_id("pl-src-open", &pid, row_ix);
        let rm_key = format!("rm-src:{pid}:{}", src.id);
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
                            .text_size(t.text_sm)
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
                        .when_some(auth_cmd.filter(|_| src.requires_auth), |row, cmd_id| {
                            let pid_auth = pid_s.clone();
                            row.child(chip_button(auth_id, "登录", true, t).on_click(cx.listener(
                                move |this, _, w, cx| {
                                    this.invoke_plugin_command(&pid_auth, &cmd_id, w, cx);
                                },
                            )))
                        })
                        .when(src.configurable, |row| {
                            row.child(chip_button(cfg_id, "配置", false, t).on_click(cx.listener(
                                move |this, _, w, cx| {
                                    this.manage_plugin_source(
                                        &pid_cfg,
                                        &sid_cfg,
                                        SourceManageAction::Configure,
                                        w,
                                        cx,
                                    );
                                },
                            )))
                        })
                        .when(src.removable, |row| {
                            row.child(
                                button(
                                    rm_id,
                                    if confirm_rm {
                                        "再次点击确认"
                                    } else {
                                        "移除"
                                    },
                                    if confirm_rm {
                                        ButtonVariant::Danger
                                    } else {
                                        ButtonVariant::Secondary
                                    },
                                    ButtonSize::Sm,
                                    t,
                                )
                                .on_click(cx.listener(
                                    move |this, _, w, cx| {
                                        if this.confirm_destructive(rm_key.clone(), cx) {
                                            this.manage_plugin_source(
                                                &pid_rm,
                                                &sid_rm,
                                                SourceManageAction::Remove,
                                                w,
                                                cx,
                                            );
                                        }
                                    },
                                )),
                            )
                        })
                        .child(chip_button(open_id, "浏览", false, t).on_click(cx.listener(
                            move |this, _, w, cx| {
                                this.overlays.settings_open = false;
                                this.focus.focus(w);
                                this.browse_plugin_source(&pid_s, &sid_s, None, None, None, cx);
                            },
                        ))),
                )
            })
            .into_any_element()
    }
}

fn plugin_source_row_id(kind: &str, pid: &str, row_ix: usize) -> SharedString {
    SharedString::from(format!("{kind}:{}:{pid}:{row_ix}", pid.len()))
}

#[cfg(test)]
mod tests {
    use super::plugin_source_row_id;

    #[test]
    fn plugin_source_row_ids_do_not_collide_across_plugins() {
        let a0 = plugin_source_row_id("pl-src-open", "iris.webdav", 0);
        let b0 = plugin_source_row_id("pl-src-open", "community.baidupan", 0);
        let c0 = plugin_source_row_id("pl-src-open", "a:b", 0);
        let d0 = plugin_source_row_id("pl-src-open", "a", 0);
        assert_ne!(a0, b0);
        assert_ne!(c0, d0);
        assert_ne!(
            plugin_source_row_id("pl-src-open", "iris.webdav", 0),
            plugin_source_row_id("pl-src-open", "iris.webdav", 1)
        );
    }
}
