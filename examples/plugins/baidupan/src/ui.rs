//! Declarative SidebarPanel, StatusBadge, Login Modal (QR / Cookie / OpenAPI OAuth),
//! and SourceDescriptor synchronization for Baidu Netdisk.

use crate::{
    auth::{AuthMode, PendingQr, Preferences, QuotaInfo, StoredSession},
    sign::format_bytes,
};
use iris_plugin_sdk::api::{
    ButtonSpec, PluginModal, SelectOption, SidebarPanel, SidebarSection, StatusBadge, ViewTimer,
    Widget,
    v2::{HostCommand, source::SourceDescriptor},
};

pub fn sync_window_ui(
    session: &StoredSession,
    quota: Option<&QuotaInfo>,
    prefs: &Preferences,
    qr: Option<&PendingQr>,
) -> Vec<HostCommand> {
    let badge_text = if session.is_authenticated() {
        match session.vip_type {
            Some(2) => "网盘 · SVIP".into(),
            Some(1) => "网盘 · VIP".into(),
            _ => "网盘 · 已登录".into(),
        }
    } else {
        "网盘 · 未登录".into()
    };
    let badge = StatusBadge {
        id: "baidupan-status".into(),
        text: badge_text,
        tooltip: Some(if session.is_authenticated() {
            format!(
                "百度网盘账号：{}（点击打开登录 / 切换账号）",
                session.username.as_deref().unwrap_or("已登录")
            )
        } else {
            "点击登录百度网盘".into()
        }),
        accent: session.is_authenticated(),
        action_id: Some("login".into()),
    };

    let mut account_widgets = Vec::new();
    if session.is_authenticated() {
        let mode_label = match session.mode {
            AuthMode::Cookie => "扫码 / Cookie (PCS)",
            AuthMode::OpenApi => "开放平台 OAuth",
        };
        account_widgets.push(Widget::Stat {
            label: "当前账号".into(),
            value: session
                .username
                .clone()
                .unwrap_or_else(|| "百度网盘用户".into()),
            accent: true,
        });
        account_widgets.push(Widget::Stat {
            label: "会员 / 模式".into(),
            value: format!("{} · {mode_label}", session.vip_label()),
            accent: session.vip_type == Some(2),
        });
        if let Some(q) = quota
            && q.total > 0
        {
            let ratio = ((q.used as f64) / (q.total as f64)).clamp(0.0, 1.0) as f32;
            account_widgets.push(Widget::Progress {
                label: format!(
                    "已用 {} / {} ({:.1}%)",
                    format_bytes(q.used),
                    format_bytes(q.total),
                    ratio * 100.0
                ),
                ratio,
            });
        }
        account_widgets.push(Widget::ButtonGroup {
            buttons: vec![
                ButtonSpec {
                    label: "刷新配额".into(),
                    action_id: "refresh_account".into(),
                    icon: None,
                    accent: false,
                },
                ButtonSpec {
                    label: "切换账号".into(),
                    action_id: "login".into(),
                    icon: None,
                    accent: false,
                },
                ButtonSpec {
                    label: "退出登录".into(),
                    action_id: "logout".into(),
                    icon: None,
                    accent: false,
                },
            ],
        });
    } else {
        account_widgets.push(Widget::Label {
            text: "未登录百度网盘。支持「百度网盘 App 扫码」、粘贴浏览器 Cookie（BDUSS）或填写开放平台 OAuth 令牌。".into(),
            muted: true,
            mono: false,
        });
        account_widgets.push(Widget::ButtonGroup {
            buttons: vec![
                ButtonSpec {
                    label: "App 扫码登录".into(),
                    action_id: "start_qr".into(),
                    icon: None,
                    accent: true,
                },
                ButtonSpec {
                    label: "Cookie / OAuth…".into(),
                    action_id: "login".into(),
                    icon: None,
                    accent: false,
                },
            ],
        });
    }

    if let Some(qr) = qr {
        account_widgets.push(Widget::PixelGrid {
            label: "手机「百度网盘 App」扫码登录".into(),
            rows: qr.matrix.clone(),
            copy_text: None,
            caption: Some(qr.status_hint.clone()),
        });
        account_widgets.push(Widget::ButtonGroup {
            buttons: vec![
                ButtonSpec {
                    label: "检查扫码状态".into(),
                    action_id: "poll_qr".into(),
                    icon: None,
                    accent: true,
                },
                ButtonSpec {
                    label: "刷新二维码".into(),
                    action_id: "start_qr".into(),
                    icon: None,
                    accent: false,
                },
            ],
        });
    }

    let mut playback_widgets = vec![
        Widget::Select {
            id: "playback_mode".into(),
            label: Some("视频默认播放线路".into()),
            selected: prefs.playback_mode.clone(),
            options: vec![
                opt("direct", "原画直链 (Locate / Official)"),
                opt("transcode_1080", "云端转码 · 1080P HLS"),
                opt("transcode_720", "云端转码 · 720P HLS"),
                opt("transcode_480", "云端转码 · 480P HLS"),
            ],
            action_id: Some("set_playback_mode".into()),
        },
        Widget::Toggle {
            id: "auto_subtitles".into(),
            label: "自动挂载同目录外挂字幕 (.srt/.ass/.vtt)".into(),
            checked: prefs.auto_subtitles,
            action_id: Some("set_auto_subtitles".into()),
        },
        Widget::Toggle {
            id: "show_thumbnails".into(),
            label: "目录列表加载云端缩略图".into(),
            checked: prefs.show_thumbnails,
            action_id: Some("set_show_thumbnails".into()),
        },
        Widget::Select {
            id: "default_order".into(),
            label: Some("默认文件排序".into()),
            selected: prefs.default_order.clone(),
            options: vec![
                opt("name", "按文件名"),
                opt("time", "按修改时间"),
                opt("size", "按文件大小"),
            ],
            action_id: Some("set_default_order".into()),
        },
    ];
    if session.mode == AuthMode::OpenApi {
        playback_widgets.insert(
            1,
            Widget::Select {
                id: "openapi_link_api".into(),
                label: Some("开放平台直链策略 (OpenList)".into()),
                selected: prefs.openapi_link_api.clone(),
                options: vec![
                    opt("official", "官方 filemetas (pan.baidu.com)"),
                    opt("crack", "DLNA filemetas (netdisk)"),
                    opt("crack_video", "DLNA mediainfo (视频优化)"),
                ],
                action_id: Some("set_openapi_link_api".into()),
            },
        );
    }

    let panel = SidebarPanel {
        title: "百度网盘 (BaiduNetdisk)".into(),
        sections: vec![
            SidebarSection {
                heading: "账号与存储空间".into(),
                widgets: account_widgets,
            },
            SidebarSection {
                heading: "播放与浏览偏好".into(),
                widgets: playback_widgets,
            },
        ],
        timer: qr.map(|_| ViewTimer {
            interval_ms: 2_000,
            action_id: "auto_poll_qr".into(),
        }),
    };

    vec![
        HostCommand::SetSidebar {
            panel: Some(Box::new(panel)),
        },
        HostCommand::SetStatus {
            badge: Some(Box::new(badge)),
        },
    ]
}

pub fn build_login_modal(qr: Option<&PendingQr>) -> PluginModal {
    let mut widgets = Vec::new();
    if let Some(qr) = qr {
        widgets.push(Widget::PixelGrid {
            label: "方式一：手机「百度网盘 App」扫码登录".into(),
            rows: qr.matrix.clone(),
            copy_text: None,
            caption: Some(qr.status_hint.clone()),
        });
        widgets.push(Widget::ButtonGroup {
            buttons: vec![
                ButtonSpec {
                    label: "我已在手机确认，检查状态".into(),
                    action_id: "modal_poll_qr".into(),
                    icon: None,
                    accent: true,
                },
                ButtonSpec {
                    label: "重新生成二维码".into(),
                    action_id: "modal_start_qr".into(),
                    icon: None,
                    accent: false,
                },
            ],
        });
    } else {
        widgets.push(Widget::Button {
            label: "生成手机「百度网盘 App」登录二维码".into(),
            action_id: "modal_start_qr".into(),
            icon: None,
            accent: true,
        });
    }
    widgets.push(Widget::Divider);
    widgets.push(Widget::Input {
        id: "cookie".into(),
        label: Some(
            "方式二：粘贴浏览器 Cookie（含 BDUSS=...，建议同时含 STOKEN）或直接填 BDUSS".into(),
        ),
        value: String::new(),
        placeholder: "BDUSS=xxx; STOKEN=yyy".into(),
        secret: true,
        action_id: None,
    });
    widgets.push(Widget::Divider);
    widgets.push(Widget::Label {
        text: "方式三（可选，OpenList / 开放平台用户）：若上方 Cookie 留空，则使用下方 OAuth 令牌登录。".into(),
        muted: true,
        mono: false,
    });
    widgets.push(Widget::Input {
        id: "access_token".into(),
        label: Some("开放平台 access_token".into()),
        value: String::new(),
        placeholder: "121.xxxx.2592000.xxxx".into(),
        secret: true,
        action_id: None,
    });
    widgets.push(Widget::Input {
        id: "refresh_token".into(),
        label: Some("refresh_token（可选，用于过期自动续期）".into()),
        value: String::new(),
        placeholder: "122.xxxx".into(),
        secret: true,
        action_id: None,
    });
    widgets.push(Widget::Input {
        id: "client_id".into(),
        label: Some("AppKey / client_id（配合 refresh_token）".into()),
        value: String::new(),
        placeholder: "百度网盘开放平台 AppKey".into(),
        secret: false,
        action_id: None,
    });
    widgets.push(Widget::Input {
        id: "client_secret".into(),
        label: Some("SecretKey / client_secret（配合 refresh_token）".into()),
        value: String::new(),
        placeholder: "百度网盘开放平台 SecretKey".into(),
        secret: true,
        action_id: None,
    });

    PluginModal {
        id: "baidupan.login".into(),
        title: "登录百度网盘".into(),
        subtitle: Some("凭据仅保存在系统钥匙串 / Windows 凭据管理器中，不写入明文文件".into()),
        widgets,
        submit_label: Some("保存凭据并校验".into()),
        cancel_label: Some("关闭".into()),
        timer: qr.map(|_| ViewTimer {
            interval_ms: 2_000,
            action_id: "auto_modal_poll_qr".into(),
        }),
    }
}

pub fn sync_sources(authenticated: bool) -> HostCommand {
    let src = |id: &str, name: &str, subtitle: &str| SourceDescriptor {
        id: id.into(),
        name: name.into(),
        subtitle: Some(subtitle.into()),
        icon: Some("folder".into()),
        badge: Some("网盘".into()),
        supports_search: true,
        requires_auth: !authenticated,
        auth_command_id: Some("login".into()),
        configurable: false,
        removable: false,
    };
    HostCommand::SetSources {
        sources: vec![
            src(
                "files",
                "百度网盘 · 全部文件",
                "全盘目录层级浏览、自然排序与全盘递归搜索",
            ),
            src(
                "videos",
                "百度网盘 · 全部视频",
                "云盘视频聚合视图（原画多线路直链 / 云端转码 HLS）",
            ),
            src(
                "photos",
                "百度网盘 · 全部照片",
                "云盘照片与图库（广色域远程预览与缩略图）",
            ),
            src(
                "audio",
                "百度网盘 · 全部音频",
                "云盘无损音乐与音频文件直连播放",
            ),
        ],
    }
}

pub fn refresh_all_sources() -> Vec<HostCommand> {
    ["files", "videos", "photos", "audio"]
        .into_iter()
        .map(|sid| HostCommand::RefreshSource {
            source_id: sid.into(),
        })
        .collect()
}

pub fn toast(msg: impl Into<String>) -> HostCommand {
    HostCommand::ShowToast {
        message: msg.into(),
    }
}

fn opt(value: &str, label: &str) -> SelectOption {
    SelectOption {
        value: value.into(),
        label: label.into(),
    }
}
