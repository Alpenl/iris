//! Localized user-facing error descriptions for mpv, player commands, and plugins.

use iris_api::v2::ErrorCode;
use iris_core::player::PlayerActionError;

pub(in crate::ui) fn mpv_error(code: i32) -> &'static str {
    match code {
        -13 => "无法打开或读取文件",
        -14 => "未识别的媒体格式",
        -9 | -10 => "不支持的解码格式或操作",
        -1 => "事件队列溢出",
        -2 => "内存不足",
        _ => "解码或播放失败",
    }
}

pub(in crate::ui) fn player_action_error(err: &anyhow::Error) -> &'static str {
    if let Some(action) = err.downcast_ref::<PlayerActionError>() {
        return match action {
            PlayerActionError::NotSeekable => "当前媒体不支持设置 A-B 循环",
            PlayerActionError::InvalidAbRange => "A-B 循环区间无效或超出时长范围",
            PlayerActionError::NoVisualMedia => "LUT 预览仅适用于已加载的视频或图片",
            PlayerActionError::InvalidSubtitlePath => "本地字幕不支持远程或内部协议路径",
            PlayerActionError::SubtitleFileNotFound => "字幕文件不存在或不可读取",
            PlayerActionError::NonUnicodePath => "文件路径含有无法识别的字符，无法交给播放器",
        };
    }
    "当前状态无法执行此操作"
}

pub(in crate::ui) fn plugin_error_code(code: ErrorCode) -> String {
    match code {
        ErrorCode::AuthFailed => "鉴权失败或播放源拒绝访问",
        ErrorCode::Denied => "播放地址或权限被策略拒绝",
        ErrorCode::QuotaExceeded => "插件资源配额不足",
        ErrorCode::Timeout => "请求超时",
        ErrorCode::IoError => "网络或文件读取失败",
        ErrorCode::StaleHandle => "会话已过期，请重试",
        ErrorCode::Unsupported => "不支持的媒体格式或操作",
        ErrorCode::Backpressure => "请求过于频繁，请稍后重试",
        ErrorCode::Cancelled => "操作已取消",
        ErrorCode::InvalidArgument => "参数格式无效",
        ErrorCode::RangeInvalid => "分片范围无效",
        ErrorCode::BadState => "插件状态异常，请重试",
        ErrorCode::OutOfWindow => "请求超出当前播放窗口范围",
        ErrorCode::Internal => "插件内部错误",
    }
    .into()
}
