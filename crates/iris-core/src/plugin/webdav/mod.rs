//! Official built-in WebDAV media source plugin (`iris.webdav`).

mod modal;
mod protocol;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use iris_api::{
    BuiltinPlugin, CommandSpec, HostCommand, HostEvent, PROTOCOL_VERSION, Permission,
    PluginManifest, PluginResponder, SourceEntry, SourceEntryKind, SourceManageAction, SourcePage,
    SourceProviderSpec, StreamSpec, SubtitleAttachment, is_valid_id,
};

use crate::library::{self, Kind};

use modal::{build_modal, source_descriptors, toast, validate_modal_submit};
use protocol::{
    WebDavConnection, WebDavListResult, WebDavSubtitle, matching_subtitles, normalize_dav_path,
    parent_dav_path,
};

const MAX_INFLIGHT_BROWSE: usize = 4;
const MAX_CACHED_FOLDERS: usize = 128;

type SubtitleCache = BTreeMap<(String, String), Vec<WebDavSubtitle>>;

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
            .retain(|(sid, _), _| sid != source_id);
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
        let req_bg = request_id.clone();

        let spawned = std::thread::Builder::new()
            .name(format!("iris-webdav-{source_id}"))
            .spawn(move || {
                let _slot = guard;
                let cmd = match conn.list_dir(secret.as_deref(), &rel_path) {
                    Ok(mut listing) => {
                        {
                            let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());
                            let key = (source_id.clone(), listing.path.clone());
                            if !map.contains_key(&key)
                                && map.len() >= MAX_CACHED_FOLDERS
                                && let Some(oldest) = map.keys().next().cloned()
                            {
                                map.remove(&oldest);
                            }
                            map.insert(key, std::mem::take(&mut listing.subtitles));
                        }
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
        folder_id: Option<&str>,
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

        let mut subtitles = Vec::new();
        if kind == SourceEntryKind::Video {
            let parent = parent_dav_path(&rel_entry)
                .or_else(|| folder_id.map(normalize_dav_path))
                .unwrap_or_else(|| "/".into());
            let guard = self.sub_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = guard.get(&(source_id.to_string(), parent)) {
                for sub in matching_subtitles(cached, &file_name) {
                    if let Ok(sub_url) = conn.build_url(&sub.path, false) {
                        subtitles.push(SubtitleAttachment {
                            url: sub_url.to_string(),
                            label: sub.name.clone(),
                            lang: None,
                            headers: headers.clone(),
                        });
                    }
                }
            }
        }

        vec![HostCommand::ResolveStream {
            request_id: Some(request_id.to_string()),
            stream: Box::new(StreamSpec {
                source_id: Some(source_id.to_string()),
                entry_id: Some(rel_entry),
                url: url.to_string(),
                headers,
                title: Some(file_name),
                kind: Some(kind),
                is_live: false,
                variants: Vec::new(),
                active_variant: None,
                subtitles,
            }),
        }]
    }
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
                self.sub_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
                vec![HostCommand::SetSources {
                    sources: source_descriptors(&self.servers),
                }]
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
                folder_id,
                entry_id,
                ..
            } => self.resolve_stream(request_id, source_id, folder_id.as_deref(), entry_id),
            _ => Vec::new(),
        }
    }
}
