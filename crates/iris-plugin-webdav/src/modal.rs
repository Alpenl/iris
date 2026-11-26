//! Declarative connection modal builder and form submission validator for `iris.webdav`.

use std::collections::BTreeMap;

use iris_plugin_sdk::api::{
    PluginModal, SelectOption, Widget, is_valid_id,
    v2::{HostCommand, source::SourceDescriptor},
};

use super::protocol::validate_stream_url;

use super::protocol::{WebDavAuth, WebDavConnection, normalize_dav_path};

pub(super) const ADD_MODAL_ID: &str = "webdav.add";
pub(super) const EDIT_MODAL_ID: &str = "webdav.edit";

pub(super) fn toast(msg: impl Into<String>) -> Vec<HostCommand> {
    vec![HostCommand::ShowToast {
        message: msg.into(),
    }]
}

fn input_widget(id: &str, label: &str, value: &str, placeholder: &str, secret: bool) -> Widget {
    Widget::Input {
        id: id.into(),
        label: Some(label.into()),
        value: value.into(),
        placeholder: placeholder.into(),
        secret,
        action_id: None,
    }
}

fn clean_descriptor_text(raw: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_bytes));
    for ch in raw.chars() {
        if ch.is_control() {
            continue;
        }
        if out.len() + ch.len_utf8() > max_bytes {
            break;
        }
        out.push(ch);
    }
    out
}

pub(super) fn source_descriptors(servers: &[WebDavConnection]) -> Vec<SourceDescriptor> {
    servers
        .iter()
        .map(|c| {
            let raw_subtitle = if c.base_path.is_empty() || c.base_path == "/" {
                c.endpoint.clone()
            } else {
                format!("{} · {}", c.endpoint, c.base_path)
            };
            let name = clean_descriptor_text(&c.name, 256);
            let subtitle = clean_descriptor_text(&raw_subtitle, 256);
            SourceDescriptor {
                id: c.id.clone(),
                name: if name.trim().is_empty() {
                    c.id.clone()
                } else {
                    name
                },
                subtitle: (!subtitle.is_empty()).then_some(subtitle),
                icon: Some("globe".into()),
                badge: Some("WebDAV".into()),
                supports_search: true,
                requires_auth: false,
                auth_command_id: None,
                configurable: true,
                removable: true,
            }
        })
        .collect()
}

fn allocate_id(servers: &[WebDavConnection], name: &str, endpoint: &str) -> String {
    let slug = |raw: &str| -> String {
        let mut out = String::new();
        for ch in raw.chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
            } else if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
            if out.len() >= 32 {
                break;
            }
        }
        out.trim_matches('-').to_string()
    };
    let mut base = slug(name);
    if base.is_empty() {
        let host = validate_stream_url(endpoint)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_default();
        base = slug(&host);
    }
    if base.is_empty() || !is_valid_id(&base) {
        base = "dav".into();
    }
    if !servers.iter().any(|s| s.id == base) {
        return base;
    }
    (2..10_000)
        .map(|i| format!("{base}-{i}"))
        .find(|cand| is_valid_id(cand) && !servers.iter().any(|s| &s.id == cand))
        .unwrap_or_else(|| format!("dav-{}", servers.len() + 1))
}

pub(super) fn build_modal(existing: Option<&WebDavConnection>, has_secret: bool) -> PluginModal {
    let (modal_id, title) = match existing {
        Some(c) => (EDIT_MODAL_ID.into(), format!("编辑 WebDAV · {}", c.name)),
        None => (ADD_MODAL_ID.into(), "添加 WebDAV 服务器".into()),
    };
    let (auth_key, username) = match existing.map(|c| &c.auth) {
        Some(WebDavAuth::Basic { username }) => ("basic", username.as_str()),
        Some(WebDavAuth::Bearer) => ("bearer", ""),
        _ => ("none", ""),
    };
    let base_val = existing
        .map(|c| {
            if c.base_path.is_empty() {
                "/"
            } else {
                c.base_path.as_str()
            }
        })
        .unwrap_or("/");
    let sec_hint = if has_secret {
        "留空保留已保存凭据；更换地址或账号需重新输入"
    } else {
        "输入密码或 Bearer Token（保存到系统凭据库）"
    };
    let options = [
        ("none", "无需认证 (None)"),
        ("basic", "账号密码 (HTTP Basic)"),
        ("bearer", "访问令牌 (Bearer Token)"),
    ]
    .into_iter()
    .map(|(v, l)| SelectOption {
        value: v.into(),
        label: l.into(),
    })
    .collect();

    PluginModal {
        id: modal_id,
        title,
        subtitle: Some("支持 NAS、Nextcloud、AList、dufs 等 HTTP/HTTPS WebDAV 存储".into()),
        widgets: vec![
            input_widget(
                "name",
                "连接名称",
                existing.map_or("", |c| &c.name),
                "例如：家庭 NAS",
                false,
            ),
            input_widget(
                "endpoint",
                "服务器地址",
                existing.map_or("", |c| &c.endpoint),
                "https://nas.example.com:5006/dav",
                false,
            ),
            input_widget("base_path", "根目录路径", base_val, "/Movies", false),
            Widget::Select {
                id: "auth".into(),
                label: Some("认证方式".into()),
                selected: auth_key.into(),
                options,
                action_id: None,
            },
            input_widget("username", "用户名", username, "Basic 认证必填", false),
            input_widget("secret", "密码 / 令牌", "", sec_hint, true),
        ],
        submit_label: Some("保存".into()),
        cancel_label: Some("取消".into()),
        timer: None,
    }
}

pub(super) struct ValidatedServer {
    pub conn: WebDavConnection,
    /// A newly supplied secret. Unchanged credentials retain their previous key.
    pub secret: Option<String>,
}

pub(super) fn validate_modal_submit(
    servers: &[WebDavConnection],
    modal_id: &str,
    editing_id: Option<&str>,
    values: &BTreeMap<String, String>,
) -> Result<ValidatedServer, String> {
    let edit = if modal_id == ADD_MODAL_ID {
        None
    } else if modal_id == EDIT_MODAL_ID {
        let id = editing_id.ok_or("未找到正在编辑的连接，请重新打开配置")?;
        Some(
            servers
                .iter()
                .find(|s| s.id == id)
                .ok_or("连接已被移除，请重新打开配置")?,
        )
    } else {
        return Err("未知的配置表单".into());
    };
    if edit.is_none() && servers.len() >= 32 {
        return Err("最多配置 32 个 WebDAV 服务器".into());
    }
    let field = |key| values.get(key).map_or("", String::as_str);
    let name = field("name").trim();
    let endpoint = field("endpoint").trim();
    let username = field("username").trim();
    let secret = field("secret");
    if name.is_empty()
        || name.len() > 256
        || name.chars().any(char::is_control)
        || endpoint.len() > 4096
        || field("base_path").len() > 4096
        || username.len() > 256
        || secret.len() > 2048
        || username.contains([':', '\r', '\n', '\0'])
        || secret.contains(['\r', '\n', '\0'])
    {
        return Err("名称、地址或凭据无效或超长".into());
    }
    let mut url =
        validate_stream_url(endpoint).map_err(|_| "请输入不含账号密码的 HTTP/HTTPS 地址")?;
    url.set_query(None);
    url.set_fragment(None);
    let auth = match field("auth") {
        "none" | "" => WebDavAuth::None,
        "basic" if !username.is_empty() => WebDavAuth::Basic {
            username: username.into(),
        },
        "bearer" => WebDavAuth::Bearer,
        _ => return Err("请选择有效的认证方式，并填写 Basic 用户名".into()),
    };
    let reuse = edit.is_some_and(|old| {
        old.auth == auth && url::Url::parse(&old.endpoint).is_ok_and(|u| u.origin() == url.origin())
    });
    if auth != WebDavAuth::None && secret.is_empty() && !reuse {
        return Err("新增连接或修改地址、账号、认证方式后，请重新输入密码或令牌".into());
    }
    let id = edit.map_or_else(
        || allocate_id(servers, name, url.as_str()),
        |old| old.id.clone(),
    );
    let credential_key =
        (reuse && auth != WebDavAuth::None).then(|| super::config::secret_key(edit.unwrap()));
    let conn = WebDavConnection {
        id,
        name: name.into(),
        endpoint: url.into(),
        base_path: normalize_dav_path(field("base_path")),
        auth,
        credential_key,
    };
    conn.root_segments().map_err(|_| "根目录路径无效")?;
    let secret = (conn.auth != WebDavAuth::None && !secret.is_empty()).then(|| secret.into());
    Ok(ValidatedServer { conn, secret })
}
