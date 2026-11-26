mod page;
use super::{
    config,
    protocol::{self, WebDavAuth, WebDavConnection, WebDavListResult},
};
use iris_plugin_sdk::{
    api::v2::{
        ErrorCode, HostCommand, Id,
        task::{CredentialBucket, Task},
        task_result::{HttpResult, OriginResult, ReadResult, StoreRead},
    },
    client::{Host, completed},
};
pub(super) use page::Cache;
use serde_json::Value;
use std::rc::Rc;

pub(super) enum Intent {
    Browse {
        path: String,
        folder_id: Option<String>,
        query: Option<String>,
    },
    Resolve {
        entry: String,
    },
}
enum Phase {
    Origin(Id),
    WaitingOrigin,
    Secret(Id),
    Http(Id),
    Read(Id),
    Finished,
}
pub(super) struct Job {
    request: Id,
    pub conn: WebDavConnection,
    intent: Intent,
    phase: Phase,
    origin: String,
    grant: Option<Id>,
    headers: Vec<(String, String)>,
    path: String,
    url: url::Url,
    handle: Option<Id>,
    body: Vec<u8>,
    cached: Option<Rc<WebDavListResult>>,
}
pub(super) enum Progress {
    Pending(Vec<HostCommand>),
    Finished(Vec<HostCommand>, Option<Box<Cache>>),
}
impl Job {
    pub fn start(
        request: Id,
        conn: WebDavConnection,
        intent: Intent,
        cache: Option<&Cache>,
        host: &mut impl Host,
    ) -> Result<Self, ErrorCode> {
        let path = match &intent {
            Intent::Browse { path, .. } => path.clone(),
            Intent::Resolve { entry } => {
                protocol::parent_dav_path(entry).ok_or(ErrorCode::InvalidArgument)?
            }
        };
        let url = conn
            .build_url(&path, true)
            .map_err(|_| ErrorCode::InvalidArgument)?;
        let origin = url.origin().ascii_serialization();
        let id = host.submit(&Task::OriginLookup {
            origin: origin.clone(),
        })?;
        let cached = matches!(intent, Intent::Resolve { .. })
            .then_some(cache)
            .flatten()
            .filter(|c| c.conn == conn && c.listing.path == path)
            .map(|c| c.listing.clone());
        Ok(Self {
            request,
            conn,
            intent,
            phase: Phase::Origin(id),
            origin,
            grant: None,
            headers: Vec::new(),
            path,
            url,
            handle: None,
            body: Vec::new(),
            cached,
        })
    }
    pub fn operation(&self) -> Option<Id> {
        match self.phase {
            Phase::Origin(id) | Phase::Secret(id) | Phase::Http(id) | Phase::Read(id) => Some(id),
            _ => None,
        }
    }
    pub fn waiting_origin(&self) -> bool {
        matches!(self.phase, Phase::WaitingOrigin)
    }
    pub fn same_intent(&self, intent: &Intent) -> bool {
        matches!(
            (&self.intent, intent),
            (Intent::Browse { .. }, Intent::Browse { .. })
                | (Intent::Resolve { .. }, Intent::Resolve { .. })
        )
    }
    pub fn cancel(mut self, host: &mut impl Host) {
        if let Some(id) = self.operation() {
            host.close(id);
        }
        if let Some(id) = self.handle.take() {
            host.close(id);
        }
    }
    fn needs_plaintext_consent(&self) -> bool {
        self.url.scheme() == "http" && self.conn.auth != WebDavAuth::None
    }
    pub fn authorized(
        &mut self,
        origin: String,
        grant: Option<Id>,
        insecure: bool,
        host: &mut impl Host,
    ) -> Result<(), ErrorCode> {
        if url::Url::parse(&origin)
            .map_err(|_| ErrorCode::Denied)?
            .origin()
            != self.url.origin()
            || grant.is_none()
            || (self.needs_plaintext_consent() && !insecure)
        {
            return Err(ErrorCode::Denied);
        }
        self.grant = grant;
        self.phase = Phase::Secret(host.submit(&Task::SecretRead {
            bucket: CredentialBucket::Plugin,
            key: config::secret_key(&self.conn),
        })?);
        Ok(())
    }
    fn request_origin(&mut self) -> Progress {
        self.phase = Phase::WaitingOrigin;
        Progress::Pending(vec![HostCommand::RequestOrigin {
            request_id: self.request,
            origin: self.origin.clone(),
        }])
    }
    pub fn complete(&mut self, value: Value, host: &mut impl Host) -> Result<Progress, ErrorCode> {
        let phase = std::mem::replace(&mut self.phase, Phase::Finished);
        match phase {
            Phase::Origin(_) => match completed::<OriginResult>(value) {
                Ok(result) if !self.needs_plaintext_consent() || result.insecure_auth => self
                    .authorized(
                        result.origin,
                        Some(result.origin_grant),
                        result.insecure_auth,
                        host,
                    )?,
                Ok(_) | Err(ErrorCode::Denied) => return Ok(self.request_origin()),
                Err(e) => return Err(e),
            },
            Phase::Secret(_) => {
                let secret: StoreRead = completed(value)?;
                if self.conn.auth != WebDavAuth::None && secret.store_value.is_none() {
                    return Err(ErrorCode::AuthFailed);
                }
                self.headers = self
                    .conn
                    .auth_headers_with_secret(secret.store_value.as_deref())
                    .into_iter()
                    .collect();
                if let Some(cached) = self.cached.take() {
                    return self.finish(cached);
                }
                let body = host.blob(protocol::PROPFIND_REQUEST_XML)?;
                let mut headers = self.headers.clone();
                headers.push(("Depth".into(), "1".into()));
                headers.push((
                    "Content-Type".into(),
                    "application/xml; charset=utf-8".into(),
                ));
                let operation = host.submit(&Task::HttpStart {
                    method: "PROPFIND".into(),
                    url: self.url.to_string(),
                    headers,
                    body_handle: Some(body),
                    stream_body: false,
                    // DAV identity is tied to this exact server/root. Configure
                    // its canonical endpoint explicitly instead of forwarding credentials.
                    follow_redirects: false,
                    timeout_ms: Some(30_000),
                    origin_grant: self.grant,
                });
                host.close(body);
                self.phase = Phase::Http(operation?);
            }
            Phase::Http(_) => {
                let response: HttpResult = completed(value)?;
                let HttpResult::Response {
                    stream_handle,
                    status,
                    url,
                    headers,
                    ..
                } = response
                else {
                    return Err(ErrorCode::BadState);
                };
                self.handle = Some(stream_handle);
                if !matches!(status, 200 | 207) {
                    if status == 401
                        && headers.iter().any(|(k, v)| {
                            k.eq_ignore_ascii_case("www-authenticate")
                                && v.to_ascii_lowercase().contains("digest")
                        })
                        && !headers.iter().any(|(k, v)| {
                            k.eq_ignore_ascii_case("www-authenticate")
                                && (v.to_ascii_lowercase().contains("basic")
                                    || v.to_ascii_lowercase().contains("bearer"))
                        })
                    {
                        host.close(self.handle.take().unwrap());
                        return Ok(Progress::Finished(
                            error(
                                self.request,
                                "服务器要求 Digest 认证，当前仅支持 Basic 或 Bearer",
                            ),
                            None,
                        ));
                    }
                    return Err(if matches!(status, 401 | 403) {
                        ErrorCode::AuthFailed
                    } else {
                        ErrorCode::IoError
                    });
                }
                if url::Url::parse(&url).map_err(|_| ErrorCode::InvalidArgument)? != self.url {
                    return Err(ErrorCode::Denied);
                }
                self.read(host)?;
            }
            Phase::Read(_) => {
                let response: ReadResult = completed(value)?;
                let bytes = host.take_blob(response.blob)?;
                if self.body.len() + bytes.len() > protocol::MAX_PROPFIND_XML_BYTES {
                    return Err(ErrorCode::QuotaExceeded);
                }
                self.body.extend_from_slice(&bytes);
                if response.eof {
                    host.close(self.handle.take().ok_or(ErrorCode::BadState)?);
                    let xml =
                        std::str::from_utf8(&self.body).map_err(|_| ErrorCode::InvalidArgument)?;
                    let (_, root) = self
                        .conn
                        .root_segments()
                        .map_err(|_| ErrorCode::InvalidArgument)?;
                    let listing = protocol::parse_propfind_xml(xml, &self.url, &root, &self.path)
                        .map_err(|_| ErrorCode::InvalidArgument)?;
                    self.body = Vec::new();
                    return self.finish(Rc::new(listing));
                }
                if bytes.is_empty() {
                    return Err(ErrorCode::IoError);
                }
                self.read(host)?;
            }
            _ => return Err(ErrorCode::BadState),
        }
        Ok(Progress::Pending(Vec::new()))
    }
    fn read(&mut self, host: &mut impl Host) -> Result<(), ErrorCode> {
        self.phase = Phase::Read(host.submit(&Task::HttpRead {
            handle: self.handle.ok_or(ErrorCode::BadState)?,
            len: 256 * 1024,
        })?);
        Ok(())
    }
    fn finish(&self, listing: Rc<WebDavListResult>) -> Result<Progress, ErrorCode> {
        let cache = Cache::new(
            self.conn.clone(),
            listing,
            self.request,
            match &self.intent {
                Intent::Browse { query, .. } => query.clone(),
                _ => None,
            },
        )?;
        let (commands, next_cache) = match &self.intent {
            Intent::Browse { folder_id, .. } => (
                cache.first_page(self.request, folder_id.as_deref())?,
                Some(Box::new(cache)),
            ),
            Intent::Resolve { entry } => (
                cache.resolve(self.request, entry, &self.headers, self.grant)?,
                None,
            ),
        };
        Ok(Progress::Finished(commands, next_cache))
    }
}
pub(super) fn error(request_id: Id, message: impl Into<String>) -> Vec<HostCommand> {
    vec![HostCommand::SourceError {
        request_id,
        message: message.into(),
    }]
}
pub(super) fn failure(request: Id, code: ErrorCode) -> Vec<HostCommand> {
    let message = match code {
        ErrorCode::Denied => "服务器未获授权，或未允许 HTTP 传输凭据",
        ErrorCode::AuthFailed => "服务器认证失败，请检查凭据",
        ErrorCode::Backpressure => "目录任务已满，请稍后重试",
        ErrorCode::QuotaExceeded => "目录大小超过插件资源限制，请缩小服务器根目录",
        ErrorCode::StaleHandle => "目录分页已失效，请刷新目录",
        ErrorCode::Cancelled => "请求已取消",
        ErrorCode::Timeout => "服务器请求超时",
        _ => "WebDAV 请求失败，请检查服务器地址、权限和响应格式",
    };
    error(request, message)
}
