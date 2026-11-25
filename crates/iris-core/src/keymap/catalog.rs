//! Built-in shortcut schemes and static catalog of bindable player actions.

use serde::{Deserialize, Serialize};

use super::effective_gpui_context;

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

    pub fn label(self) -> &'static str {
        match self {
            KeymapScheme::IrisDefault => "Iris 默认",
            KeymapScheme::MpvClassic => "mpv 经典",
            KeymapScheme::PotPlayer => "PotPlayer 风格",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            KeymapScheme::IrisDefault => "兼顾视频播放、逐帧审片与照片浏览的现代默认键位",
            KeymapScheme::MpvClassic => "沿用 mpv 9/0 音量、[/] 倍速、</> 切集与 S 截图习惯",
            KeymapScheme::PotPlayer => {
                "沿用 PotPlayer Z/X/C 倍速、D/F 逐帧、Enter 全屏与 F5 设置习惯"
            }
        }
    }
}

/// Static metadata and scheme defaults for a core player action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionEntry {
    pub id: &'static str,
    pub title: &'static str,
    pub category: &'static str,
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
            "quit" | "open_settings" | "open_palette" => {
                match effective_gpui_context(self.context, keystroke) {
                    Some("Workspace && !Modal") => &["Workspace"],
                    Some("Workspace && !TextInput && !Modal") => {
                        &["Workspace && !TextInput && !Modal"]
                    }
                    _ => &[],
                }
            }
            _ => match effective_gpui_context(self.context, keystroke) {
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
    title: &'static str,
    category: &'static str,
    context: &'static str,
    default_key: &'static str,
    mpv_key: &'static str,
    pot_key: &'static str,
) -> ActionEntry {
    ActionEntry {
        id,
        title,
        category,
        context,
        default_key,
        mpv_key,
        pot_key,
    }
}

const C_PLAY: &str = "播放与进度";
const C_AV: &str = "音频与字幕";
const C_VIEW: &str = "画面与变换";
const C_PHOTO: &str = "照片浏览";
const C_FILE: &str = "列表与文件";
const C_WIN: &str = "窗口与工作区";

const CTX_VID: &str = "Viewport && media == video";
const CTX_IMG: &str = "Viewport && media == image";
const CTX_VP: &str = "Viewport";
const CTX_WS: &str = "Workspace";
const CTX_WS_SAFE: &str = "Workspace && !TextInput";

/// Complete catalog of bindable core player actions.
pub const ACTIONS: &[ActionEntry] = &[
    // 播放与进度
    act(
        "toggle_pause",
        "播放 / 暂停",
        C_PLAY,
        CTX_VID,
        "space",
        "space",
        "space",
    ),
    act("seek_back", "快退", C_PLAY, CTX_VID, "left", "left", "left"),
    act(
        "seek_forward",
        "快进",
        C_PLAY,
        CTX_VID,
        "right",
        "right",
        "right",
    ),
    act(
        "seek_back_large",
        "大幅快退",
        C_PLAY,
        CTX_VID,
        "shift-left",
        "down",
        "alt-left",
    ),
    act(
        "seek_forward_large",
        "大幅快进",
        C_PLAY,
        CTX_VID,
        "shift-right",
        "up",
        "alt-right",
    ),
    act("frame_back", "上一帧", C_PLAY, CTX_VID, ",", ",", "d"),
    act("frame_forward", "下一帧", C_PLAY, CTX_VID, ".", ".", "f"),
    act("speed_down", "减速播放", C_PLAY, CTX_VID, "[", "[", "x"),
    act("speed_up", "加速播放", C_PLAY, CTX_VID, "]", "]", "c"),
    act(
        "reset_speed",
        "恢复原速 (1.0×)",
        C_PLAY,
        CTX_VID,
        "backspace",
        "backspace",
        "z",
    ),
    act(
        "toggle_loop",
        "切换单曲循环",
        C_PLAY,
        CTX_VID,
        "l",
        "shift-l",
        "alt-l",
    ),
    act(
        "cycle_ab_loop",
        "设置 A / B 点或清除区间",
        C_PLAY,
        CTX_VID,
        "ctrl-shift-l",
        "ctrl-shift-l",
        "ctrl-shift-l",
    ),
    act(
        "clear_ab_loop",
        "清除 A-B 循环",
        C_PLAY,
        CTX_VID,
        "ctrl-alt-shift-l",
        "ctrl-alt-shift-l",
        "ctrl-alt-shift-l",
    ),
    act(
        "prev_chapter",
        "上一章节",
        C_PLAY,
        CTX_VID,
        "shift-[",
        "pageup",
        "shift-pageup",
    ),
    act(
        "next_chapter",
        "下一章节",
        C_PLAY,
        CTX_VID,
        "shift-]",
        "pagedown",
        "shift-pagedown",
    ),
    // 音频与字幕
    act("volume_up", "增大音量", C_AV, CTX_VID, "up", "0", "up"),
    act(
        "volume_down",
        "减小音量",
        C_AV,
        CTX_VID,
        "down",
        "9",
        "down",
    ),
    act(
        "toggle_mute",
        "静音 / 取消静音",
        C_AV,
        CTX_VID,
        "m",
        "m",
        "m",
    ),
    act(
        "toggle_subs",
        "显示 / 隐藏字幕",
        C_AV,
        CTX_VID,
        "v",
        "v",
        "alt-h",
    ),
    act("cycle_sub", "切换字幕轨道", C_AV, CTX_VID, "j", "j", "l"),
    act(
        "sub_seek_prev",
        "跳到上一句字幕",
        C_AV,
        CTX_VID,
        "shift-,",
        "ctrl-left",
        "shift-,",
    ),
    act(
        "sub_seek_next",
        "跳到下一句字幕",
        C_AV,
        CTX_VID,
        "shift-.",
        "ctrl-right",
        "shift-.",
    ),
    act(
        "sub_delay_back",
        "字幕提前 (-0.1s)",
        C_AV,
        CTX_VID,
        "z",
        "z",
        ",",
    ),
    act(
        "sub_delay_forward",
        "字幕延后 (+0.1s)",
        C_AV,
        CTX_VID,
        "x",
        "x",
        ".",
    ),
    act(
        "sub_delay_reset",
        "重置字幕偏移",
        C_AV,
        CTX_VID,
        "shift-z",
        "ctrl-shift-z",
        "/",
    ),
    act("cycle_audio", "切换音轨", C_AV, CTX_VID, "a", "3", "a"),
    // 画面与变换
    act(
        "cycle_aspect",
        "切换画面比例",
        C_VIEW,
        CTX_VID,
        "c",
        "shift-a",
        "f6",
    ),
    act(
        "rotate_cw",
        "顺时针旋转 90°",
        C_VIEW,
        CTX_VID,
        "r",
        "alt-right",
        "alt-k",
    ),
    act(
        "rotate_ccw",
        "逆时针旋转 90°",
        C_VIEW,
        CTX_VID,
        "shift-r",
        "alt-left",
        "alt-j",
    ),
    act(
        "toggle_hflip",
        "水平镜像翻转",
        C_VIEW,
        CTX_VID,
        "h",
        "alt-h",
        "alt-m",
    ),
    act(
        "toggle_vflip",
        "垂直镜像翻转",
        C_VIEW,
        CTX_VID,
        "shift-h",
        "alt-v",
        "alt-v",
    ),
    // 照片浏览
    act(
        "photo_prev",
        "上一张照片",
        C_PHOTO,
        CTX_IMG,
        "left",
        "left",
        "left",
    ),
    act(
        "photo_next",
        "下一张照片",
        C_PHOTO,
        CTX_IMG,
        "right",
        "right",
        "right",
    ),
    act(
        "toggle_image_size",
        "切换 1:1 原始尺寸",
        C_PHOTO,
        CTX_IMG,
        "space",
        "1",
        "space",
    ),
    act("fit_image", "适应窗口大小", C_PHOTO, CTX_IMG, "0", "0", "0"),
    act("zoom_in", "放大照片", C_PHOTO, CTX_IMG, "=", "=", "="),
    act("zoom_out", "缩小照片", C_PHOTO, CTX_IMG, "-", "-", "-"),
    act(
        "toggle_filmstrip",
        "显示 / 隐藏缩略图条",
        C_PHOTO,
        CTX_IMG,
        "g",
        "g",
        "g",
    ),
    // 列表与文件
    act(
        "open_prev",
        "上一个媒体",
        C_FILE,
        CTX_VP,
        "pageup",
        "shift-,",
        "pageup",
    ),
    act(
        "open_next",
        "下一个媒体",
        C_FILE,
        CTX_VP,
        "pagedown",
        "shift-.",
        "pagedown",
    ),
    act(
        "open_file",
        "打开文件或文件夹…",
        C_FILE,
        CTX_WS,
        "secondary-o",
        "secondary-o",
        "f3",
    ),
    act(
        "open_url",
        "打开网络流 / 远程地址…",
        C_FILE,
        CTX_WS,
        "secondary-u",
        "secondary-u",
        "secondary-u",
    ),
    act(
        "refresh_directory",
        "刷新目录",
        C_FILE,
        CTX_WS,
        "secondary-r",
        "secondary-r",
        "secondary-r",
    ),
    act(
        "toggle_search",
        "搜索目录",
        C_FILE,
        CTX_WS,
        "secondary-f",
        "secondary-f",
        "secondary-f",
    ),
    act(
        "toggle_favorite",
        "收藏 / 取消收藏",
        C_FILE,
        CTX_VP,
        "s",
        "shift-f",
        "b",
    ),
    act(
        "take_screenshot",
        "保存当前帧截图",
        C_FILE,
        CTX_WS,
        "secondary-s",
        "s",
        "secondary-e",
    ),
    // 窗口与工作区
    act(
        "toggle_directory",
        "展开 / 收起侧栏",
        C_WIN,
        CTX_WS_SAFE,
        "tab",
        "tab",
        "f7",
    ),
    act(
        "toggle_fullscreen",
        "切换全屏",
        C_WIN,
        CTX_WS_SAFE,
        "f",
        "f",
        "enter",
    ),
    act(
        "toggle_always_on_top",
        "切换窗口置顶",
        C_WIN,
        CTX_WS_SAFE,
        "t",
        "shift-t",
        "t",
    ),
    act(
        "toggle_info",
        "媒体详情与统计",
        C_WIN,
        CTX_VP,
        "i",
        "i",
        "tab",
    ),
    act(
        "open_palette",
        "打开命令面板",
        C_WIN,
        CTX_WS,
        "secondary-p",
        "secondary-p",
        "secondary-p",
    ),
    act(
        "open_settings",
        "打开设置",
        C_WIN,
        CTX_WS,
        "secondary-,",
        "secondary-,",
        "f5",
    ),
    act(
        "cycle_preset",
        "切换工作区布局预设",
        C_WIN,
        CTX_WS,
        "alt-w",
        "alt-w",
        "alt-w",
    ),
    act(
        "quit",
        "退出 Iris",
        C_WIN,
        CTX_WS,
        "secondary-q",
        "q",
        "secondary-q",
    ),
];

pub fn actions() -> &'static [ActionEntry] {
    ACTIONS
}

pub fn find_action(id: &str) -> Option<&'static ActionEntry> {
    ACTIONS.iter().find(|a| a.id == id)
}
