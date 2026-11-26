//! Localized labels for built-in workspace presets and sidebar modes.

use iris_core::preset::{PresetManager, SidebarMode, WorkspacePreset};

const DEFAULT_PRESET_NAME: &str = "默认工作区";
pub(in crate::ui) const DEFAULT_CUSTOM_PRESET_NAME: &str = "自定义工作区";

pub(in crate::ui) fn sidebar_mode_label(mode: SidebarMode) -> &'static str {
    match mode {
        SidebarMode::Files => "目录",
        SidebarMode::Sources => "数据源",
        SidebarMode::Queue => "队列",
        SidebarMode::Plugins => "插件",
    }
}

pub(in crate::ui) fn preset_name(preset: &WorkspacePreset) -> &str {
    if preset.builtin {
        return match preset.id.as_str() {
            "default" => DEFAULT_PRESET_NAME,
            "cinema" => "沉浸观影",
            "review" => "审片与选片",
            "study" => "精听与学习",
            "pip" => "小窗置顶",
            _ => {
                let name = preset.name.trim();
                if name.is_empty() { &preset.id } else { name }
            }
        };
    }
    let name = preset.name.trim();
    if name.is_empty() {
        DEFAULT_CUSTOM_PRESET_NAME
    } else {
        name
    }
}

pub(in crate::ui) fn preset_name_by_id<'a>(presets: &'a PresetManager, id: &str) -> &'a str {
    presets.get(id).map_or(DEFAULT_PRESET_NAME, preset_name)
}

pub(in crate::ui) fn preset_description(preset: &WorkspacePreset) -> &str {
    if preset.builtin {
        return match preset.id.as_str() {
            "default" => "回到个人日常基线布局与播放偏好",
            "cinema" => "收起侧栏、悬浮控制栏、OLED 暗房主题并自动连播下一集",
            "review" => "右侧宽目录、开启照片缩略图条、胶片暖灰主题并开启三分构图线",
            "study" => "左侧插件面板就绪、3 秒精细回退、1.25× 大字幕",
            "pip" => "560×320 迷你窗口、自动置顶、隐藏冗余按钮与目录栏",
            _ => preset.description.trim(),
        };
    }
    preset.description.trim()
}
