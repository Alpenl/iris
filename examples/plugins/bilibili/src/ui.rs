//! Declarative SidebarPanel, StatusBadge, Login Modal (QR + Cookie), and
//! Direct BV/av/ep/live target jump modal builders.

use crate::{
    auth::{PendingQr, Preferences, StoredSession},
    playback::qn_label,
};
use iris_plugin_sdk::api::{
    ButtonSpec, PluginModal, SelectOption, SidebarPanel, SidebarSection, StatusBadge, Widget,
    v2::{HostCommand, source::SourceDescriptor},
};

pub fn sync_window_ui(
    session: &StoredSession,
    prefs: &Preferences,
    qr: Option<&PendingQr>,
    active_cid: Option<u64>,
    danmaku_count: usize,
) -> Vec<HostCommand> {
    let status_text = if prefs.danmaku_enabled {
        if active_cid.is_some() && danmaku_count > 0 {
            format!("弹幕 {danmaku_count}")
        } else {
            "B站 · 弹幕开".into()
        }
    } else {
        "B站 · 弹幕关".into()
    };
    let badge = StatusBadge {
        id: "bili-status".into(),
        text: status_text,
        tooltip: Some("点击切换哔哩哔哩弹幕显示".into()),
        accent: prefs.danmaku_enabled && active_cid.is_some(),
        action_id: Some("toggle_danmaku".into()),
    };

    let mut account_widgets = Vec::new();
    if session.is_authenticated() {
        let name = session.uname.as_deref().unwrap_or("已登录用户");
        account_widgets.push(Widget::Stat {
            label: "当前账号".into(),
            value: name.to_owned(),
            accent: true,
        });
        if let Some(vip) = &session.vip_label {
            account_widgets.push(Widget::Stat {
                label: "会员标识".into(),
                value: vip.clone(),
                accent: true,
            });
        }
        account_widgets.push(Widget::ButtonGroup {
            buttons: vec![
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
            text: "未登录（仍可浏览推荐/热门与基础画质播放；登录后支持 1080P60/4K、关注动态、收藏夹与云端历史）".into(),
            muted: true,
            mono: false,
        });
        account_widgets.push(Widget::ButtonGroup {
            buttons: vec![
                ButtonSpec {
                    label: "扫码登录".into(),
                    action_id: "start_qr".into(),
                    icon: None,
                    accent: true,
                },
                ButtonSpec {
                    label: "Cookie 登录…".into(),
                    action_id: "login".into(),
                    icon: None,
                    accent: false,
                },
            ],
        });
    }

    if let Some(qr) = qr {
        account_widgets.push(Widget::QrCode {
            label: "哔哩哔哩 App 扫码登录".into(),
            data: qr.url.clone(),
            matrix: qr.matrix.clone(),
            hint: Some(qr.status_hint.clone()),
            poll_action_id: Some("auto_poll_qr".into()),
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

    let quick_section = SidebarSection {
        heading: "直达播放".into(),
        widgets: vec![Widget::Input {
            id: "quick_target".into(),
            label: Some("BV / av / ep / ss / 直播间号或链接".into()),
            value: String::new(),
            placeholder: "例如 BV1xx411c7mD 或 ep123456 或 live:6".into(),
            secret: false,
            action_id: Some("open_quick_target".into()),
        }],
    };

    let playback_section = SidebarSection {
        heading: "播放与弹幕偏好".into(),
        widgets: vec![
            Widget::Toggle {
                id: "danmaku_enabled".into(),
                label: "启用实时滚动/顶底弹幕".into(),
                checked: prefs.danmaku_enabled,
                action_id: Some("set_danmaku_enabled".into()),
            },
            Widget::Slider {
                id: "danmaku_opacity".into(),
                label: "弹幕不透明度".into(),
                value: f64::from(prefs.danmaku_opacity),
                min: 20.0,
                max: 100.0,
                step: 5.0,
                unit: Some("%".into()),
                action_id: "set_danmaku_opacity".into(),
            },
            Widget::Select {
                id: "preferred_qn".into(),
                label: Some(format!(
                    "默认最高画质（当前：{}）",
                    qn_label(prefs.preferred_qn)
                )),
                selected: prefs.preferred_qn.to_string(),
                options: vec![
                    opt("127", "8K"),
                    opt("120", "4K"),
                    opt("116", "1080P60"),
                    opt("80", "1080P"),
                    opt("64", "720P"),
                ],
                action_id: Some("set_preferred_qn".into()),
            },
            Widget::Select {
                id: "preferred_codec".into(),
                label: Some("首选视频编码".into()),
                selected: prefs.preferred_codec.clone(),
                options: vec![opt("hevc", "HEVC"), opt("av1", "AV1"), opt("avc", "AVC")],
                action_id: Some("set_preferred_codec".into()),
            },
        ],
    };

    let panel = SidebarPanel {
        title: "哔哩哔哩 (BiliSail)".into(),
        sections: vec![
            SidebarSection {
                heading: "账号与通行证".into(),
                widgets: account_widgets,
            },
            quick_section,
            playback_section,
        ],
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
        widgets.push(Widget::QrCode {
            label: "方式一：使用哔哩哔哩手机 App 扫码登录".into(),
            data: qr.url.clone(),
            matrix: qr.matrix.clone(),
            hint: Some(qr.status_hint.clone()),
            poll_action_id: Some("auto_modal_poll_qr".into()),
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
        widgets.push(Widget::Divider);
    } else {
        widgets.push(Widget::Button {
            label: "生成手机 App 登录二维码".into(),
            action_id: "modal_start_qr".into(),
            icon: None,
            accent: true,
        });
        widgets.push(Widget::Divider);
    }
    widgets.push(Widget::Input {
        id: "cookie".into(),
        label: Some("方式二：粘贴浏览器 Cookie（含 SESSDATA=...）或直接填入 SESSDATA".into()),
        value: String::new(),
        placeholder: "SESSDATA=xxx; bili_jct=yyy; DedeUserID=12345".into(),
        secret: true,
        action_id: None,
    });

    PluginModal {
        id: "bili.login".into(),
        title: "登录哔哩哔哩".into(),
        subtitle: Some("凭据仅保存在系统钥匙串 / Windows 凭据管理器中，不写入明文文件".into()),
        widgets,
        submit_label: Some("保存 Cookie".into()),
        cancel_label: Some("关闭".into()),
    }
}

pub fn build_target_modal() -> PluginModal {
    PluginModal {
        id: "bili.open_target".into(),
        title: "打开哔哩哔哩视频 / 番剧 / 直播".into(),
        subtitle: Some(
            "支持 BV 号、av 号、ep/ss 剧集号、live:房间号 或 bilibili.com / b23.tv 链接".into(),
        ),
        widgets: vec![Widget::Input {
            id: "target".into(),
            label: Some("目标编号或链接".into()),
            value: String::new(),
            placeholder: "BV1xx411c7mD / ep123456 / live:6 / https://b23.tv/...".into(),
            secret: false,
            action_id: None,
        }],
        submit_label: Some("解析并播放".into()),
        cancel_label: Some("取消".into()),
    }
}

pub fn sync_sources(authenticated: bool) -> HostCommand {
    let src = |id: &str, name: &str, subtitle: &str, search: bool, auth: bool| SourceDescriptor {
        id: id.into(),
        name: name.into(),
        subtitle: Some(subtitle.into()),
        icon: None,
        supports_search: search,
        requires_auth: auth && !authenticated,
        auth_command_id: auth.then(|| "login".into()),
        configurable: false,
        removable: false,
    };
    HostCommand::SetSources {
        sources: vec![
            src(
                "recommend",
                "B站 · 首页推荐",
                "个性化推荐与全网视频/番剧/直播搜索",
                true,
                false,
            ),
            src(
                "popular",
                "B站 · 热门与排行",
                "综合热门、入站必刷与分区排行榜",
                true,
                false,
            ),
            src(
                "dynamic",
                "B站 · 关注动态",
                "已关注 UP 主最新视频动态",
                false,
                true,
            ),
            src(
                "favorites",
                "B站 · 我的收藏",
                "云端收藏夹列表与夹内检索",
                true,
                true,
            ),
            src(
                "bangumi",
                "B站 · 番剧与影视",
                "追番追剧与番剧国创索引",
                true,
                false,
            ),
            src(
                "history",
                "B站 · 观看历史",
                "云端观看历史游标浏览",
                false,
                true,
            ),
            src(
                "live",
                "B站 · 哔哩直播",
                "热门直播与关注开播房间",
                true,
                false,
            ),
        ],
    }
}

pub fn toast(msg: impl Into<String>) -> HostCommand {
    HostCommand::ShowToast {
        message: msg.into(),
    }
}

pub fn refresh_all_sources() -> Vec<HostCommand> {
    [
        "recommend",
        "popular",
        "dynamic",
        "favorites",
        "bangumi",
        "history",
        "live",
    ]
    .into_iter()
    .map(|sid| HostCommand::RefreshSource {
        source_id: sid.into(),
    })
    .collect()
}

fn opt(value: &str, label: &str) -> SelectOption {
    SelectOption {
        value: value.into(),
        label: label.into(),
    }
}
