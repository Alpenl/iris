//! Localized playback chrome labels, capability badges, and canonical UI terminology.

use iris_api::v2::Capability;
use iris_core::player::{AspectMode, AudioDevice, Track};

pub(crate) const OPEN_STREAM_LABEL: &str = "打开网络流…";
pub(in crate::ui) const OPEN_STREAM_DIALOG_TITLE: &str = "打开网络流";
pub(crate) const MEDIA_INFO_LABEL: &str = "媒体详情";
pub(crate) const RESET_APPEARANCE_LABEL: &str = "恢复默认外观";
pub(in crate::ui) const DEFAULT_PLUGIN_MEDIA_TITLE: &str = "插件媒体";

pub(in crate::ui) fn aspect_label(mode: AspectMode) -> &'static str {
    match mode {
        AspectMode::Original => "原始比例",
        AspectMode::Ratio16x9 => "16:9",
        AspectMode::Ratio4x3 => "4:3",
        AspectMode::Ratio21x9 => "21:9",
        AspectMode::FillCrop => "铺满裁剪",
    }
}

pub(in crate::ui) fn track_label(track: &Track) -> String {
    let mut parts = Vec::new();
    if !track.title.is_empty() {
        parts.push(track.title.clone());
    }
    if !track.lang.is_empty()
        && !track
            .title
            .to_ascii_lowercase()
            .contains(&track.lang.to_ascii_lowercase())
    {
        parts.push(track.lang.clone());
    }
    if !track.codec.is_empty() {
        parts.push(track.codec.to_ascii_uppercase());
    }
    if parts.is_empty() {
        parts.push(format!("轨道 {}", track.id));
    }
    let mut text = parts.join(" · ");
    if track.external {
        text.push_str(" [外挂]");
    }
    text
}

pub(in crate::ui) fn audio_device_label(dev: &AudioDevice, verbose_auto: bool) -> String {
    if dev.name == "auto" {
        return if verbose_auto {
            "自动选择（跟随系统默认）"
        } else {
            "自动选择设备"
        }
        .to_string();
    }
    let desc = dev.description.trim();
    if desc.is_empty() {
        dev.name.clone()
    } else {
        desc.to_string()
    }
}

pub(in crate::ui) fn chapter_title(index: usize, raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        format!("第 {} 章", index + 1)
    } else {
        trimmed.to_string()
    }
}

pub(in crate::ui) fn capability_label(cap: Capability) -> &'static str {
    match cap {
        Capability::ReadState => "读取播放状态与媒体身份",
        Capability::ControlPlayback => "控制播放",
        Capability::MediaSource => "媒体数据源",
        Capability::UrlResolver => "解析媒体网址",
        Capability::MediaEnhance => "添加媒体增强",
        Capability::Annotations => "编辑已授权的标注",
        Capability::Secrets => "私有凭据",
        Capability::Notifications => "通知",
        Capability::UiSidebar => "侧栏",
        Capability::UiStatus => "状态栏",
        Capability::UiOverlay => "画面叠加",
        Capability::UiToolbar => "工具栏",
        Capability::UiModal => "插件表单",
        Capability::UiTheme => "主题与布局",
        Capability::NetHttp => "HTTP 网络",
        Capability::NetWs => "WebSocket 网络",
        Capability::NetTcp => "TCP 网络",
    }
}
