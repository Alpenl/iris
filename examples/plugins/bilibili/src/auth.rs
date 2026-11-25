//! Bilibili session credentials (`SESSDATA`, `bili_jct`, `DedeUserID`, `buvid3`),
//! QR code login parsing, WBI key caching, and user preferences.

use crate::wbi;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
pub const REFERER: &str = "https://www.bilibili.com/";

/// Strips control and Unicode bidi characters so host validation never rejects
/// remote Bilibili titles, subtitles, or chapter names.
pub fn sanitize_text(raw: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_bytes));
    let mut last_space = true;
    for ch in raw.chars() {
        if matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}') {
            continue;
        }
        let mapped = if ch.is_control() || ch.is_whitespace() {
            ' '
        } else {
            ch
        };
        if mapped == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        if out.len() + mapped.len_utf8() > max_bytes {
            break;
        }
        out.push(mapped);
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSession {
    #[serde(default)]
    pub sessdata: String,
    #[serde(default)]
    pub bili_jct: String,
    #[serde(default)]
    pub dede_user_id: String,
    #[serde(default)]
    pub buvid3: String,
    #[serde(default)]
    pub uname: Option<String>,
    #[serde(default)]
    pub vip_label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default = "default_true")]
    pub danmaku_enabled: bool,
    #[serde(default = "default_opacity")]
    pub danmaku_opacity: u8,
    #[serde(default = "default_qn")]
    pub preferred_qn: u32,
    #[serde(default = "default_codec")]
    pub preferred_codec: String,
}

fn default_true() -> bool {
    true
}
fn default_opacity() -> u8 {
    85
}
fn default_qn() -> u32 {
    120
}
fn default_codec() -> String {
    "hevc".into()
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            danmaku_enabled: true,
            danmaku_opacity: 85,
            preferred_qn: 120,
            preferred_codec: "hevc".into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct WbiKeys {
    pub img_key: String,
    pub sub_key: String,
    pub updated_secs: u64,
}

impl WbiKeys {
    pub fn mixin(&self) -> Option<String> {
        wbi::mixin_key(&self.img_key, &self.sub_key)
    }

    pub fn is_fresh(&self, now_secs: u64) -> bool {
        !self.img_key.is_empty()
            && !self.sub_key.is_empty()
            && now_secs.saturating_sub(self.updated_secs) < 3600 * 6
    }
}

#[derive(Clone, Debug, Default)]
pub struct PendingQr {
    pub qrcode_key: String,
    pub url: String,
    pub matrix: Vec<String>,
    pub status_hint: String,
}

impl StoredSession {
    pub fn is_authenticated(&self) -> bool {
        !self.sessdata.trim().is_empty()
    }

    pub fn sanitize_header(val: &str) -> String {
        val.chars()
            .filter(|c| !c.is_control() && *c != ';')
            .collect()
    }

    pub fn cookie_header(&self) -> String {
        let mut parts = Vec::new();
        if !self.buvid3.is_empty() {
            parts.push(format!("buvid3={}", Self::sanitize_header(&self.buvid3)));
        }
        parts.push("CURRENT_FNVAL=4048".into());
        if !self.sessdata.is_empty() {
            parts.push(format!(
                "SESSDATA={}",
                Self::sanitize_header(&self.sessdata)
            ));
        }
        if !self.bili_jct.is_empty() {
            parts.push(format!(
                "bili_jct={}",
                Self::sanitize_header(&self.bili_jct)
            ));
        }
        if !self.dede_user_id.is_empty() {
            parts.push(format!(
                "DedeUserID={}",
                Self::sanitize_header(&self.dede_user_id)
            ));
        }
        parts.join("; ")
    }

    pub fn api_headers(&self) -> Vec<(String, String)> {
        vec![
            ("User-Agent".into(), USER_AGENT.into()),
            ("Referer".into(), REFERER.into()),
            ("Origin".into(), "https://www.bilibili.com".into()),
            ("Accept".into(), "application/json, text/plain, */*".into()),
            ("Cookie".into(), self.cookie_header()),
        ]
    }

    pub fn media_headers(&self, include_cookie: bool) -> Vec<(String, String)> {
        let mut headers = vec![
            ("User-Agent".into(), USER_AGENT.into()),
            ("Referer".into(), REFERER.into()),
        ];
        if include_cookie && self.is_authenticated() {
            headers.push(("Cookie".into(), self.cookie_header()));
        }
        headers
    }

    pub fn parse_cookie_input(raw: &str, existing_buvid3: &str) -> Result<Self, &'static str> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("请输入 Cookie 或 SESSDATA");
        }
        let mut sessdata = String::new();
        let mut bili_jct = String::new();
        let mut dede_user_id = String::new();
        let mut buvid3 = existing_buvid3.to_owned();

        if trimmed.contains('=') {
            for pair in trimmed.split(';') {
                let Some((k, v)) = pair.trim().split_once('=') else {
                    continue;
                };
                let (k, v) = (k.trim(), v.trim());
                if k.eq_ignore_ascii_case("SESSDATA") {
                    sessdata = v.to_owned();
                } else if k.eq_ignore_ascii_case("bili_jct") {
                    bili_jct = v.to_owned();
                } else if k.eq_ignore_ascii_case("DedeUserID") {
                    dede_user_id = v.to_owned();
                } else if k.eq_ignore_ascii_case("buvid3") && !v.is_empty() {
                    buvid3 = v.to_owned();
                }
            }
        } else {
            sessdata = trimmed.to_owned();
        }
        if sessdata.is_empty() || sessdata.len() > 1024 {
            return Err("未识别到有效的 SESSDATA 字段");
        }
        Ok(Self {
            sessdata,
            bili_jct,
            dede_user_id,
            buvid3,
            uname: None,
            vip_label: None,
        })
    }
}

pub enum QrPollOutcome {
    Waiting(String),
    Expired(String),
    Success(StoredSession),
}

pub fn parse_qr_generate(json: &Value) -> Result<PendingQr, String> {
    if json.get("code").and_then(Value::as_i64) != Some(0) {
        let msg = json
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("二维码申请失败");
        return Err(msg.to_owned());
    }
    let data = json
        .get("data")
        .ok_or_else(|| "缺少 data 字段".to_owned())?;
    let url = data
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "缺少扫码 URL".to_owned())?
        .to_owned();
    let qrcode_key = data
        .get("qrcode_key")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "缺少 qrcode_key".to_owned())?
        .to_owned();
    let matrix = crate::qr::encode_qr_matrix(&url);
    Ok(PendingQr {
        qrcode_key,
        url,
        matrix,
        status_hint: "请使用哔哩哔哩手机 App 扫码并在手机端确认，随后点击「检查扫码状态」".into(),
    })
}

pub fn parse_qr_poll(
    json: &Value,
    response_headers: &[(String, String)],
    existing_buvid3: &str,
) -> Result<QrPollOutcome, String> {
    if json.get("code").and_then(Value::as_i64) != Some(0) {
        return Err(json
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("轮询扫码状态失败")
            .to_owned());
    }
    let data = json
        .get("data")
        .ok_or_else(|| "缺少 data 字段".to_owned())?;
    let status_code = data.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let msg = data.get("message").and_then(Value::as_str).unwrap_or("");
    match status_code {
        86101 => Ok(QrPollOutcome::Waiting(
            "等待扫码（请打开哔哩哔哩 App 扫一扫）".into(),
        )),
        86090 => Ok(QrPollOutcome::Waiting(
            "已扫码，请在手机上点击「确认登录」".into(),
        )),
        86038 => Ok(QrPollOutcome::Expired(
            "二维码已过期，请重新生成二维码".into(),
        )),
        0 => {
            let mut sessdata = String::new();
            let mut bili_jct = String::new();
            let mut dede_user_id = String::new();
            for (k, v) in response_headers {
                if k.eq_ignore_ascii_case("set-cookie")
                    && let Some(first) = v.split(';').next()
                    && let Some((ck, cv)) = first.split_once('=')
                {
                    let (ck, cv) = (ck.trim(), cv.trim());
                    if ck.eq_ignore_ascii_case("SESSDATA") {
                        sessdata = cv.to_owned();
                    } else if ck.eq_ignore_ascii_case("bili_jct") {
                        bili_jct = cv.to_owned();
                    } else if ck.eq_ignore_ascii_case("DedeUserID") {
                        dede_user_id = cv.to_owned();
                    }
                }
            }
            if let Some(cross_url) = data.get("url").and_then(Value::as_str)
                && let Ok(parsed) = Url::parse(cross_url)
            {
                for (k, v) in parsed.query_pairs() {
                    if k.eq_ignore_ascii_case("SESSDATA") && sessdata.is_empty() {
                        sessdata = v.into_owned();
                    } else if k.eq_ignore_ascii_case("bili_jct") && bili_jct.is_empty() {
                        bili_jct = v.into_owned();
                    } else if k.eq_ignore_ascii_case("DedeUserID") && dede_user_id.is_empty() {
                        dede_user_id = v.into_owned();
                    }
                }
            }
            if sessdata.is_empty() {
                return Err("登录响应未包含 SESSDATA 凭据".into());
            }
            Ok(QrPollOutcome::Success(StoredSession {
                sessdata,
                bili_jct,
                dede_user_id,
                buvid3: existing_buvid3.to_owned(),
                uname: None,
                vip_label: None,
            }))
        }
        _ => Err(format!("扫码状态异常 ({status_code})：{msg}")),
    }
}

pub fn apply_nav_response(
    json: &Value,
    session: &mut StoredSession,
    wbi: &mut WbiKeys,
    now_secs: u64,
) {
    let Some(data) = json.get("data") else {
        return;
    };
    if let Some(wbi_img) = data.get("wbi_img") {
        let img = wbi_img
            .get("img_url")
            .and_then(Value::as_str)
            .and_then(wbi::extract_wbi_stem);
        let sub = wbi_img
            .get("sub_url")
            .and_then(Value::as_str)
            .and_then(wbi::extract_wbi_stem);
        if let (Some(img_key), Some(sub_key)) = (img, sub) {
            wbi.img_key = img_key;
            wbi.sub_key = sub_key;
            wbi.updated_secs = now_secs;
        }
    }
    if data.get("isLogin").and_then(Value::as_bool) == Some(true) {
        session.uname = data
            .get("uname")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if let Some(mid) = data.get("mid").and_then(Value::as_u64)
            && session.dede_user_id.is_empty()
        {
            session.dede_user_id = mid.to_string();
        }
        session.vip_label = data
            .get("vip_label")
            .and_then(|v| v.get("text"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
    } else if session.is_authenticated() && json.get("code").and_then(Value::as_i64) == Some(-101) {
        session.uname = None;
        session.vip_label = None;
    }
}
