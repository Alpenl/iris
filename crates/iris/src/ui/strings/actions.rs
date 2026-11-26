//! Localized labels for keymap schemes, action categories, bindable actions,
//! reserved shortcuts, and conflict descriptions.

use iris_core::keymap::{ActionCategory, ConflictTarget, KeymapScheme};
use iris_core::preset::PresetManager;

use super::chrome::{MEDIA_INFO_LABEL, OPEN_STREAM_LABEL};
use super::presets::preset_name;

pub(in crate::ui) fn keymap_scheme_label(scheme: KeymapScheme) -> &'static str {
    match scheme {
        KeymapScheme::IrisDefault => "Iris 默认",
        KeymapScheme::MpvClassic => "mpv 经典",
        KeymapScheme::PotPlayer => "PotPlayer 风格",
    }
}

pub(in crate::ui) fn keymap_scheme_description(scheme: KeymapScheme) -> &'static str {
    match scheme {
        KeymapScheme::IrisDefault => "兼顾视频播放、逐帧审片与照片浏览的现代默认键位",
        KeymapScheme::MpvClassic => "沿用 mpv 9/0 音量、[/] 倍速、</> 切集与 S 截图习惯",
        KeymapScheme::PotPlayer => "沿用 PotPlayer Z/X/C 倍速、D/F 逐帧、Enter 全屏与 F5 设置习惯",
    }
}

pub(in crate::ui) fn action_category_label(category: ActionCategory) -> &'static str {
    match category {
        ActionCategory::Playback => "播放与进度",
        ActionCategory::AudioSub => "音频与字幕",
        ActionCategory::View => "画面与变换",
        ActionCategory::Photo => "照片浏览",
        ActionCategory::Files => "列表与文件",
        ActionCategory::Window => "窗口与工作区",
    }
}

pub(crate) fn action_title(id: &str) -> &'static str {
    match id {
        "toggle_pause" => "播放 / 暂停",
        "seek_back" => "快退",
        "seek_forward" => "快进",
        "seek_back_large" => "大幅快退",
        "seek_forward_large" => "大幅快进",
        "frame_back" => "上一帧",
        "frame_forward" => "下一帧",
        "speed_down" => "减速播放",
        "speed_up" => "加速播放",
        "reset_speed" => "恢复原速 (1.0×)",
        "toggle_loop" => "切换单曲循环",
        "cycle_ab_loop" => "设置 A / B 点或清除区间",
        "clear_ab_loop" => "清除 A-B 循环",
        "prev_chapter" => "上一章节",
        "next_chapter" => "下一章节",
        "volume_up" => "增大音量",
        "volume_down" => "减小音量",
        "toggle_mute" => "静音 / 取消静音",
        "toggle_subs" => "显示 / 隐藏字幕",
        "cycle_sub" => "切换字幕轨道",
        "sub_seek_prev" => "跳到上一句字幕",
        "sub_seek_next" => "跳到下一句字幕",
        "sub_delay_back" => "字幕提前 (-0.1s)",
        "sub_delay_forward" => "字幕延后 (+0.1s)",
        "sub_delay_reset" => "重置字幕偏移",
        "cycle_audio" => "切换音轨",
        "cycle_aspect" => "切换画面比例",
        "rotate_cw" => "顺时针旋转 90°",
        "rotate_ccw" => "逆时针旋转 90°",
        "toggle_hflip" => "水平镜像翻转",
        "toggle_vflip" => "垂直镜像翻转",
        "photo_prev" => "上一张照片",
        "photo_next" => "下一张照片",
        "toggle_image_size" => "切换 1:1 原始尺寸",
        "fit_image" => "适应窗口大小",
        "zoom_in" => "放大照片",
        "zoom_out" => "缩小照片",
        "toggle_filmstrip" => "显示 / 隐藏缩略图条",
        "open_prev" => "上一个媒体",
        "open_next" => "下一个媒体",
        "open_file" => "打开文件或文件夹…",
        "open_url" => OPEN_STREAM_LABEL,
        "refresh_directory" => "刷新目录",
        "toggle_search" => "搜索目录",
        "toggle_favorite" => "收藏 / 取消收藏",
        "take_screenshot" => "保存当前帧截图",
        "toggle_directory" => "展开 / 收起侧栏",
        "toggle_fullscreen" => "切换全屏",
        "toggle_always_on_top" => "切换窗口置顶",
        "toggle_info" => MEDIA_INFO_LABEL,
        "open_palette" => "打开命令面板…",
        "open_settings" => "打开设置…",
        "cycle_preset" => "切换工作区预设",
        "quit" => "退出 Iris",
        _ => "快捷键操作",
    }
}

fn reserved_shortcut_label(action_id: &str) -> &'static str {
    match action_id {
        "toggle_info" => "媒体详情（固定快捷键）",
        "open_palette" => "命令面板（固定快捷键）",
        "exit_fullscreen" => "退出全屏（固定快捷键）",
        "reset_appearance" => "恢复默认外观（固定快捷键）",
        _ => "固定快捷键",
    }
}

pub(in crate::ui) fn describe_conflict(target: &ConflictTarget, presets: &PresetManager) -> String {
    match target {
        ConflictTarget::Reserved(id) => reserved_shortcut_label(id).to_string(),
        ConflictTarget::Action(id) => action_title(id).to_string(),
        ConflictTarget::Preset(id) => {
            let name = presets.get(id).map_or(id.as_str(), preset_name);
            format!("工作区预设 ({name})")
        }
        ConflictTarget::Plugin(plugin_ref) => format!("插件命令 ({plugin_ref})"),
        ConflictTarget::Other(id) => id.clone(),
    }
}
