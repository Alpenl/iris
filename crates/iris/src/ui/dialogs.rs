//! Host built-in modal dialogs (`OpenUrl` and `SavePreset`) built on `kit::modal`
//! and `kit::text_field`, plus Tab field cycling across built-in and plugin modals.

use std::rc::Rc;

use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, Window, div, px};
use iris_api::Widget;

use super::IrisWindow;
use super::kit::form::checkbox;
use super::kit::modal::{ModalCallback, ModalShellSpec, modal_shell};
use super::kit::text_field::{FormEnterCallback, TextFieldKind, TextFieldSpec, TextInputState};
use super::state::BuiltinDialog;
use crate::theme::ThemeTokens;

impl IrisWindow {
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
        match &dialog {
            BuiltinDialog::OpenUrl => {
                self.set_form_value("url", "");
                self.text_inputs.active_form_key = Some("url".into());
                self.text_inputs.form_focus.focus(window);
            }
            BuiltinDialog::SavePreset { .. } => {
                self.set_form_value("preset_name", "");
                self.set_form_value("preset_desc", "");
                self.text_inputs.active_form_key = Some("preset_name".into());
                self.text_inputs.form_focus.focus(window);
            }
        }
        self.overlays.open_dialog(dialog);
        cx.notify();
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
        cx.notify();
    }

    pub(super) fn render_builtin_dialog(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.overlays.dialog.clone()?;
        let t = ThemeTokens::of(cx);
        let vp = window.viewport_size();
        let modal_w = (f32::from(vp.width) - 32.).clamp(320., 460.);
        let max_h = (f32::from(vp.height) - 32.).clamp(240., 520.);

        let cancel_cb: ModalCallback = Rc::new(|this, w, cx| {
            this.overlays.dialog = None;
            this.text_inputs.active_form_key = None;
            this.focus.focus(w);
            cx.notify();
        });

        let (title, submit_label, body, on_submit) = match dialog {
            BuiltinDialog::OpenUrl => {
                let submit: ModalCallback = Rc::new(|this, w, cx| {
                    let url = this.form_value("url").trim().to_string();
                    if url.is_empty() {
                        this.show_toast("请输入网络流或媒体地址", cx);
                        return;
                    }
                    this.overlays.dialog = None;
                    this.text_inputs.active_form_key = None;
                    this.focus.focus(w);
                    this.open_direct_stream_url(&url, None, cx);
                });
                let enter_cb: FormEnterCallback = submit.clone();
                let inp = self.render_text_field(
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
                            .text_size(px(11.))
                            .text_color(t.text_dim)
                            .child("支持 HTTP / HTTPS 视频、HLS (.m3u8)、音频与远程图片直链"),
                    )
                    .child(inp)
                    .into_any_element();
                ("打开网络流 / 远程媒体", "打开播放", content, submit)
            }
            BuiltinDialog::SavePreset { scope } => {
                let submit: ModalCallback = Rc::new(move |this, w, cx| {
                    let name = this.form_value("preset_name").to_string();
                    let desc = this.form_value("preset_desc").to_string();
                    this.overlays.dialog = None;
                    this.text_inputs.active_form_key = None;
                    this.focus.focus(w);
                    this.save_current_as_preset(name, desc, scope, w, cx);
                });
                let enter_cb: FormEnterCallback = submit.clone();
                let name_inp = self.render_text_field(
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
                let desc_inp = self.render_text_field(
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
                                    .text_size(px(11.))
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
                                    .text_size(px(11.))
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
                                    .text_size(px(11.))
                                    .text_color(t.text_dim)
                                    .child("包含配置范围"),
                            )
                            .child(checkbox(
                                "sp-layout",
                                scope.layout,
                                "界面布局与主题外观",
                                t,
                                cx.listener(move |this, _, _, cx| {
                                    let mut next = scope;
                                    next.layout = !next.layout;
                                    this.overlays.dialog =
                                        Some(BuiltinDialog::SavePreset { scope: next });
                                    cx.notify();
                                }),
                            ))
                            .child(checkbox(
                                "sp-playback",
                                scope.playback,
                                "播放倍速、画面比例与播完动作",
                                t,
                                cx.listener(move |this, _, _, cx| {
                                    let mut next = scope;
                                    next.playback = !next.playback;
                                    this.overlays.dialog =
                                        Some(BuiltinDialog::SavePreset { scope: next });
                                    cx.notify();
                                }),
                            ))
                            .child(checkbox(
                                "sp-plugins",
                                scope.plugins,
                                "当前插件启停组合",
                                t,
                                cx.listener(move |this, _, _, cx| {
                                    let mut next = scope;
                                    next.plugins = !next.plugins;
                                    this.overlays.dialog =
                                        Some(BuiltinDialog::SavePreset { scope: next });
                                    cx.notify();
                                }),
                            )),
                    )
                    .into_any_element();
                ("保存当前状态为工作区预设", "保存预设", content, submit)
            }
        };

        Some(modal_shell(
            ModalShellSpec::dialog(
                "builtin-dialog",
                title,
                modal_w,
                max_h,
                "取消",
                submit_label,
                body,
                cancel_cb,
                on_submit,
            ),
            t,
            cx,
        ))
    }
}
