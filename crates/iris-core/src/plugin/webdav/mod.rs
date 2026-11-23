//! Official built-in WebDAV media source plugin (`iris.webdav`).

mod modal;
mod protocol;

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use iris_api::{
    BuiltinPlugin, CommandSpec, HostCommand, HostEvent, PROTOCOL_VERSION, Permission,
    PluginManifest, PluginResponder, SourceEntry, SourceEntryKind, SourceManageAction, SourcePage,
    SourceProviderSpec, StreamSpec, SubtitleAttachment, is_valid_id,
};

use crate::library::{self, Kind};
use crate::vault::CredentialVault;
use crate::vfs::http_client::HttpCancel;

use modal::{build_modal, source_descriptors, toast, validate_modal_submit};
use protocol::{
    WebDavAuth, WebDavConnection, WebDavListResult, WebDavSubtitle, has_userinfo,
    matching_subtitles, normalize_dav_path, parent_dav_path,
};

const MAX_INFLIGHT_BROWSE: usize = 4;
const MAX_CACHED_FOLDERS: usize = 128;

#[derive(Default)]
struct SubtitleCache {
    cancels: BTreeMap<String, HttpCancel>,
    folders: VecDeque<(String, String, Vec<WebDavSubtitle>)>,
}

impl SubtitleCache {
    fn invalidate(&mut self, source_id: &str) {
        if let Some(cancel) = self.cancels.remove(source_id) {
            cancel.cancel();
        }
        self.folders.retain(|(source, _, _)| source != source_id);
    }

    fn cancellation(&mut self, source: &str) -> HttpCancel {
        self.cancels.entry(source.to_owned()).or_default().clone()
    }

    fn clear(&mut self) {
        for (_, cancel) in std::mem::take(&mut self.cancels) {
            cancel.cancel();
        }
        self.folders.clear();
    }

    fn get(&mut self, source: &str, path: &str) -> Option<&[WebDavSubtitle]> {
        let index = self
            .folders
            .iter()
            .position(|(s, p, _)| s == source && p == path)?;
        let item = self.folders.remove(index).unwrap();
        self.folders.push_back(item);
        self.folders
            .back()
            .map(|(_, _, subtitles)| subtitles.as_slice())
    }

    fn insert(&mut self, source: String, path: String, subtitles: Vec<WebDavSubtitle>) {
        if let Some(index) = self
            .folders
            .iter()
            .position(|(s, p, _)| *s == source && *p == path)
        {
            self.folders.remove(index);
        } else if self.folders.len() == MAX_CACHED_FOLDERS {
            self.folders.pop_front();
        }
        self.folders.push_back((source, path, subtitles));
    }
}

pub(super) fn reconcile_state(
    state: &mut BTreeMap<String, String>,
    vault: &CredentialVault,
) -> bool {
    let Some(raw) = state.get_mut("servers") else {
        return false;
    };
    let Ok(mut servers) = serde_json::from_str::<Vec<WebDavConnection>>(raw) else {
        // A malformed legacy record cannot be shown safely.
        state.remove("servers");
        return true;
    };
    let mut dirty = false;
    for conn in &mut servers {
        let Ok(mut url) = crate::vfs::validate_stream_url(&conn.endpoint) else {
            dirty |= !conn.endpoint.is_empty();
            conn.endpoint.clear();
            continue;
        };
        if has_userinfo(&url) {
            let username = percent_encoding::percent_decode_str(url.username()).decode_utf8();
            let password = url.password().map(percent_encoding::percent_decode_str);
            if let Ok(username) = username
                && !username.is_empty()
                && !username.contains([':', '\r', '\n', '\0'])
                && is_valid_id(&conn.id)
                && (conn.auth == WebDavAuth::None
                    || matches!(&conn.auth, WebDavAuth::Basic { username: existing } if existing == &username))
            {
                let key = format!("secret.{}", conn.id);
                let scope = "builtin.iris.webdav";
                let stored = if let Some(password) = password {
                    vault.get_secret(scope, &key).is_some()
                        || password
                            .decode_utf8()
                            .is_ok_and(|value| vault.set_secret(scope, &key, Some(&value)).is_ok())
                } else {
                    true
                };
                if stored {
                    conn.auth = WebDavAuth::Basic {
                        username: username.into_owned(),
                    };
                }
            }
            let _ = url.set_password(None);
            let _ = url.set_username("");
            conn.allow_insecure_auth = false;
        }
        url.set_query(None);
        url.set_fragment(None);
        if conn.endpoint != url.as_str() {
            conn.endpoint = url.into();
            dirty = true;
        }
    }
    if dirty {
        *raw = serde_json::to_string(&servers).expect("WebDAV connection state is serializable");
    }
    dirty
}

struct InflightGuard(Arc<AtomicUsize>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn source_err(request_id: impl Into<String>, msg: impl Into<String>) -> Vec<HostCommand> {
    vec![HostCommand::SourceError {
        request_id: request_id.into(),
        message: msg.into(),
    }]
}

fn build_source_page(
    conn_name: &str,
    source_id: String,
    listing: WebDavListResult,
    query: Option<&str>,
) -> SourcePage {
    let q_low = query
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_lowercase);
    let matches_q = |name: &str| {
        q_low
            .as_ref()
            .is_none_or(|q| name.to_lowercase().contains(q))
    };
    let mut entries = Vec::with_capacity(listing.subfolders.len() + listing.entries.len());
    for f in listing
        .subfolders
        .into_iter()
        .filter(|f| matches_q(&f.name))
    {
        entries.push(SourceEntry {
            id: f.path,
            name: f.name,
            subtitle: None,
            kind: SourceEntryKind::Folder,
            size: None,
            modified_secs: (f.modified_secs > 0).then_some(f.modified_secs),
            duration: None,
            variants: Vec::new(),
        });
    }
    for item in listing.entries.into_iter().filter(|e| matches_q(&e.name)) {
        let kind = match item.kind {
            Kind::Video => SourceEntryKind::Video,
            Kind::Photo => SourceEntryKind::Photo,
            Kind::Audio => SourceEntryKind::Audio,
        };
        entries.push(SourceEntry {
            id: item.path,
            name: item.name,
            subtitle: None,
            kind,
            size: (item.size > 0).then_some(item.size),
            modified_secs: (item.modified_secs > 0).then_some(item.modified_secs),
            duration: None,
            variants: Vec::new(),
        });
    }
    let title = if listing.path == "/" {
        conn_name.to_string()
    } else {
        listing
            .path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(conn_name)
            .to_string()
    };
    SourcePage {
        source_id,
        folder_id: Some(listing.path),
        parent_id: listing.parent_path,
        title,
        entries,
        next_page: None,
        authenticated: true,
        auth_hint: None,
    }
}

#[derive(Default)]
pub(super) struct WebDavPlugin {
    servers: Vec<WebDavConnection>,
    secrets: BTreeMap<String, String>,
    sub_cache: Arc<Mutex<SubtitleCache>>,
    inflight: Arc<AtomicUsize>,
    responder: Option<PluginResponder>,
}

impl WebDavPlugin {
    fn serialize_servers(&self) -> Option<String> {
        (!self.servers.is_empty()).then(|| serde_json::to_string(&self.servers).ok())?
    }

    fn purge_cache_for(&self, source_id: &str) {
        self.sub_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate(source_id);
    }

    fn clear_subtitle_cache(&self) {
        let mut cache = self.sub_cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.clear();
    }

    fn submit_modal(
        &mut self,
        modal_id: &str,
        values: &BTreeMap<String, String>,
    ) -> Vec<HostCommand> {
        let validated = match validate_modal_submit(&self.servers, &self.secrets, modal_id, values)
        {
            Ok(Some(v)) => v,
            Ok(None) => return Vec::new(),
            Err(msg) => return toast(msg),
        };
        let conn_id = validated.conn.id.clone();
        let name = validated.conn.name.clone();
        if let Some(existing) = self.servers.iter_mut().find(|s| s.id == conn_id) {
            *existing = validated.conn;
        } else {
            self.servers.push(validated.conn);
        }
        match &validated.secret {
            Some(s) => {
                self.secrets.insert(conn_id.clone(), s.clone());
            }
            None => {
                self.secrets.remove(&conn_id);
            }
        }
        self.purge_cache_for(&conn_id);
        let verb = if validated.is_edit {
            "已更新"
        } else {
            "已添加"
        };
        vec![
            HostCommand::SetState {
                key: "servers".into(),
                value: self.serialize_servers(),
            },
            HostCommand::SetSecret {
                key: format!("secret.{conn_id}"),
                value: validated.secret,
            },
            HostCommand::SetSources {
                sources: source_descriptors(&self.servers),
            },
            HostCommand::CloseModal {
                id: Some(modal_id.to_string()),
            },
            HostCommand::ShowToast {
                message: format!("{verb} WebDAV 服务器：{name}"),
            },
        ]
    }

    fn try_acquire_browse_slot(&self) -> Option<InflightGuard> {
        let mut cur = self.inflight.load(Ordering::Relaxed);
        while cur < MAX_INFLIGHT_BROWSE {
            match self.inflight.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(InflightGuard(Arc::clone(&self.inflight))),
                Err(actual) => cur = actual,
            }
        }
        None
    }

    fn spawn_browse(
        &self,
        request_id: String,
        source_id: String,
        folder_id: Option<String>,
        query: Option<String>,
    ) -> Vec<HostCommand> {
        let Some(conn) = self.servers.iter().find(|s| s.id == source_id).cloned() else {
            return source_err(request_id, format!("未找到 WebDAV 服务器：{source_id}"));
        };
        let Some(responder) = self.responder.clone() else {
            return source_err(request_id, "WebDAV 异步响应通道未就绪");
        };
        let Some(guard) = self.try_acquire_browse_slot() else {
            return source_err(request_id, "WebDAV 目录请求繁忙，请稍后重试");
        };
        let secret = self.secrets.get(&source_id).cloned();
        let rel_path = folder_id
            .as_deref()
            .map_or_else(|| "/".into(), normalize_dav_path);
        let cache = Arc::clone(&self.sub_cache);
        let cancel = cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancellation(&source_id);
        let req_bg = request_id.clone();

        let spawned = std::thread::Builder::new()
            .name(format!("iris-webdav-{source_id}"))
            .spawn(move || {
                let _slot = guard;
                let listing = conn.list_dir(secret.as_deref(), &rel_path, &cancel);
                let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
                if cancel.is_cancelled() {
                    responder(source_err(req_bg, "WebDAV 连接已更改"));
                    return;
                }
                let cmd = match listing {
                    Ok(mut listing) => {
                        cache.insert(
                            source_id.clone(),
                            listing.path.clone(),
                            std::mem::take(&mut listing.subtitles),
                        );
                        HostCommand::ProvideSourcePage {
                            request_id: req_bg,
                            page: Box::new(build_source_page(
                                &conn.name,
                                source_id,
                                listing,
                                query.as_deref(),
                            )),
                        }
                    }
                    Err(e) => HostCommand::SourceError {
                        request_id: req_bg,
                        message: e.to_string(),
                    },
                };
                responder(vec![cmd]);
            });

        match spawned {
            Ok(_) => Vec::new(),
            Err(e) => source_err(request_id, format!("无法启动 WebDAV 工作线程：{e}")),
        }
    }

    fn resolve_stream(
        &self,
        request_id: &str,
        source_id: &str,
        entry_id: &str,
    ) -> Vec<HostCommand> {
        let Some(conn) = self.servers.iter().find(|s| s.id == source_id) else {
            return source_err(request_id, format!("未找到 WebDAV 服务器：{source_id}"));
        };
        let rel_entry = normalize_dav_path(entry_id);
        let url = match conn.build_url(&rel_entry, false) {
            Ok(u) => u,
            Err(e) => return source_err(request_id, format!("WebDAV 文件地址无效：{e}")),
        };
        let headers =
            conn.auth_headers_with_secret(self.secrets.get(source_id).map(String::as_str));
        let file_name = Path::new(&rel_entry)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&rel_entry)
            .to_string();
        let kind = match library::kind_of(Path::new(&file_name)) {
            Some(Kind::Photo) => SourceEntryKind::Photo,
            Some(Kind::Audio) => SourceEntryKind::Audio,
            _ => SourceEntryKind::Video,
        };

        let mut stream = StreamSpec {
            source_id: Some(source_id.to_string()),
            entry_id: Some(rel_entry.clone()),
            url: url.to_string(),
            headers,
            title: Some(file_name.clone()),
            kind: Some(kind),
            is_live: false,
            variants: Vec::new(),
            active_variant: None,
            subtitles: Vec::new(),
        };
        if kind != SourceEntryKind::Video {
            return resolved(request_id.to_owned(), stream);
        }

        let parent = parent_dav_path(&rel_entry).unwrap_or_else(|| "/".into());
        let mut cache = self.sub_cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.get(source_id, &parent) {
            attach_subtitles(&mut stream, conn, cached, &file_name);
            return resolved(request_id.to_owned(), stream);
        }
        let cancel = cache.cancellation(source_id);
        drop(cache);

        let Some(responder) = self.responder.clone() else {
            return source_err(request_id, "WebDAV 异步响应通道未就绪");
        };
        let Some(slot) = self.try_acquire_browse_slot() else {
            return source_err(request_id, "WebDAV 目录请求繁忙，请稍后重试");
        };
        let conn = conn.clone();
        let secret = self.secrets.get(source_id).cloned();
        let cache = Arc::clone(&self.sub_cache);
        let id = request_id.to_owned();
        let spawned = std::thread::Builder::new()
            .name(format!("iris-webdav-{}", conn.id))
            .spawn(move || {
                let _slot = slot;
                let listing = conn.list_dir(secret.as_deref(), &parent, &cancel);
                let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
                if cancel.is_cancelled() {
                    responder(source_err(id, "WebDAV 连接已更改"));
                    return;
                }
                if let Some(cached) = cache.get(&conn.id, &parent) {
                    attach_subtitles(&mut stream, &conn, cached, &file_name);
                } else if let Ok(listing) = listing {
                    attach_subtitles(&mut stream, &conn, &listing.subtitles, &file_name);
                    cache.insert(conn.id, parent, listing.subtitles);
                }
                // A failed optional subtitle lookup must not prevent playback.
                responder(resolved(id, stream));
            });
        match spawned {
            Ok(_) => Vec::new(),
            Err(e) => source_err(request_id, format!("无法启动 WebDAV 工作线程：{e}")),
        }
    }
}

fn attach_subtitles(
    stream: &mut StreamSpec,
    conn: &WebDavConnection,
    cached: &[WebDavSubtitle],
    file_name: &str,
) {
    stream.subtitles = matching_subtitles(cached, file_name)
        .into_iter()
        .filter_map(|sub| {
            let url = conn.build_url(&sub.path, false).ok()?;
            Some(SubtitleAttachment {
                url: url.into(),
                label: sub.name.clone(),
                lang: None,
                headers: stream.headers.clone(),
            })
        })
        .collect();
}

fn resolved(request_id: String, stream: StreamSpec) -> Vec<HostCommand> {
    vec![HostCommand::ResolveStream {
        request_id: Some(request_id),
        stream: Box::new(stream),
    }]
}

impl BuiltinPlugin for WebDavPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "iris.webdav".into(),
            name: "WebDAV 远程存储".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            author: "Iris".into(),
            description:
                "连接 NAS 与云盘 WebDAV 目录，支持 HTTP/HTTPS 浏览、修改时间排序、外挂字幕挂载与直链播放"
                    .into(),
            protocol: PROTOCOL_VERSION,
            permissions: vec![
                Permission::MediaSource,
                Permission::Secrets,
                Permission::UiModal,
                Permission::Notifications,
            ],
            source_provider: Some(SourceProviderSpec {
                label: "WebDAV 远程存储".into(),
                add_command_id: "add_server".into(),
            }),
            commands: vec![CommandSpec {
                id: "add_server".into(),
                title: "添加 WebDAV 服务器…".into(),
                subtitle: Some("配置 NAS 或云端 WebDAV 地址与凭据".into()),
                shortcut_hint: None,
            }],
            ..Default::default()
        }
    }

    fn bind_responder(&mut self, responder: PluginResponder) {
        self.responder = Some(responder);
    }

    fn handle_event(&mut self, event: &HostEvent) -> Vec<HostCommand> {
        match event {
            HostEvent::Init { state, secrets, .. } => {
                self.servers = state
                    .get("servers")
                    .and_then(|raw| serde_json::from_str::<Vec<WebDavConnection>>(raw).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|c| is_valid_id(&c.id))
                    .collect();
                self.secrets.clear();
                for conn in &self.servers {
                    if let Some(sec) = secrets.get(&format!("secret.{}", conn.id)) {
                        self.secrets.insert(conn.id.clone(), sec.clone());
                    }
                }
                self.clear_subtitle_cache();
                vec![HostCommand::SetSources {
                    sources: source_descriptors(&self.servers),
                }]
            }
            HostEvent::Shutdown => {
                self.clear_subtitle_cache();
                Vec::new()
            }
            HostEvent::Command { id, .. } | HostEvent::Action { id, .. } if id == "add_server" => {
                vec![HostCommand::OpenModal {
                    modal: Box::new(build_modal(None, false)),
                }]
            }
            HostEvent::SourceManage { source_id, action } => match action {
                SourceManageAction::Configure => {
                    let Some(conn) = self.servers.iter().find(|s| &s.id == source_id) else {
                        return toast(format!("未找到 WebDAV 服务器：{source_id}"));
                    };
                    let modal = build_modal(Some(conn), self.secrets.contains_key(source_id));
                    vec![HostCommand::OpenModal {
                        modal: Box::new(modal),
                    }]
                }
                SourceManageAction::Remove => {
                    let Some(pos) = self.servers.iter().position(|s| &s.id == source_id) else {
                        return Vec::new();
                    };
                    let removed = self.servers.remove(pos);
                    self.secrets.remove(source_id);
                    self.purge_cache_for(source_id);
                    vec![
                        HostCommand::SetState {
                            key: "servers".into(),
                            value: self.serialize_servers(),
                        },
                        HostCommand::SetSecret {
                            key: format!("secret.{source_id}"),
                            value: None,
                        },
                        HostCommand::SetSources {
                            sources: source_descriptors(&self.servers),
                        },
                        HostCommand::ShowToast {
                            message: format!("已移除 WebDAV 服务器：{}", removed.name),
                        },
                    ]
                }
            },
            HostEvent::ModalSubmit { id, values } => self.submit_modal(id, values),
            HostEvent::SourceBrowse {
                request_id,
                source_id,
                folder_id,
                query,
                ..
            } => self.spawn_browse(
                request_id.clone(),
                source_id.clone(),
                folder_id.clone(),
                query.clone(),
            ),
            HostEvent::SourceResolve {
                request_id,
                source_id,
                entry_id,
                ..
            } => self.resolve_stream(request_id, source_id, entry_id),
            _ => Vec::new(),
        }
    }
}
