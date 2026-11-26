//! Built-in shortcut schemes and static catalog of bindable player actions.

use serde::{Deserialize, Serialize};

use super::effective_gpui_context_for_os;

/// Built-in keyboard shortcut scheme families.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeymapScheme {
    #[default]
    IrisDefault,
    MpvClassic,
    PotPlayer,
}

impl KeymapScheme {
    pub const ALL: &'static [KeymapScheme] = &[
        KeymapScheme::IrisDefault,
        KeymapScheme::MpvClassic,
        KeymapScheme::PotPlayer,
    ];
}

/// Logical grouping for bindable core player actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionCategory {
    Playback,
    AudioSub,
    View,
    Photo,
    Files,
    Window,
}

/// Static metadata and scheme defaults for a core player action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionEntry {
    pub id: &'static str,
    pub category: ActionCategory,
    pub context: &'static str,
    pub default_key: &'static str,
    pub mpv_key: &'static str,
    pub pot_key: &'static str,
}

impl ActionEntry {
    pub fn key_for_scheme(&self, scheme: KeymapScheme) -> &'static str {
        match scheme {
            KeymapScheme::IrisDefault => self.default_key,
            KeymapScheme::MpvClassic => self.mpv_key,
            KeymapScheme::PotPlayer => self.pot_key,
        }
    }

    /// Returns all effective GPUI `KeyContext` predicates where this action should be active
    /// for `keystroke`, enforcing the `!TextInput` firewall on unmodified `Workspace` keys.
    pub fn gpui_contexts(&self, keystroke: &str) -> &'static [&'static str] {
        self.gpui_contexts_for_os(keystroke, cfg!(target_os = "macos"))
    }

    pub(super) fn gpui_contexts_for_os(
        &self,
        keystroke: &str,
        is_macos: bool,
    ) -> &'static [&'static str] {
        if keystroke.trim().is_empty() {
            return &[];
        }
        match self.id {
            "toggle_pause" | "seek_back" | "seek_forward" | "seek_back_large"
            | "seek_forward_large" | "volume_up" | "volume_down" | "toggle_mute" | "speed_down"
            | "speed_up" | "reset_speed" | "toggle_loop" | "cycle_ab_loop" | "clear_ab_loop" => {
                &["Viewport && media == video", "Viewport && media == audio"]
            }
            "rotate_cw" | "rotate_ccw" | "toggle_hflip" | "toggle_vflip" => {
                &["Viewport && media == video", "Viewport && media == image"]
            }
            "quit" | "open_settings" => {
                match effective_gpui_context_for_os(self.context, keystroke, is_macos) {
                    Some("Workspace && !Modal") => &["Workspace"],
                    Some("Workspace && !TextInput && !Modal") => {
                        &["Workspace && !TextInput && !Modal"]
                    }
                    _ => &[],
                }
            }
            _ => match effective_gpui_context_for_os(self.context, keystroke, is_macos) {
                Some("Viewport && media == video") => &["Viewport && media == video"],
                Some("Viewport && media == image") => &["Viewport && media == image"],
                Some("Viewport && media == audio") => &["Viewport && media == audio"],
                Some("Viewport") => &["Viewport"],
                Some("Workspace && !TextInput && !Modal") => &["Workspace && !TextInput && !Modal"],
                Some("Workspace && !Modal") => &["Workspace && !Modal"],
                _ => &[],
            },
        }
    }
}

const fn act(
    id: &'static str,
    category: ActionCategory,
    context: &'static str,
    default_key: &'static str,
    mpv_key: &'static str,
    pot_key: &'static str,
) -> ActionEntry {
    ActionEntry {
        id,
        category,
        context,
        default_key,
        mpv_key,
        pot_key,
    }
}

const C_PLAY: ActionCategory = ActionCategory::Playback;
const C_AV: ActionCategory = ActionCategory::AudioSub;
const C_VIEW: ActionCategory = ActionCategory::View;
const C_PHOTO: ActionCategory = ActionCategory::Photo;
const C_FILE: ActionCategory = ActionCategory::Files;
const C_WIN: ActionCategory = ActionCategory::Window;

const CTX_VID: &str = "Viewport && media == video";
const CTX_IMG: &str = "Viewport && media == image";
const CTX_VP: &str = "Viewport";
const CTX_WS: &str = "Workspace";
const CTX_WS_SAFE: &str = "Workspace && !TextInput";

/// Complete catalog of bindable core player actions.
pub const ACTIONS: &[ActionEntry] = &[
    // Playback & timeline
    act("toggle_pause", C_PLAY, CTX_VID, "space", "space", "space"),
    act("seek_back", C_PLAY, CTX_VID, "left", "left", "left"),
    act("seek_forward", C_PLAY, CTX_VID, "right", "right", "right"),
    act(
        "seek_back_large",
        C_PLAY,
        CTX_VID,
        "shift-left",
        "down",
        "alt-left",
    ),
    act(
        "seek_forward_large",
        C_PLAY,
        CTX_VID,
        "shift-right",
        "up",
        "alt-right",
    ),
    act("frame_back", C_PLAY, CTX_VID, ",", ",", "d"),
    act("frame_forward", C_PLAY, CTX_VID, ".", ".", "f"),
    act("speed_down", C_PLAY, CTX_VID, "[", "[", "x"),
    act("speed_up", C_PLAY, CTX_VID, "]", "]", "c"),
    act(
        "reset_speed",
        C_PLAY,
        CTX_VID,
        "backspace",
        "backspace",
        "z",
    ),
    act("toggle_loop", C_PLAY, CTX_VID, "l", "shift-l", "alt-l"),
    act(
        "cycle_ab_loop",
        C_PLAY,
        CTX_VID,
        "ctrl-shift-l",
        "ctrl-shift-l",
        "ctrl-shift-l",
    ),
    act(
        "clear_ab_loop",
        C_PLAY,
        CTX_VID,
        "ctrl-alt-shift-l",
        "ctrl-alt-shift-l",
        "ctrl-alt-shift-l",
    ),
    act(
        "prev_chapter",
        C_PLAY,
        CTX_VID,
        "shift-[",
        "pageup",
        "shift-pageup",
    ),
    act(
        "next_chapter",
        C_PLAY,
        CTX_VID,
        "shift-]",
        "pagedown",
        "shift-pagedown",
    ),
    // Audio & subtitles
    act("volume_up", C_AV, CTX_VID, "up", "0", "up"),
    act("volume_down", C_AV, CTX_VID, "down", "9", "down"),
    act("toggle_mute", C_AV, CTX_VID, "m", "m", "m"),
    act("toggle_subs", C_AV, CTX_VID, "v", "v", "alt-h"),
    act("cycle_sub", C_AV, CTX_VID, "j", "j", "l"),
    act(
        "sub_seek_prev",
        C_AV,
        CTX_VID,
        "shift-,",
        "ctrl-left",
        "shift-,",
    ),
    act(
        "sub_seek_next",
        C_AV,
        CTX_VID,
        "shift-.",
        "ctrl-right",
        "shift-.",
    ),
    act("sub_delay_back", C_AV, CTX_VID, "z", "z", ","),
    act("sub_delay_forward", C_AV, CTX_VID, "x", "x", "."),
    act(
        "sub_delay_reset",
        C_AV,
        CTX_VID,
        "shift-z",
        "ctrl-shift-z",
        "/",
    ),
    act("cycle_audio", C_AV, CTX_VID, "a", "3", "a"),
    // Video view & transform
    act("cycle_aspect", C_VIEW, CTX_VID, "c", "shift-a", "f6"),
    act("rotate_cw", C_VIEW, CTX_VID, "r", "alt-right", "alt-k"),
    act(
        "rotate_ccw",
        C_VIEW,
        CTX_VID,
        "shift-r",
        "alt-left",
        "alt-j",
    ),
    act("toggle_hflip", C_VIEW, CTX_VID, "h", "alt-h", "alt-m"),
    act("toggle_vflip", C_VIEW, CTX_VID, "shift-h", "alt-v", "alt-v"),
    // Photo viewer
    act("photo_prev", C_PHOTO, CTX_IMG, "left", "left", "left"),
    act("photo_next", C_PHOTO, CTX_IMG, "right", "right", "right"),
    act("toggle_image_size", C_PHOTO, CTX_IMG, "space", "1", "space"),
    act("fit_image", C_PHOTO, CTX_IMG, "0", "0", "0"),
    act("zoom_in", C_PHOTO, CTX_IMG, "=", "=", "="),
    act("zoom_out", C_PHOTO, CTX_IMG, "-", "-", "-"),
    act("toggle_filmstrip", C_PHOTO, CTX_IMG, "g", "g", "g"),
    // Files & list
    act("open_prev", C_FILE, CTX_VP, "pageup", "shift-,", "pageup"),
    act(
        "open_next",
        C_FILE,
        CTX_VP,
        "pagedown",
        "shift-.",
        "pagedown",
    ),
    act(
        "open_file",
        C_FILE,
        CTX_WS,
        "secondary-o",
        "secondary-o",
        "f3",
    ),
    act(
        "open_url",
        C_FILE,
        CTX_WS,
        "secondary-u",
        "secondary-u",
        "secondary-u",
    ),
    act(
        "refresh_directory",
        C_FILE,
        CTX_WS,
        "secondary-r",
        "secondary-r",
        "secondary-r",
    ),
    act(
        "toggle_search",
        C_FILE,
        CTX_WS,
        "secondary-f",
        "secondary-f",
        "secondary-f",
    ),
    act("toggle_favorite", C_FILE, CTX_VP, "s", "shift-f", "b"),
    act(
        "take_screenshot",
        C_FILE,
        CTX_WS,
        "secondary-s",
        "s",
        "secondary-e",
    ),
    // Window & workspace
    act("toggle_directory", C_WIN, CTX_WS_SAFE, "tab", "tab", "f7"),
    act("toggle_fullscreen", C_WIN, CTX_WS_SAFE, "f", "f", "enter"),
    act(
        "toggle_always_on_top",
        C_WIN,
        CTX_WS_SAFE,
        "t",
        "shift-t",
        "t",
    ),
    act("toggle_info", C_WIN, CTX_VP, "i", "i", "tab"),
    act(
        "open_palette",
        C_WIN,
        CTX_WS,
        "secondary-p",
        "secondary-p",
        "secondary-p",
    ),
    act(
        "open_settings",
        C_WIN,
        CTX_WS,
        "secondary-,",
        "secondary-,",
        "f5",
    ),
    act("cycle_preset", C_WIN, CTX_WS, "alt-w", "alt-w", "alt-w"),
    act("quit", C_WIN, CTX_WS, "secondary-q", "q", "secondary-q"),
];

pub fn actions() -> &'static [ActionEntry] {
    ACTIONS
}

pub fn find_action(id: &str) -> Option<&'static ActionEntry> {
    ACTIONS.iter().find(|a| a.id == id)
}
