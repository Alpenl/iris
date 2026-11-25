//! In-window centered modal settings center built on `kit::modal::modal_shell`
//! with a 7-tab left navigation rail.

mod appearance;
mod playback_and_view;
mod presets_and_plugins;
mod shortcuts;

use std::rc::Rc;

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, KeyContext, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use super::IrisWindow;
use super::kit::list_row::select_row;
use super::kit::modal::{ModalCallback, ModalShellSpec, modal_shell};
use super::state::SettingsTab;
use crate::theme::ThemeTokens;

const NAV_TABS: &[(SettingsTab, &str)] = &[
    (SettingsTab::Appearance, "外观与常规"),
    (SettingsTab::Presets, "工作区预设"),
    (SettingsTab::Playback, "播放"),
    (SettingsTab::Tracks, "字幕与音频"),
    (SettingsTab::View, "看图"),
    (SettingsTab::Plugins, "插件"),
    (SettingsTab::Shortcuts, "快捷键"),
];

impl IrisWindow {
    pub(super) fn render_settings_modal(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        let vp = window.viewport_size();
        let modal_w = (f32::from(vp.width) - 24.).clamp(0., 700.);
        let modal_h = (f32::from(vp.height) - 24.).clamp(0., 532.);
        let is_recording = self.overlays.recording_shortcut.is_some();

        let page: AnyElement = match self.overlays.settings_tab {
            SettingsTab::Appearance => self.settings_appearance(t, cx).into_any_element(),
            SettingsTab::Presets => self.settings_presets(t, cx).into_any_element(),
            SettingsTab::Playback => self.settings_playback(t, cx).into_any_element(),
            SettingsTab::Tracks => self.settings_tracks(t, cx).into_any_element(),
            SettingsTab::View => self.settings_view(t, cx).into_any_element(),
            SettingsTab::Plugins => self.settings_plugins(t, cx).into_any_element(),
            SettingsTab::Shortcuts => self.settings_shortcuts(t, cx).into_any_element(),
        };

        let nav_rail = div()
            .id("settings-nav-rail")
            .w(px(144.))
            .min_w(px(96.))
            .max_w(gpui::relative(0.32))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .py(px(8.))
            .bg(t.window_bg)
            .border_r_1()
            .border_color(t.line)
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(NAV_TABS.iter().map(|&(tab, label)| {
                let active = self.overlays.settings_tab == tab;
                select_row(
                    ("settings-tab", tab as usize),
                    label,
                    None,
                    active,
                    false,
                    false,
                    t,
                )
                .px(px(12.))
                .when(active, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left_0()
                            .top(px(5.))
                            .bottom(px(5.))
                            .w(px(2.))
                            .bg(t.accent),
                    )
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, w, cx| {
                    if this.overlays.recording_shortcut.take().is_some() {
                        this.focus.focus(w);
                    }
                    this.overlays.settings_tab = tab;
                    cx.notify();
                }))
            }));

        let mut content = div()
            .id("settings-body")
            .size_full()
            .flex()
            .child(nav_rail)
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .overflow_y_scroll()
                    .p(px(18.))
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(page),
            );

        if is_recording {
            let mut kc = KeyContext::default();
            kc.add("ShortcutRecorder");
            kc.add("TextInput");
            content = content
                .track_focus(&self.text_inputs.recorder_focus)
                .key_context(kc)
                .on_key_down(cx.listener(Self::handle_shortcut_recorder_key));
        }

        let cancel_cb: ModalCallback = Rc::new(|this, w, cx| {
            this.overlays.recording_shortcut = None;
            this.overlays.settings_open = false;
            this.focus.focus(w);
            this.bump_activity(cx);
            cx.notify();
        });

        modal_shell(
            ModalShellSpec {
                id_prefix: "settings-modal",
                title: "设置".into(),
                header_hint: self.key_hint("open_settings"),
                error: None,
                width: modal_w,
                fixed_height: Some(modal_h),
                max_height: modal_h,
                raw_body: true,
                accent_border: is_recording,
                body: content.into_any_element(),
                on_cancel: cancel_cb,
                footer: None,
            },
            t,
            cx,
        )
    }
}
