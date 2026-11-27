//! Host built-in modal dialogs (`OpenUrl`, `SavePreset`, and `ConfirmUninstallPlugin`)
//! built on `kit::modal` and `kit::text_field`, plus Tab field cycling across modals.

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{AnyElement, App, Context, Entity, IntoElement, ParentElement, Styled, Window, div, px};
use iris_api::Widget;

use super::AppModel;
use super::kit::form::checkbox;
use super::kit::modal::{ModalCallback, ModalShellSpec, modal_shell};
use super::kit::text_field::{
    FormEnterCallback, TextFieldKind, TextFieldSpec, TextInputState, render_text_field,
};
use super::state::BuiltinDialog;
use super::views::{DirtyRegions, model_listener};
use crate::theme::ThemeTokens;

fn confirm_regions(key: &str) -> DirtyRegions {
    if key == "clear-queue" || key.starts_with("rm-bm:") {
        DirtyRegions::sidebar_only()
    } else if key.starts_with("del-preset:") || key == "clear-progress" {
        DirtyRegions::settings_only()
    } else if key.starts_with("rm-src:") {
        DirtyRegions {
            sidebar: true,
            settings: true,
            ..DirtyRegions::NONE
        }
    } else {
        DirtyRegions {
            settings: true,
            viewport: true,
            ..DirtyRegions::NONE
        }
    }
}

impl AppModel {
    pub(super) fn is_confirming(&self, key: &str) -> bool {
        self.overlays.is_confirming(key, Instant::now())
    }

    /// Two-click inline confirmation guard for destructive actions (3s auto-reset).
    pub(super) fn confirm_destructive(
        &mut self,
        key: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = key.into();
        let regions = confirm_regions(&key);
        let now = Instant::now();
        let ttl = Duration::from_secs(3);
        if self.overlays.confirm_or_arm(key.clone(), now, ttl) {
            self.notify_regions(regions, cx);
            return true;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ttl).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .overlays
                    .pending_confirm
                    .as_ref()
                    .is_some_and(|(k, until)| k == &key && Instant::now() >= *until)
                {
                    this.overlays.pending_confirm = None;
                    this.notify_regions(regions, cx);
                }
            });
        })
        .detach();
        self.notify_regions(regions, cx);
        false
    }

    pub(super) fn form_value(&self, key: &str) -> &str {
        self.text_inputs
            .form_fields
            .get(key)
            .map_or("", TextInputState::text)
    }

    pub(super) fn set_form_value(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.text_inputs
            .form_fields
            .insert(key.into(), TextInputState::with_text(value));
    }

    pub(super) fn open_builtin_dialog(
        &mut self,
        dialog: BuiltinDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_host_consent() || self.plugins.active_modal().is_some() {
            return;
        }
        match &dialog {
            BuiltinDialog::OpenUrl => {
                self.set_form_value("url", "");
                self.set_form_value("url-resolver", "auto");
                self.text_inputs.active_form_key = Some("url".into());
                self.text_inputs.form_focus.focus(window);
                self.overlays.open_dialog(dialog);
            }
            BuiltinDialog::SavePreset { .. } => {
                self.set_form_value("preset_name", "");
                self.set_form_value("preset_desc", "");
                self.text_inputs.active_form_key = Some("preset_name".into());
                self.text_inputs.form_focus.focus(window);
                self.overlays.open_dialog(dialog);
            }
            BuiltinDialog::ConfirmUninstallPlugin { .. } => {
                self.overlays.open_dialog_over_settings(dialog);
                self.text_inputs.active_form_key = None;
                self.focus.focus(window);
            }
        }
        self.notify_all(cx);
    }

    pub(super) fn cycle_form_field(
        &mut self,
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keys: Vec<String> = if let Some((pid, modal)) = self.plugins.active_modal() {
            modal
                .widgets
                .iter()
                .filter_map(|w| match w {
                    Widget::Input { id, .. } => Some(format!("modal:{pid}:{}:{id}", modal.id)),
                    _ => None,
                })
                .collect()
        } else if matches!(self.overlays.dialog, Some(BuiltinDialog::SavePreset { .. })) {
            vec!["preset_name".into(), "preset_desc".into()]
        } else {
            Vec::new()
        };
        if keys.len() < 2 {
            return;
        }
        let cur = self
            .text_inputs
            .active_form_key
            .as_ref()
            .and_then(|k| keys.iter().position(|x| x == k))
            .unwrap_or(0);
        let delta: isize = if reverse { -1 } else { 1 };
        let next = (cur as isize + delta).rem_euclid(keys.len() as isize) as usize;
        self.text_inputs.active_form_key = Some(keys[next].clone());
        self.text_inputs.form_focus.focus(window);
        self.notify_root(cx);
    }

    pub(super) fn submit_builtin_dialog(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.overlays.dialog.clone() else {
            return;
        };
        match dialog {
            BuiltinDialog::OpenUrl => {
                let url = self.form_value("url").trim().to_string();
                if url.is_empty() {
                    self.overlays.dialog_error = Some("请输入网络流或媒体地址".into());
                    self.notify_root(cx);
                    return;
                }
                let resolver = self.form_value("url-resolver").to_owned();
                if !self.open_user_url(&url, &resolver, cx) {
                    return;
                }
                self.overlays.dialog = None;
                self.overlays.dialog_error = None;
                self.text_inputs.active_form_key = None;
                self.focus.focus(w);
                self.notify_root(cx);
            }
            BuiltinDialog::SavePreset { scope } => {
                let name = self.form_value("preset_name").to_string();
                let desc = self.form_value("preset_desc").to_string();
                self.overlays.dialog = None;
                self.overlays.dialog_error = None;
                self.text_inputs.active_form_key = None;
                self.focus.focus(w);
                self.notify_root(cx);
                self.save_current_as_preset(name, desc, scope, w, cx);
            }
            BuiltinDialog::ConfirmUninstallPlugin {
                plugin_id,
                keep_data,
                ..
            } => {
                self.overlays.dialog = None;
                self.overlays.dialog_error = None;
                self.text_inputs.active_form_key = None;
                self.focus.focus(w);
                self.notify_root(cx);
                self.uninstall_plugin(&plugin_id, keep_data, cx);
            }
        }
    }
}

pub(super) fn render_builtin_dialog(
    model: &Entity<AppModel>,
    window: &Window,
    cx: &App,
) -> Option<AnyElement> {
    let m = model.read(cx);
    let dialog = m.overlays.dialog.clone()?;
    let dialog_error = m.overlays.dialog_error.clone();
    let t = ThemeTokens::of(cx);
    let vp = window.viewport_size();
    let modal_w = (f32::from(vp.width) - 32.).clamp(0., 460.);
    let max_h = (f32::from(vp.height) - 32.).clamp(0., 520.);

    let cancel_cb: ModalCallback<AppModel> = Rc::new(|this, w, cx| {
        this.overlays.dialog = None;
        this.overlays.dialog_error = None;
        this.text_inputs.active_form_key = None;
        this.focus.focus(w);
        this.notify_root(cx);
    });

    let submit_cb: ModalCallback<AppModel> = Rc::new(|this, w, cx| {
        this.submit_builtin_dialog(w, cx);
    });

    let (title, submit_label, body) = match dialog {
        BuiltinDialog::OpenUrl => {
            let enter_cb: FormEnterCallback<AppModel> = submit_cb.clone();
            let inp = render_text_field(
                model,
                TextFieldSpec {
                    id: "dlg-open-url".into(),
                    kind: TextFieldKind::Form {
                        key: "url".into(),
                        seed: None,
                        secret: false,
                        on_enter: Some(enter_cb),
                    },
                    placeholder: "https://example.com/live/stream.m3u8 或 video.mp4".into(),
                    height: px(30.),
                    leading_icon: None,
                    borderless: false,
                },
                Some(window),
                cx,
            );
            let content = div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(t.text_sm)
                        .text_color(t.text_dim)
                        .child("支持 HTTP / HTTPS 视频、HLS (.m3u8)、音频与远程图片直链"),
                )
                .child(inp)
                .child(super::plugin_runtime::sources::render_url_resolver_choice(
                    model, cx,
                ))
                .into_any_element();
            (
                super::strings::chrome::OPEN_STREAM_DIALOG_TITLE,
                "打开播放",
                content,
            )
        }
        BuiltinDialog::SavePreset { scope } => {
            let enter_cb: FormEnterCallback<AppModel> = submit_cb.clone();
            let name_inp = render_text_field(
                model,
                TextFieldSpec {
                    id: "dlg-preset-name".into(),
                    kind: TextFieldKind::Form {
                        key: "preset_name".into(),
                        seed: None,
                        secret: false,
                        on_enter: Some(enter_cb.clone()),
                    },
                    placeholder: "例如：夜间审片工作区".into(),
                    height: px(30.),
                    leading_icon: None,
                    borderless: false,
                },
                Some(window),
                cx,
            );
            let desc_inp = render_text_field(
                model,
                TextFieldSpec {
                    id: "dlg-preset-desc".into(),
                    kind: TextFieldKind::Form {
                        key: "preset_desc".into(),
                        seed: None,
                        secret: false,
                        on_enter: Some(enter_cb),
                    },
                    placeholder: "可选简短说明".into(),
                    height: px(30.),
                    leading_icon: None,
                    borderless: false,
                },
                Some(window),
                cx,
            );
            let content = div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(t.text_sm)
                                .text_color(t.text_muted)
                                .child("预设名称"),
                        )
                        .child(name_inp),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(t.text_sm)
                                .text_color(t.text_muted)
                                .child("备注说明"),
                        )
                        .child(desc_inp),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .pt(px(4.))
                        .child(
                            div()
                                .text_size(t.text_sm)
                                .text_color(t.text_dim)
                                .child("包含配置范围"),
                        )
                        .child(checkbox(
                            "sp-layout",
                            scope.layout,
                            "界面布局与主题外观",
                            t,
                            model_listener(model, move |this, _, _, cx| {
                                let mut next = scope;
                                next.layout = !next.layout;
                                this.overlays.dialog =
                                    Some(BuiltinDialog::SavePreset { scope: next });
                                this.notify_root(cx);
                            }),
                        ))
                        .child(checkbox(
                            "sp-playback",
                            scope.playback,
                            "播放倍速、画面比例与播完动作",
                            t,
                            model_listener(model, move |this, _, _, cx| {
                                let mut next = scope;
                                next.playback = !next.playback;
                                this.overlays.dialog =
                                    Some(BuiltinDialog::SavePreset { scope: next });
                                this.notify_root(cx);
                            }),
                        ))
                        .child(checkbox(
                            "sp-plugins",
                            scope.plugins,
                            "当前插件启停组合",
                            t,
                            model_listener(model, move |this, _, _, cx| {
                                let mut next = scope;
                                next.plugins = !next.plugins;
                                this.overlays.dialog =
                                    Some(BuiltinDialog::SavePreset { scope: next });
                                this.notify_root(cx);
                            }),
                        )),
                )
                .into_any_element();
            ("保存当前状态为工作区预设", "保存预设", content)
        }
        BuiltinDialog::ConfirmUninstallPlugin {
            plugin_id,
            plugin_name,
            keep_data,
        } => {
            let pid_toggle = plugin_id.clone();
            let name_toggle = plugin_name.clone();
            let desc = if keep_data {
                "将立即停止并移除该插件包；登录凭据与偏好设置将保留在本地，重新安装插件包时可由你选择是否恢复。"
            } else {
                "将立即停止该插件并彻底清除其安装包、登录凭据、本地状态与网络授权记录。"
            };
            let content = div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(t.text_md)
                        .text_color(t.text)
                        .child(format!("确定要删除插件「{plugin_name}」吗？")),
                )
                .child(
                    div()
                        .font_family(crate::theme::FONT_MONO)
                        .text_size(t.text_sm)
                        .text_color(t.text_muted)
                        .child(plugin_id),
                )
                .child(checkbox(
                    "uninstall-keep-data",
                    keep_data,
                    "保留登录凭据与偏好设置（供后续重装恢复）",
                    t,
                    model_listener(model, move |this, _, _, cx| {
                        this.overlays.dialog = Some(BuiltinDialog::ConfirmUninstallPlugin {
                            plugin_id: pid_toggle.clone(),
                            plugin_name: name_toggle.clone(),
                            keep_data: !keep_data,
                        });
                        this.notify_root(cx);
                    }),
                ))
                .child(
                    div()
                        .text_size(t.text_sm)
                        .text_color(t.text_dim)
                        .child(desc),
                )
                .into_any_element();
            ("删除插件", "确认删除", content)
        }
    };

    Some(modal_shell(
        model,
        ModalShellSpec::dialog(
            "builtin-dialog",
            title,
            modal_w,
            max_h,
            "取消",
            submit_label,
            body,
            cancel_cb,
            submit_cb,
        )
        .with_error(dialog_error),
        t,
    ))
}
