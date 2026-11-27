//! In-window centered modal settings center (`SettingsPanel`) built on
//! `kit::modal::modal_shell` with a 7-tab left navigation rail.

mod appearance;
mod playback_and_view;
mod presets_and_plugins;
mod shortcuts;

use std::rc::Rc;

use gpui::{
    AnyElement, App, Entity, InteractiveElement, IntoElement, KeyContext, MouseButton,
    ParentElement, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use super::AppModel;
use super::kit::list_row::select_row;
use super::kit::modal::{ModalCallback, ModalShellSpec, modal_shell};
use super::state::SettingsTab;
use super::views::{DirtyRegions, model_listener};
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

pub(in crate::ui) fn render_settings_entity(
    model: &Entity<AppModel>,
    window: &Window,
    cx: &App,
) -> impl IntoElement {
    let m = model.read(cx);
    let t = ThemeTokens::of(cx);
    let vp = window.viewport_size();
    let modal_w = (f32::from(vp.width) - 24.).clamp(0., 700.);
    let modal_h = (f32::from(vp.height) - 24.).clamp(0., 532.);
    let is_recording = m.overlays.recording_shortcut.is_some();
    let cur_tab = m.overlays.settings_tab;
    let recorder_focus = m.text_inputs.recorder_focus.clone();
    let header_hint = m.key_hint("open_settings");

    let page: AnyElement = match cur_tab {
        SettingsTab::Appearance => appearance::settings_appearance(model, t, cx).into_any_element(),
        SettingsTab::Presets => {
            presets_and_plugins::settings_presets(model, t, cx).into_any_element()
        }
        SettingsTab::Playback => {
            playback_and_view::settings_playback(model, t, cx).into_any_element()
        }
        SettingsTab::Tracks => playback_and_view::settings_tracks(model, t, cx).into_any_element(),
        SettingsTab::View => playback_and_view::settings_view(model, t, cx).into_any_element(),
        SettingsTab::Plugins => {
            presets_and_plugins::settings_plugins(model, t, cx).into_any_element()
        }
        SettingsTab::Shortcuts => shortcuts::settings_shortcuts(model, t, cx).into_any_element(),
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
            let active = cur_tab == tab;
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
            .on_click(model_listener(model, move |this, _, w, cx| {
                let was_recording = this.overlays.recording_shortcut.take().is_some();
                if was_recording {
                    this.focus.focus(w);
                }
                this.overlays.settings_tab = tab;
                this.notify_regions(
                    DirtyRegions {
                        root: was_recording,
                        settings: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
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
            .track_focus(&recorder_focus)
            .key_context(kc)
            .on_key_down(model_listener(
                model,
                AppModel::handle_shortcut_recorder_key,
            ));
    }

    let cancel_cb: ModalCallback<AppModel> = Rc::new(|this, w, cx| {
        this.overlays.recording_shortcut = None;
        this.overlays.settings_open = false;
        this.focus.focus(w);
        this.bump_activity(cx);
        this.notify_regions(
            DirtyRegions {
                root: true,
                settings: true,
                ..DirtyRegions::NONE
            },
            cx,
        );
    });

    modal_shell(
        model,
        ModalShellSpec {
            id_prefix: "settings-modal",
            title: "设置".into(),
            header_hint,
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
    )
}
