//! Interactive keyboard shortcut settings tab: 3 `KeymapScheme` switcher,
//! keystroke recorder interceptor, scope conflict alerts, and rebindable rows.

use gpui::{
    Context, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use iris_core::keymap::{self, KeymapScheme};

use super::super::IrisWindow;
use super::super::actions::rebind_keys;
use super::super::kit::form::{chip_button, segmented};
use super::super::kit::{badge_pill, section_label};
use crate::platform::IS_MACOS;
use crate::theme::{FONT_MONO, FONT_UI, ThemeTokens};

fn scope_short_label(entry: &keymap::ActionEntry) -> &'static str {
    let ctxs = entry.gpui_contexts(entry.default_key);
    let has_v = ctxs.iter().any(|c| c.contains("video"));
    let has_a = ctxs.iter().any(|c| c.contains("audio"));
    let has_i = ctxs.iter().any(|c| c.contains("image"));
    match (has_v, has_a, has_i) {
        (true, false, true) => "视频/看图",
        (true, true, false) => "视频/音频",
        (true, false, false) => "视频",
        (false, false, true) => "看图",
        _ if entry.context.contains("Viewport") => "视口",
        _ => "全局",
    }
}

impl IrisWindow {
    pub(super) fn handle_shortcut_recorder_key(
        &mut self,
        ev: &KeyDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.recording_shortcut.is_none() {
            return;
        }
        let mods = ev.keystroke.modifiers;
        let key_str = ev.keystroke.key.as_str();
        let Some(spec) = keymap::normalize_gpui_keystroke(
            key_str,
            mods.platform,
            mods.control,
            mods.alt,
            mods.shift,
        ) else {
            cx.stop_propagation();
            return;
        };
        if spec == "escape" {
            self.overlays.recording_shortcut = None;
            self.overlays.shortcut_conflict = None;
            self.focus.focus(w);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if keymap::reserved_shortcut(&spec).is_some_and(|(id, _)| id == "reset_appearance") {
            return;
        }
        let disp = keymap::format_keystroke_display(&spec, IS_MACOS);
        if let Some((target_id, ctx_pred)) = self.overlays.recording_shortcut.as_ref()
            && let Some(other) = self.keymap.find_conflict(ctx_pred, &spec, target_id)
        {
            let warn = format!("无法绑定 {disp}：与「{other}」冲突，请选择其他按键");
            self.overlays.shortcut_conflict = Some(warn.clone());
            self.show_error_toast(warn, cx);
        } else if let Some((target_id, _)) = self.overlays.recording_shortcut.take() {
            self.keymap.set_binding(&target_id, spec);
            rebind_keys(cx, &self.keymap);
            self.overlays.shortcut_conflict = None;
            self.show_toast(format!("已绑定快捷键 · {disp}"), cx);
            self.focus.focus(w);
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn settings_shortcuts(
        &self,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let cur_scheme = self.keymap.scheme;
        let has_any_overrides = !self.keymap.overrides.is_empty();
        let recording_id = self
            .overlays
            .recording_shortcut
            .as_ref()
            .map(|(id, _)| id.clone());
        let conflict_banner = self.overlays.shortcut_conflict.clone();

        let mut categories: Vec<(&'static str, Vec<&'static keymap::ActionEntry>)> = Vec::new();
        for entry in keymap::actions() {
            if let Some((_, list)) = categories.iter_mut().find(|(c, _)| *c == entry.category) {
                list.push(entry);
            } else {
                categories.push((entry.category, vec![entry]));
            }
        }

        let preset_rows: Vec<(String, String)> = self
            .presets
            .list()
            .iter()
            .map(|p| {
                (
                    format!("preset:{}", p.id),
                    format!("切换到预设 · {}", p.name),
                )
            })
            .collect();

        let plugin_rows: Vec<(String, String)> = self
            .plugins
            .commands()
            .into_iter()
            .map(|(pid, pname, spec)| {
                (
                    format!("plugin:{pid}:{}", spec.id),
                    format!("{pname} · {}", spec.title),
                )
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .pb(px(12.))
                    .border_b_1()
                    .border_color(t.line)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .text_size(t.text_md)
                                            .text_color(t.text)
                                            .child("快捷键方案与自定义录制"),
                                    )
                                    .child(
                                        div()
                                            .text_size(t.text_sm)
                                            .text_color(t.text_faint)
                                            .child(cur_scheme.description()),
                                    ),
                            )
                            .child(
                                chip_button(
                                    "reset-all-shortcuts",
                                    "全部恢复默认",
                                    has_any_overrides,
                                    t,
                                )
                                .on_click(cx.listener(
                                    |this, _, w, cx| {
                                        this.overlays.recording_shortcut = None;
                                        this.overlays.shortcut_conflict = None;
                                        this.keymap.reset_all();
                                        this.keymap.save();
                                        rebind_keys(cx, &this.keymap);
                                        this.focus.focus(w);
                                        this.show_toast("已将全部快捷键恢复为当前方案默认值", cx);
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                    .child({
                        let schemes: Vec<_> =
                            KeymapScheme::ALL.iter().map(|&s| (s.label(), s)).collect();
                        segmented(
                            "keymap-scheme",
                            &schemes,
                            cur_scheme,
                            t,
                            cx,
                            |this, scheme, cx| {
                                this.overlays.recording_shortcut = None;
                                this.overlays.shortcut_conflict = None;
                                this.keymap.set_scheme(scheme);
                                this.keymap.save();
                                rebind_keys(cx, &this.keymap);
                                this.show_toast(
                                    format!("已切换快捷键方案 · {}", scheme.label()),
                                    cx,
                                );
                                cx.notify();
                            },
                        )
                    }),
            )
            .when_some(recording_id.clone(), |d, rec| {
                d.child(
                    div()
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(t.radius_md)
                        .bg(t.selected)
                        .border_1()
                        .border_color(t.accent)
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.))
                        .child(div().text_size(t.text_sm).text_color(t.text).child(format!(
                            "正在录制「{rec}」：请直接按下目标按键或组合键，按 Esc 取消"
                        )))
                        .child(chip_button("cancel-rec-banner", "取消", false, t).on_click(
                            cx.listener(|this, _, w, cx| {
                                this.overlays.recording_shortcut = None;
                                this.focus.focus(w);
                                cx.notify();
                            }),
                        )),
                )
            })
            .when_some(conflict_banner, |d, msg| {
                d.child(
                    div()
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(t.radius_md)
                        .bg(t.window_bg)
                        .border_1()
                        .border_color(t.danger)
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.))
                        .child(div().text_size(t.text_sm).text_color(t.danger).child(msg))
                        .child(
                            chip_button("dismiss-conflict-banner", "知道了", false, t).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.overlays.shortcut_conflict = None;
                                    cx.notify();
                                }),
                            ),
                        ),
                )
            })
            .children({
                let cx_ref: &Context<Self> = cx;
                let rec_str = recording_id.as_deref();
                categories.into_iter().map(move |(cat_title, entries)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            section_label(cat_title, t)
                                .pb(px(4.))
                                .border_b_1()
                                .border_color(t.line),
                        )
                        .children(entries.into_iter().map(move |entry| {
                            self.render_shortcut_recorder_row(
                                entry.id.to_string(),
                                entry.title.to_string(),
                                Some(scope_short_label(entry)),
                                entry.context.to_string(),
                                rec_str,
                                t,
                                cx_ref,
                            )
                        }))
                })
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .pt(px(4.))
                    .child(
                        section_label("工作区预设快捷键", t)
                            .pb(px(4.))
                            .border_b_1()
                            .border_color(t.line),
                    )
                    .children(preset_rows.into_iter().map(|(target_id, title)| {
                        self.render_shortcut_recorder_row(
                            target_id,
                            title,
                            Some("预设"),
                            "Workspace".into(),
                            recording_id.as_deref(),
                            t,
                            cx,
                        )
                    })),
            )
            .when(!plugin_rows.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .pt(px(4.))
                        .child(
                            section_label("启用插件命令快捷键", t)
                                .pb(px(4.))
                                .border_b_1()
                                .border_color(t.line),
                        )
                        .children(plugin_rows.into_iter().map(|(target_id, title)| {
                            self.render_shortcut_recorder_row(
                                target_id,
                                title,
                                Some("插件"),
                                "Workspace".into(),
                                recording_id.as_deref(),
                                t,
                                cx,
                            )
                        })),
                )
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn render_shortcut_recorder_row(
        &self,
        target_id: String,
        title: String,
        scope_tag: Option<&'static str>,
        context_pred: String,
        active_recording: Option<&str>,
        t: ThemeTokens,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let is_recording = active_recording == Some(target_id.as_str());
        let is_overridden = self.keymap.is_overridden(&target_id);
        let effective = self.keymap.effective_key(&target_id);
        let key_text = if is_recording {
            "按下组合键…".to_string()
        } else if effective.is_empty() {
            "点击设置".to_string()
        } else {
            let formatted = keymap::format_keystroke_display(&effective, IS_MACOS);
            if formatted.is_empty() {
                effective
            } else {
                formatted
            }
        };

        let id_rec = target_id.clone();
        let id_reset = target_id.clone();
        let row_btn_id = SharedString::from(format!("sc-rec:{target_id}"));
        let reset_btn_id = SharedString::from(format!("sc-rst:{target_id}"));

        div()
            .min_h(t.row_h)
            .py(px(2.))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(t.text_md)
                            .text_color(if is_recording { t.text } else { t.text_muted })
                            .truncate()
                            .child(title),
                    )
                    .when_some(scope_tag, |d, tag| d.child(badge_pill(tag, false, t))),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .when(is_overridden, |d| {
                        d.child(
                            chip_button(reset_btn_id, "重置", false, t).on_click(cx.listener(
                                move |this, _, w, cx| {
                                    if this.overlays.recording_shortcut.as_ref().map(|(k, _)| k)
                                        == Some(&id_reset)
                                    {
                                        this.overlays.recording_shortcut = None;
                                        this.focus.focus(w);
                                    }
                                    this.keymap.reset_binding(&id_reset);
                                    this.keymap.save();
                                    rebind_keys(cx, &this.keymap);
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .child(
                        div()
                            .id(row_btn_id)
                            .h(t.control_h_sm)
                            .min_w(px(68.))
                            .px(px(8.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(t.radius_sm)
                            .cursor_pointer()
                            .font_family(if is_recording || key_text == "点击设置" {
                                FONT_UI
                            } else {
                                FONT_MONO
                            })
                            .text_size(t.text_sm)
                            .bg(if is_recording {
                                t.selected
                            } else {
                                t.window_bg
                            })
                            .border_1()
                            .border_color(if is_recording || is_overridden {
                                t.accent
                            } else {
                                t.line_strong
                            })
                            .text_color(if is_recording {
                                t.accent
                            } else if key_text == "点击设置" {
                                t.text_faint
                            } else {
                                t.text
                            })
                            .hover(|s| s.bg(t.hover).border_color(t.accent))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, w, cx| {
                                if is_recording {
                                    this.overlays.recording_shortcut = None;
                                    this.focus.focus(w);
                                } else {
                                    this.text_inputs.active_form_key = None;
                                    this.overlays.shortcut_conflict = None;
                                    this.overlays.recording_shortcut =
                                        Some((id_rec.clone(), context_pred.clone()));
                                    this.text_inputs.recorder_focus.focus(w);
                                }
                                cx.notify();
                            }))
                            .child(key_text),
                    ),
            )
    }
}
