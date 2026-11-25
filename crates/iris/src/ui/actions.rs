//! GPUI action declarations, runtime `Keymap` binding with `!TextInput` firewall,
//! dynamic `key_hint` lookup, and `#root` action listener registration.

use gpui::{
    Action, App, Context, Div, InteractiveElement, KeyBinding, SharedString, Stateful, Window,
    actions,
};
use iris_core::keymap::{self, Keymap};

use super::IrisWindow;
use crate::platform;

actions!(
    iris,
    [
        TogglePause,
        SeekBackwardExact,
        SeekForwardExact,
        SeekBackwardLarge,
        SeekForwardLarge,
        FrameBack,
        FrameForward,
        VolumeUp,
        VolumeDown,
        ToggleMute,
        SpeedDown,
        SpeedUp,
        ResetSpeed,
        ToggleLoop,
        CycleAbLoop,
        ClearAbLoop,
        ToggleDirectory,
        ToggleSearch,
        RefreshDirectory,
        ToggleFullscreen,
        ExitFullscreen,
        OpenSettings,
        OpenCommandPalette,
        ToggleAlwaysOnTop,
        ToggleInfo,
        TakeScreenshot,
        CycleAspect,
        ToggleSubs,
        CycleSub,
        SubSeekPrev,
        SubSeekNext,
        SubDelayBack,
        SubDelayForward,
        SubDelayReset,
        CycleAudio,
        PrevChapter,
        NextChapter,
        ToggleImageSize,
        FitImage,
        ZoomIn,
        ZoomOut,
        RotateCw,
        RotateCcw,
        ToggleHFlip,
        ToggleVFlip,
        ToggleFilmstrip,
        OpenNextMedia,
        OpenPreviousMedia,
        OpenFile,
        OpenStreamUrl,
        CyclePreset,
        ResetAppearance,
        ToggleFavorite,
        Quit
    ]
);

#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = iris, no_json)]
pub(super) struct ActivatePreset {
    pub(super) id: String,
}

#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = iris, no_json)]
pub(super) struct TriggerPluginCommand {
    pub(super) plugin_id: String,
    pub(super) command_id: String,
}

fn push_action_binding(out: &mut Vec<KeyBinding>, action_id: &str, key: &str, ctx: &'static str) {
    if key.is_empty() {
        return;
    }
    if gpui::Keystroke::parse(key).is_err() {
        log::warn!("skipping invalid keystroke '{key}' for action '{action_id}'");
        return;
    }
    let b = match action_id {
        "toggle_pause" => KeyBinding::new(key, TogglePause, Some(ctx)),
        "seek_back" => KeyBinding::new(key, SeekBackwardExact, Some(ctx)),
        "seek_forward" => KeyBinding::new(key, SeekForwardExact, Some(ctx)),
        "seek_back_large" => KeyBinding::new(key, SeekBackwardLarge, Some(ctx)),
        "seek_forward_large" => KeyBinding::new(key, SeekForwardLarge, Some(ctx)),
        "frame_back" => KeyBinding::new(key, FrameBack, Some(ctx)),
        "frame_forward" => KeyBinding::new(key, FrameForward, Some(ctx)),
        "volume_up" => KeyBinding::new(key, VolumeUp, Some(ctx)),
        "volume_down" => KeyBinding::new(key, VolumeDown, Some(ctx)),
        "toggle_mute" => KeyBinding::new(key, ToggleMute, Some(ctx)),
        "speed_down" => KeyBinding::new(key, SpeedDown, Some(ctx)),
        "speed_up" => KeyBinding::new(key, SpeedUp, Some(ctx)),
        "reset_speed" => KeyBinding::new(key, ResetSpeed, Some(ctx)),
        "toggle_loop" => KeyBinding::new(key, ToggleLoop, Some(ctx)),
        "cycle_ab_loop" => KeyBinding::new(key, CycleAbLoop, Some(ctx)),
        "clear_ab_loop" => KeyBinding::new(key, ClearAbLoop, Some(ctx)),
        "toggle_directory" => KeyBinding::new(key, ToggleDirectory, Some(ctx)),
        "toggle_search" => KeyBinding::new(key, ToggleSearch, Some(ctx)),
        "refresh_directory" => KeyBinding::new(key, RefreshDirectory, Some(ctx)),
        "toggle_fullscreen" => KeyBinding::new(key, ToggleFullscreen, Some(ctx)),
        "open_settings" => KeyBinding::new(key, OpenSettings, Some(ctx)),
        "open_palette" => KeyBinding::new(key, OpenCommandPalette, Some(ctx)),
        "toggle_always_on_top" => KeyBinding::new(key, ToggleAlwaysOnTop, Some(ctx)),
        "toggle_info" => KeyBinding::new(key, ToggleInfo, Some(ctx)),
        "take_screenshot" => KeyBinding::new(key, TakeScreenshot, Some(ctx)),
        "cycle_aspect" => KeyBinding::new(key, CycleAspect, Some(ctx)),
        "toggle_subs" => KeyBinding::new(key, ToggleSubs, Some(ctx)),
        "cycle_sub" => KeyBinding::new(key, CycleSub, Some(ctx)),
        "sub_seek_prev" => KeyBinding::new(key, SubSeekPrev, Some(ctx)),
        "sub_seek_next" => KeyBinding::new(key, SubSeekNext, Some(ctx)),
        "sub_delay_back" => KeyBinding::new(key, SubDelayBack, Some(ctx)),
        "sub_delay_forward" => KeyBinding::new(key, SubDelayForward, Some(ctx)),
        "sub_delay_reset" => KeyBinding::new(key, SubDelayReset, Some(ctx)),
        "cycle_audio" => KeyBinding::new(key, CycleAudio, Some(ctx)),
        "prev_chapter" => KeyBinding::new(key, PrevChapter, Some(ctx)),
        "next_chapter" => KeyBinding::new(key, NextChapter, Some(ctx)),
        "photo_prev" | "open_prev" => KeyBinding::new(key, OpenPreviousMedia, Some(ctx)),
        "photo_next" | "open_next" => KeyBinding::new(key, OpenNextMedia, Some(ctx)),
        "toggle_image_size" => KeyBinding::new(key, ToggleImageSize, Some(ctx)),
        "fit_image" => KeyBinding::new(key, FitImage, Some(ctx)),
        "zoom_in" => KeyBinding::new(key, ZoomIn, Some(ctx)),
        "zoom_out" => KeyBinding::new(key, ZoomOut, Some(ctx)),
        "rotate_cw" => KeyBinding::new(key, RotateCw, Some(ctx)),
        "rotate_ccw" => KeyBinding::new(key, RotateCcw, Some(ctx)),
        "toggle_hflip" => KeyBinding::new(key, ToggleHFlip, Some(ctx)),
        "toggle_vflip" => KeyBinding::new(key, ToggleVFlip, Some(ctx)),
        "toggle_filmstrip" => KeyBinding::new(key, ToggleFilmstrip, Some(ctx)),
        "open_file" => KeyBinding::new(key, OpenFile, Some(ctx)),
        "open_url" => KeyBinding::new(key, OpenStreamUrl, Some(ctx)),
        "cycle_preset" => KeyBinding::new(key, CyclePreset, Some(ctx)),
        "toggle_favorite" => KeyBinding::new(key, ToggleFavorite, Some(ctx)),
        "quit" => KeyBinding::new(key, Quit, Some(ctx)),
        "exit_fullscreen" => KeyBinding::new(key, ExitFullscreen, Some(ctx)),
        "reset_appearance" => KeyBinding::new(key, ResetAppearance, Some(ctx)),
        _ => return,
    };
    out.push(b);
}

/// Clears and re-applies all active keyboard bindings from `keymap` at runtime.
pub(crate) fn rebind_keys(cx: &mut App, keymap: &Keymap) {
    cx.clear_key_bindings();
    cx.bind_keys(runtime_bindings(keymap));
}

pub(super) fn runtime_bindings(keymap: &Keymap) -> Vec<KeyBinding> {
    let mut bindings = Vec::with_capacity(96);

    for entry in keymap::actions() {
        let key = keymap.effective_key(entry.id);
        if keymap::reserved_shortcut(&key).is_some() {
            continue;
        }
        let Some(binding) = keymap::gpui_binding(&key) else {
            continue;
        };
        for &ctx in entry.gpui_contexts(&key) {
            push_action_binding(&mut bindings, entry.id, &binding, ctx);
        }
    }

    for (key, action) in keymap::RESERVED_SHORTCUTS {
        let context = match action {
            "exit_fullscreen" => "Workspace && !TextInput",
            "reset_appearance" => "Workspace || ShortcutRecorder",
            "open_palette" => "Workspace && !Modal",
            _ => "Workspace && !Modal",
        };
        push_action_binding(&mut bindings, action, key, context);
    }

    for (preset_id, key) in keymap.preset_bindings() {
        if keymap::reserved_shortcut(key).is_none()
            && let Some(binding) = keymap::gpui_binding(key)
            && gpui::Keystroke::parse(&binding).is_ok()
            && let Some(ctx) = keymap::effective_gpui_context("Workspace", key)
        {
            bindings.push(KeyBinding::new(
                &binding,
                ActivatePreset {
                    id: preset_id.to_string(),
                },
                Some(ctx),
            ));
        }
    }

    for (plugin_id, cmd_id, key) in keymap.plugin_bindings() {
        if keymap::reserved_shortcut(key).is_none()
            && let Some(binding) = keymap::gpui_binding(key)
            && gpui::Keystroke::parse(&binding).is_ok()
            && let Some(ctx) = keymap::effective_gpui_context("Workspace", key)
        {
            bindings.push(KeyBinding::new(
                &binding,
                TriggerPluginCommand {
                    plugin_id: plugin_id.to_string(),
                    command_id: cmd_id.to_string(),
                },
                Some(ctx),
            ));
        }
    }

    bindings
}

impl IrisWindow {
    /// Returns the live formatted shortcut label for `action_id` under the active `Keymap`.
    pub(super) fn key_hint(&self, action_id: &str) -> Option<SharedString> {
        let canonical = match action_id {
            "open_stream_url" => "open_url",
            other => other,
        };
        let raw = self.keymap.effective_key(canonical);
        if raw.is_empty() {
            return None;
        }
        let disp = keymap::format_keystroke_display(&raw, platform::IS_MACOS);
        (!disp.is_empty()).then(|| SharedString::from(disp))
    }

    pub(super) fn reset_appearance_action(
        &mut self,
        _: &ResetAppearance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reset_appearance(window, cx);
    }

    pub(super) fn bind_window_actions(
        &self,
        root: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        root.on_action(cx.listener(Self::toggle_pause))
            .on_action(cx.listener(Self::seek_back))
            .on_action(cx.listener(Self::seek_forward))
            .on_action(cx.listener(Self::seek_back_large))
            .on_action(cx.listener(Self::seek_forward_large))
            .on_action(cx.listener(Self::frame_back))
            .on_action(cx.listener(Self::frame_forward))
            .on_action(cx.listener(Self::volume_up))
            .on_action(cx.listener(Self::volume_down))
            .on_action(cx.listener(Self::toggle_mute))
            .on_action(cx.listener(Self::speed_down))
            .on_action(cx.listener(Self::speed_up))
            .on_action(cx.listener(Self::reset_speed))
            .on_action(cx.listener(Self::toggle_loop))
            .on_action(cx.listener(Self::cycle_ab_loop))
            .on_action(cx.listener(Self::clear_ab_loop))
            .on_action(cx.listener(Self::toggle_directory))
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(Self::refresh_directory))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::open_command_palette))
            .on_action(cx.listener(Self::toggle_always_on_top))
            .on_action(cx.listener(Self::toggle_info))
            .on_action(cx.listener(Self::take_screenshot))
            .on_action(cx.listener(Self::cycle_aspect))
            .on_action(cx.listener(Self::toggle_fullscreen))
            .on_action(cx.listener(Self::exit_fullscreen))
            .on_action(cx.listener(Self::toggle_subs))
            .on_action(cx.listener(Self::cycle_sub))
            .on_action(cx.listener(Self::sub_seek_prev))
            .on_action(cx.listener(Self::sub_seek_next))
            .on_action(cx.listener(Self::sub_delay_back))
            .on_action(cx.listener(Self::sub_delay_forward))
            .on_action(cx.listener(Self::sub_delay_reset))
            .on_action(cx.listener(Self::cycle_audio))
            .on_action(cx.listener(Self::prev_chapter))
            .on_action(cx.listener(Self::next_chapter))
            .on_action(cx.listener(Self::toggle_image_size))
            .on_action(cx.listener(Self::fit_image))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::rotate_cw))
            .on_action(cx.listener(Self::rotate_ccw))
            .on_action(cx.listener(Self::toggle_hflip))
            .on_action(cx.listener(Self::toggle_vflip))
            .on_action(cx.listener(Self::toggle_filmstrip))
            .on_action(cx.listener(Self::next_item))
            .on_action(cx.listener(Self::prev_item))
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::open_stream_url_action))
            .on_action(cx.listener(Self::cycle_preset_action))
            .on_action(cx.listener(Self::activate_preset_action))
            .on_action(cx.listener(Self::trigger_plugin_command_action))
            .on_action(cx.listener(Self::reset_appearance_action))
            .on_action(cx.listener(Self::toggle_favorite))
    }
}
