//! Declarative connection modal builder and form submission validator for `iris.webdav`.

use std::collections::BTreeMap;

use iris_api::{HostCommand, PluginModal, SelectOption, SourceDescriptor, Widget, is_valid_id};

use crate::vfs::validate_stream_url;

use super::protocol::{WebDavAuth, WebDavConnection, normalize_dav_path};

pub(super) const ADD_MODAL_ID: &str = "webdav.add";
pub(super) const EDIT_MODAL_PREFIX: &str = "webdav.edit:";

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

pub(super) fn source_descriptors(servers: &[WebDavConnection]) -> Vec<SourceDescriptor> {
    servers
        .iter()
        .map(|c| {
            let subtitle = if c.base_path.is_empty() || c.base_path == "/" {
                c.endpoint.clone()
            } else {
                format!("{} · {}", c.endpoint, c.base_path)
            };
            SourceDescriptor {
                id: c.id.clone(),
                name: c.name.clone(),
                subtitle: Some(subtitle),
                supports_search: false,
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
        Some(c) => (
            format!("{EDIT_MODAL_PREFIX}{}", c.id),
            format!("编辑 WebDAV · {}", c.name),
        ),
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
        "已保存凭据，留空则保持原密码/令牌不变"
    } else {
        "输入密码或 Bearer Token（加密存入系统密钥库）"
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
    }
}

pub(super) struct ValidatedServer {
    pub is_edit: bool,
    pub conn: WebDavConnection,
    pub secret: Option<String>,
}

pub(super) fn validate_modal_submit(
    servers: &[WebDavConnection],
    secrets: &BTreeMap<String, String>,
    modal_id: &str,
    values: &BTreeMap<String, String>,
) -> Result<Option<ValidatedServer>, String> {
    let edit_id = if modal_id == ADD_MODAL_ID || modal_id == "add_server" {
        None
    } else if let Some(id) = modal_id.strip_prefix(EDIT_MODAL_PREFIX) {
        Some(id.to_string())
    } else {
        return Ok(None);
    };

    let name = values.get("name").map_or("", |s| s.trim());
    if name.is_empty() {
        return Err("请填写 WebDAV 连接名称".into());
    }
    let endpoint_raw = values
        .get("endpoint")
        .or_else(|| values.get("url"))
        .map_or("", |s| s.trim());
    let parsed_url =
        validate_stream_url(endpoint_raw).map_err(|e| format!("WebDAV 服务器地址无效：{e}"))?;
    let norm_base = normalize_dav_path(values.get("base_path").map_or("/", String::as_str));
    let base_path = if norm_base == "/" {
        String::new()
    } else {
        norm_base
    };

    let auth_mode = values
        .get("auth")
        .map_or_else(|| "none".into(), |s| s.trim().to_ascii_lowercase());
    let username = values.get("username").map_or("", |s| s.trim());
    let raw_secret = values
        .get("secret")
        .or_else(|| values.get("password"))
        .map_or("", |s| s.trim_matches(['\r', '\n', '\0']));

    let conn_id = edit_id
        .clone()
        .unwrap_or_else(|| allocate_id(servers, name, parsed_url.as_str()));
    let kept_secret = edit_id.as_deref().and_then(|id| secrets.get(id)).cloned();
    let chosen_secret = if !raw_secret.is_empty() {
        Some(raw_secret.to_string())
    } else {
        kept_secret
    };

    let (auth, secret) = match auth_mode.as_str() {
        "none" | "" => (WebDavAuth::None, None),
        "basic" => {
            if username.is_empty() {
                return Err("Basic 认证需要填写用户名".into());
            }
            (
                WebDavAuth::Basic {
                    username: username.to_string(),
                },
                chosen_secret,
            )
        }
        "bearer" => {
            if chosen_secret.as_deref().unwrap_or("").trim().is_empty() {
                return Err("Bearer 认证需要填写访问令牌".into());
            }
            (WebDavAuth::Bearer, chosen_secret)
        }
        other => return Err(format!("未知的 WebDAV 认证方式：{other}")),
    };

    Ok(Some(ValidatedServer {
        is_edit: edit_id.is_some(),
        conn: WebDavConnection {
            id: conn_id,
            name: name.to_string(),
            endpoint: parsed_url.to_string(),
            base_path,
            auth,
        },
        secret,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::protocol::{matching_subtitles, parse_http_date, parse_propfind_xml};
    use super::*;

    #[test]
    fn propfind_xml_http_dates_and_modal_validation() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_http_date("Sun Nov  6 08:49:37 1994"),
            Some(784_111_777)
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));

        let xml = r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:">
            <D:response><D:href>/dav/Anime/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
            <D:response><D:href>/dav/Anime/Season%2010/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
            <D:response><D:href>/dav/Anime/Season%202/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
            <D:response><D:href>/dav/Anime/ep10.mkv</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>104857600</D:getcontentlength><D:getlastmodified>Sun, 06 Nov 1994 08:49:37 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
            <D:response><D:href>/dav/Anime/ep2%20%E7%89%B9%E5%85%B8.mkv</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>52428800</D:getcontentlength><D:getlastmodified>Wed, 21 Oct 2015 07:28:00 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
            <D:response><D:href>/dav/Anime/ep2%20%E7%89%B9%E5%85%B8.zh-Hans.ass</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
        </D:multistatus>"#;

        let parsed = parse_propfind_xml(xml, &["dav".into()], "/Anime").unwrap();
        assert_eq!(parsed.parent_path.as_deref(), Some("/"));
        assert_eq!(parsed.subfolders[0].name, "Season 2");
        assert_eq!(parsed.subfolders[1].name, "Season 10");
        assert_eq!(parsed.entries[0].name, "ep2 特典.mkv");
        assert_eq!(parsed.entries[0].size, 52_428_800);
        assert_eq!(parsed.entries[0].modified_secs, 1_445_412_480);
        assert_eq!(parsed.entries[1].modified_secs, 784_111_777);
        assert_eq!(
            matching_subtitles(&parsed.subtitles, "ep2 特典.mkv").len(),
            1
        );

        assert!(
            validate_modal_submit(&[], &BTreeMap::new(), ADD_MODAL_ID, &BTreeMap::new()).is_err()
        );
    }
}
