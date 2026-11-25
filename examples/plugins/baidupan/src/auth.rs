//! Baidu Netdisk authentication models, QR code login (Passport `getqrcode` ->
//! `qrcode` PNG -> `channel/unicast` -> `qrbdusslogin`), Cookie (`BDUSS` / `STOKEN`),
//! Open Platform OAuth (`access_token` / `refresh_token`), user profile, and quota parsing.

use crate::sign::sanitize_text;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const BAIDU_APP_ID: &str = "250528";
pub const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
pub const MOBILE_PCS_USER_AGENT: &str = "netdisk;P2SP;3.0.0.8;netdisk;11.12.3;ANG-AN00;android-android;10.0;JSbridge4.4.0;jointBridge;1.1.0;";
pub const OFFICIAL_DLINK_USER_AGENT: &str = "pan.baidu.com";
pub const CRACK_USER_AGENT: &str = "netdisk";
pub const PAN_REFERER: &str = "https://pan.baidu.com/disk/main";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    #[default]
    Cookie,
    OpenApi,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSession {
    #[serde(default)]
    pub mode: AuthMode,
    #[serde(default)]
    pub bduss: String,
    #[serde(default)]
    pub stoken: String,
    #[serde(default)]
    pub ptoken: String,
    #[serde(default)]
    pub baiduid: String,
    #[serde(default)]
    pub panpsc: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub uid: u64,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub vip_type: Option<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuotaInfo {
    pub used: u64,
    pub total: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preferences {
    /// `"direct"` | `"transcode_1080"` | `"transcode_720"` | `"transcode_480"`
    #[serde(default = "default_playback_mode")]
    pub playback_mode: String,
    /// OpenAPI download strategy: `"official"` | `"crack"` | `"crack_video"`
    #[serde(default = "default_openapi_link_api")]
    pub openapi_link_api: String,
    /// Automatically mount same-folder `.srt`/`.vtt`/`.ass` subtitles for videos.
    #[serde(default = "default_true")]
    pub auto_subtitles: bool,
    /// Request remote thumbnails in folder/category listings.
    #[serde(default = "default_true")]
    pub show_thumbnails: bool,
    /// Default directory sort field: `"name"` | `"time"` | `"size"`.
    #[serde(default = "default_order")]
    pub default_order: String,
}

fn default_playback_mode() -> String {
    "direct".into()
}
fn default_openapi_link_api() -> String {
    "official".into()
}
fn default_true() -> bool {
    true
}
fn default_order() -> String {
    "name".into()
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            playback_mode: default_playback_mode(),
            openapi_link_api: default_openapi_link_api(),
            auto_subtitles: true,
            show_thumbnails: true,
            default_order: default_order(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PendingQr {
    pub sign: String,
    pub baiduid: String,
    pub img_url: String,
    pub matrix: Vec<String>,
    pub status_hint: String,
}

impl StoredSession {
    pub fn is_authenticated(&self) -> bool {
        match self.mode {
            AuthMode::Cookie => !self.bduss.trim().is_empty(),
            AuthMode::OpenApi => !self.access_token.trim().is_empty(),
        }
    }

    pub fn can_refresh_oauth(&self) -> bool {
        self.mode == AuthMode::OpenApi
            && !self.refresh_token.trim().is_empty()
            && !self.client_id.trim().is_empty()
            && !self.client_secret.trim().is_empty()
    }

    pub fn vip_label(&self) -> &'static str {
        match self.vip_type {
            Some(2) => "SVIP 超级会员",
            Some(1) => "VIP 会员",
            Some(0) => "普通用户",
            _ => "已登录",
        }
    }

    pub fn sanitize_cookie_val(val: &str) -> String {
        val.chars()
            .filter(|c| !c.is_control() && *c != ';')
            .collect()
    }

    pub fn cookie_header(&self) -> String {
        let mut parts = Vec::new();
        if !self.bduss.is_empty() {
            parts.push(format!("BDUSS={}", Self::sanitize_cookie_val(&self.bduss)));
        }
        if !self.stoken.is_empty() {
            parts.push(format!(
                "STOKEN={}",
                Self::sanitize_cookie_val(&self.stoken)
            ));
        }
        if !self.ptoken.is_empty() {
            parts.push(format!(
                "PTOKEN={}",
                Self::sanitize_cookie_val(&self.ptoken)
            ));
        }
        if !self.baiduid.is_empty() {
            parts.push(format!(
                "BAIDUID={}",
                Self::sanitize_cookie_val(&self.baiduid)
            ));
        }
        if !self.panpsc.is_empty() {
            parts.push(format!(
                "PANPSC={}",
                Self::sanitize_cookie_val(&self.panpsc)
            ));
        }
        parts.join("; ")
    }

    pub fn web_api_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            ("User-Agent".into(), WEB_USER_AGENT.into()),
            ("Referer".into(), PAN_REFERER.into()),
            ("Accept".into(), "application/json, text/plain, */*".into()),
        ];
        if self.mode == AuthMode::Cookie && !self.bduss.is_empty() {
            headers.push(("Cookie".into(), self.cookie_header()));
        }
        headers
    }

    pub fn pcs_api_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            ("User-Agent".into(), MOBILE_PCS_USER_AGENT.into()),
            ("Accept".into(), "application/json, text/plain, */*".into()),
        ];
        if self.mode == AuthMode::Cookie && !self.bduss.is_empty() {
            headers.push(("Cookie".into(), self.cookie_header()));
        }
        headers
    }

    /// Parses pasted browser Cookie string (`BDUSS=...; STOKEN=...`) or a bare BDUSS value.
    pub fn parse_cookie_input(raw: &str) -> Result<Self, &'static str> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("请输入 Cookie 或 BDUSS");
        }
        let mut session = Self {
            mode: AuthMode::Cookie,
            ..Default::default()
        };
        if trimmed.contains('=') {
            for token in trimmed.split([';', '\n', '\r']) {
                let token = token.trim();
                let Some((k, v)) = token.split_once('=') else {
                    continue;
                };
                let (k, v) = (k.trim(), Self::sanitize_cookie_val(v.trim()));
                if v.is_empty() || v.len() > 768 {
                    continue;
                }
                if k.eq_ignore_ascii_case("BDUSS") {
                    session.bduss = v;
                } else if k.eq_ignore_ascii_case("STOKEN") {
                    session.stoken = v;
                } else if k.eq_ignore_ascii_case("PTOKEN") {
                    session.ptoken = v;
                } else if k.eq_ignore_ascii_case("BAIDUID") {
                    session.baiduid = v;
                } else if k.eq_ignore_ascii_case("PANPSC") {
                    session.panpsc = v;
                }
            }
        } else {
            let clean = Self::sanitize_cookie_val(trimmed);
            if clean.len() <= 768 {
                session.bduss = clean;
            }
        }
        if session.bduss.is_empty() {
            return Err("未识别到有效的 BDUSS 字段，请粘贴包含 BDUSS=... 的完整 Cookie");
        }
        Ok(session)
    }

    /// Parses Open Platform OAuth credentials (`access_token`, optional `refresh_token`,
    /// `client_id`, and `client_secret`).
    pub fn parse_openapi_input(
        access_token: &str,
        refresh_token: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<Self, &'static str> {
        let at = access_token.trim();
        let rt = refresh_token.trim();
        let cid = client_id.trim();
        let csec = client_secret.trim();
        if at.is_empty() && (rt.is_empty() || cid.is_empty() || csec.is_empty()) {
            return Err(
                "开放平台登录需填写 access_token，或同时填写 refresh_token、AppKey 与 SecretKey",
            );
        }
        if at.len() > 512 || rt.len() > 512 || cid.len() > 256 || csec.len() > 256 {
            return Err("开放平台凭据字段过长");
        }
        Ok(Self {
            mode: AuthMode::OpenApi,
            access_token: Self::sanitize_cookie_val(at),
            refresh_token: Self::sanitize_cookie_val(rt),
            client_id: Self::sanitize_cookie_val(cid),
            client_secret: Self::sanitize_cookie_val(csec),
            ..Default::default()
        })
    }
}

pub enum QrPollOutcome {
    Waiting(String),
    Scanned(String),
    Expired(String),
    Confirmed { v_code: String },
}

/// Strips JSONP wrapper such as `tangram_guid_123({...})` into raw JSON text.
pub fn strip_jsonp(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        return trimmed;
    }
    if let (Some(start), Some(end)) = (trimmed.find('('), trimmed.rfind(')'))
        && start < end
    {
        return trimmed[start + 1..end].trim();
    }
    trimmed
}

pub fn extract_cookie_from_headers(headers: &[(String, String)], name: &str) -> Option<String> {
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("set-cookie")
            && let Some(first) = v.split(';').next()
            && let Some((ck, cv)) = first.split_once('=')
            && ck.trim().eq_ignore_ascii_case(name)
        {
            let clean = StoredSession::sanitize_cookie_val(cv.trim());
            if !clean.is_empty() {
                return Some(clean);
            }
        }
    }
    None
}

/// Parses `https://passport.baidu.com/v2/api/getqrcode` JSONP response.
pub fn parse_getqrcode(
    raw_body: &str,
    headers: &[(String, String)],
) -> Result<(String, String, String), String> {
    let json_str = strip_jsonp(raw_body);
    let json: Value = serde_json::from_str(json_str).map_err(|_| "解析二维码接口 JSON 失败")?;
    if json.get("errno").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        let msg = json
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("二维码申请失败");
        return Err(sanitize_text(msg, 200));
    }
    let sign = json
        .get("sign")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            json.get("imgurl")
                .and_then(Value::as_str)
                .and_then(|u| u.split("sign=").nth(1))
                .and_then(|s| s.split('&').next())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .ok_or_else(|| "二维码响应缺少 sign 字段".to_owned())?;
    let baiduid = extract_cookie_from_headers(headers, "BAIDUID").unwrap_or_default();
    let img_url = format!(
        "https://passport.baidu.com/v2/api/qrcode?sign={sign}&lp=mobile&qrloginfrom=mobile&tpl=netdisk"
    );
    Ok((sign, img_url, baiduid))
}

/// Parses `https://passport.baidu.com/channel/unicast` Comet poll response.
pub fn parse_unicast_poll(raw_body: &str) -> Result<QrPollOutcome, String> {
    let json_str = strip_jsonp(raw_body);
    let json: Value = serde_json::from_str(json_str).map_err(|_| "解析扫码轮询响应失败")?;
    let errno = json.get("errno").and_then(Value::as_i64).unwrap_or(0);
    // errno == 1 on unicast is a normal Comet long-poll timeout (still waiting).
    if errno == 1 {
        return Ok(QrPollOutcome::Waiting(
            "请使用手机「百度网盘 App」扫一扫登录".into(),
        ));
    }
    if errno == 2 {
        return Ok(QrPollOutcome::Expired(
            "二维码已过期，请点击「刷新二维码」".into(),
        ));
    }
    let channel_v = match json.get("channel_v") {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        Some(v) => v.clone(),
        None => Value::Null,
    };
    if let Some(v_code) = channel_v
        .get("v")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Ok(QrPollOutcome::Confirmed {
            v_code: v_code.to_owned(),
        });
    }
    let status = channel_v.get("status").and_then(Value::as_i64).unwrap_or(0);
    match status {
        0 => Ok(QrPollOutcome::Waiting(
            "等待扫码（请打开手机「百度网盘 App」扫一扫）".into(),
        )),
        1 => Ok(QrPollOutcome::Scanned(
            "已扫码，请在手机上点击「确认登录」".into(),
        )),
        -1 | -2 => Ok(QrPollOutcome::Expired(
            "二维码已过期，请重新生成二维码".into(),
        )),
        2 => Err("扫码确认异常（未返回临时凭据），请重新生成二维码".into()),
        _ if errno != 0 => {
            let msg = json
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("未知状态");
            Err(format!(
                "扫码轮询异常 ({errno})：{}",
                sanitize_text(msg, 120)
            ))
        }
        _ => Ok(QrPollOutcome::Waiting("等待扫码…".into())),
    }
}

/// Extracts `BDUSS`, `STOKEN`, `PTOKEN`, `BAIDUID` from `qrbdusslogin` Set-Cookie headers.
pub fn extract_qrbdusslogin_session(
    headers: &[(String, String)],
    fallback_baiduid: &str,
) -> Result<StoredSession, String> {
    let bduss = extract_cookie_from_headers(headers, "BDUSS")
        .ok_or_else(|| "确认登录响应未包含 BDUSS 凭据，请重新扫码".to_owned())?;
    let stoken = extract_cookie_from_headers(headers, "STOKEN").unwrap_or_default();
    let ptoken = extract_cookie_from_headers(headers, "PTOKEN").unwrap_or_default();
    let baiduid = extract_cookie_from_headers(headers, "BAIDUID")
        .unwrap_or_else(|| fallback_baiduid.to_owned());
    Ok(StoredSession {
        mode: AuthMode::Cookie,
        bduss,
        stoken,
        ptoken,
        baiduid,
        ..Default::default()
    })
}

/// Applies `https://pan.baidu.com/rest/2.0/membership/user/info?method=query` response.
pub fn apply_membership_uinfo(
    json: &Value,
    session: &mut StoredSession,
) -> Result<Option<QuotaInfo>, String> {
    let errno = json.get("errno").and_then(Value::as_i64).unwrap_or(0);
    if errno != 0 {
        return Err(format!("账号校验失败 (errno={errno})，Cookie 可能已过期"));
    }
    let info = json.get("user_info").unwrap_or(json);
    let username = info
        .get("username")
        .or_else(|| info.get("baidu_name"))
        .or_else(|| info.get("netdisk_name"))
        .and_then(Value::as_str)
        .map(|s| sanitize_text(s, 80))
        .filter(|s| !s.is_empty());
    let uid = info
        .get("uk")
        .or_else(|| info.get("user_id"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if uid == 0 && username.is_none() {
        return Err("未能获取有效百度账号信息，请检查 BDUSS 是否有效".into());
    }
    if uid != 0 {
        session.uid = uid;
    }
    if let Some(name) = username {
        session.username = Some(name);
    }
    let vip_type = if info.get("is_svip").and_then(Value::as_i64) == Some(1) {
        2
    } else if info.get("is_vip").and_then(Value::as_i64) == Some(1) {
        1
    } else {
        0
    };
    session.vip_type = Some(vip_type);

    let quota = match (
        json.get("used").and_then(Value::as_u64),
        json.get("total").and_then(Value::as_u64),
    ) {
        (Some(used), Some(total)) if total > 0 => Some(QuotaInfo { used, total }),
        _ => None,
    };
    Ok(quota)
}

/// Applies `https://pan.baidu.com/rest/2.0/xpan/nas?method=uinfo` (OpenAPI) response.
pub fn apply_openapi_uinfo(json: &Value, session: &mut StoredSession) -> Result<(), String> {
    let errno = json.get("errno").and_then(Value::as_i64).unwrap_or(0);
    if errno != 0 {
        return Err(format!("开放平台账号校验失败 (errno={errno})"));
    }
    let name = json
        .get("netdisk_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            json.get("baidu_name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(|s| sanitize_text(s, 80));
    if let Some(n) = name {
        session.username = Some(n);
    }
    if let Some(uk) = json.get("uk").and_then(Value::as_u64) {
        session.uid = uk;
    }
    if let Some(vt) = json.get("vip_type").and_then(Value::as_u64) {
        session.vip_type = Some(vt.min(2) as u8);
    }
    Ok(())
}

pub fn parse_quota(json: &Value) -> Option<QuotaInfo> {
    if json.get("errno").and_then(Value::as_i64).unwrap_or(0) != 0 {
        return None;
    }
    let total = json.get("total").and_then(Value::as_u64)?;
    let used = json.get("used").and_then(Value::as_u64)?;
    (total > 0).then_some(QuotaInfo { used, total })
}

/// Applies `https://openapi.baidu.com/oauth/2.0/token?grant_type=refresh_token` response.
pub fn parse_oauth_refresh(json: &Value) -> Result<(String, String), String> {
    if let Some(err) = json.get("error").and_then(Value::as_str) {
        let desc = json
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or("");
        return Err(format!(
            "刷新 OAuth 令牌失败：{err} {}",
            sanitize_text(desc, 120)
        ));
    }
    let at = json
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "OAuth 刷新响应缺少 access_token".to_owned())?
        .to_owned();
    let rt = json
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    Ok((at, rt))
}

pub fn is_token_expired_errno(errno: i64) -> bool {
    matches!(errno, 111 | -6 | 20016 | 20017)
}
