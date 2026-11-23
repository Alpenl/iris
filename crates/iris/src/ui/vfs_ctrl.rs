//! Unified virtual media source controller: local folders, `MediaSource` plugins
//! (including built-in `iris.webdav`), bookmarks, direct HTTP(S) streams, async
//! remote photo header probing, cross-origin subtitle prefetching, and `step`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use gpui::{Context, Window};
use iris_api::{HostEvent, SourceEntryKind, SourceManageAction, StreamSpec, SubtitleAttachment};
use iris_core::catalog::{CatalogItem, CatalogItemKind};
use iris_core::library::{self, Kind};
use iris_core::media_uri::MediaUri;
use iris_core::photo;
use iris_core::player::{RemoteMediaRequest, RemoteSubtitle};
use iris_core::preset::SidebarMode;
use iris_core::vfs::{self, PinnedBookmark};

use super::IrisWindow;
use super::state::{
    ActiveSource, ActiveStreamMeta, PendingSource, PluginSourceView, RemoteLoad, SourceOperation,
    StreamReply,
};

const SOURCE_TIMEOUT: Duration = Duration::from_secs(30);

fn same_origin(a: &url::Url, b: &url::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str()
            .zip(b.host_str())
            .is_some_and(|(ha, hb)| ha.eq_ignore_ascii_case(hb))
        && a.port_or_known_default() == b.port_or_known_default()
}

impl IrisWindow {
    pub(super) fn cancel_remote_requests(&mut self) {
        let pending = self.catalog.take_pending_source();
        let was_resolving =
            pending.is_some_and(|p| matches!(p.operation, SourceOperation::Resolve { .. }));
        if self.catalog.remote_load.take().is_some() || was_resolving {
            let vp = self.active_mut();
            vp.load_generation += 1;
            vp.loading_visible = false;
        }
    }

    fn begin_source_request(
        &mut self,
        plugin_id: &str,
        source_id: &str,
        operation: SourceOperation,
        cx: &mut Context<Self>,
    ) -> String {
        self.cancel_remote_requests();
        let vp = self.active_mut();
        vp.load_generation += 1;
        vp.pending_path = None;
        vp.loading_visible = matches!(operation, SourceOperation::Resolve { .. });
        let prefix = match operation {
            SourceOperation::Browse { .. } => "browse",
            SourceOperation::Resolve { .. } => "resolve",
        };
        self.catalog.source_req_seq += 1;
        let request_id = format!("{prefix}-{}", self.catalog.source_req_seq);
        self.catalog.pending_source = Some(PendingSource {
            req_id: request_id.clone(),
            deadline: std::time::Instant::now() + SOURCE_TIMEOUT,
            plugin_id: plugin_id.to_owned(),
            source_id: source_id.to_owned(),
            operation,
        });
        let id = request_id.clone();
        let pid = plugin_id.to_owned();
        let timer = cx.background_executor().timer(SOURCE_TIMEOUT);
        self.catalog.source_timeout = Some(cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                this.handle_source_error(&pid, &id, "数据源请求超时".into(), cx);
            });
        }));
        request_id
    }

    pub(super) fn browse_local_source(&mut self, cx: &mut Context<Self>) {
        self.cancel_remote_requests();
        self.catalog.active_source = ActiveSource::Local;
        self.catalog.sidebar_mode = SidebarMode::Files;
        self.rebuild_directory_rows();
        cx.notify();
    }

    pub(super) fn pin_current_location(&mut self, cx: &mut Context<Self>) {
        let (name, uri, folder_id) = match &self.catalog.active_source {
            ActiveSource::Local => {
                let Some(folder) = self.active().session.folder().map(Path::to_path_buf) else {
                    self.show_toast("当前未打开本地文件夹", cx);
                    return;
                };
                let name = folder
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| folder.display().to_string());
                (name, MediaUri::Local(folder), None)
            }
            ActiveSource::Plugin(ps) => (
                ps.title.clone(),
                MediaUri::Plugin {
                    plugin_id: ps.plugin_id.clone(),
                    source_id: ps.source_id.clone(),
                    entry_id: ps.folder_id.clone().unwrap_or_default(),
                },
                ps.folder_id.clone(),
            ),
        };
        let id = format!(
            "bm-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis())
        );
        self.locations.add_bookmark(PinnedBookmark {
            id,
            name: name.clone(),
            uri,
            folder_id,
        });
        self.show_toast(format!("已固定书签 · {name}"), cx);
    }

    pub(super) fn open_bookmark(&mut self, bm: &PinnedBookmark, cx: &mut Context<Self>) {
        match &bm.uri {
            MediaUri::Local(path) => {
                self.catalog.active_source = ActiveSource::Local;
                self.catalog.sidebar_mode = SidebarMode::Files;
                self.open_path(path, cx);
            }
            MediaUri::Plugin {
                plugin_id,
                source_id,
                entry_id,
            } => {
                self.catalog.sidebar_mode = SidebarMode::Files;
                let fid = bm
                    .folder_id
                    .clone()
                    .or_else(|| (!entry_id.is_empty()).then(|| entry_id.clone()));
                self.browse_plugin_source(plugin_id, source_id, fid, None, None, cx);
            }
            MediaUri::HttpUrl(url) => {
                self.open_direct_stream_url(url, Some(bm.name.clone()), cx);
            }
        }
    }

    pub(super) fn open_media_uri(
        &mut self,
        uri: &MediaUri,
        fallback_title: Option<String>,
        cx: &mut Context<Self>,
    ) {
        match uri {
            MediaUri::Local(path) => self.open_path(path, cx),
            MediaUri::HttpUrl(url) => self.open_direct_stream_url(url, fallback_title, cx),
            MediaUri::Plugin {
                plugin_id,
                source_id,
                entry_id,
            } => {
                self.resolve_plugin_entry(plugin_id, source_id, entry_id, fallback_title, None, cx)
            }
        }
    }

    pub(super) fn open_catalog_item(&mut self, item: CatalogItem, cx: &mut Context<Self>) {
        let variant = item.variants.first().map(|v| v.id.clone());
        match (item.kind, item.uri) {
            (_, MediaUri::Local(path)) => self.open_path(&path, cx),
            (_, MediaUri::HttpUrl(url)) => self.open_direct_stream_url(&url, Some(item.name), cx),
            (
                CatalogItemKind::Folder { folder_id },
                MediaUri::Plugin {
                    plugin_id,
                    source_id,
                    entry_id,
                },
            ) => {
                let fid = folder_id.or(Some(entry_id));
                self.browse_plugin_source(&plugin_id, &source_id, fid, None, None, cx);
            }
            (
                CatalogItemKind::Media(_),
                MediaUri::Plugin {
                    plugin_id,
                    source_id,
                    entry_id,
                },
            ) => self.resolve_plugin_entry(
                &plugin_id,
                &source_id,
                &entry_id,
                Some(item.name),
                variant,
                cx,
            ),
        }
    }

    pub(super) fn browse_plugin_source(
        &mut self,
        plugin_id: &str,
        source_id: &str,
        folder_id: Option<String>,
        page: Option<String>,
        query: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some((pid, pname, s)) = self
            .plugins
            .media_sources()
            .into_iter()
            .find(|(pid, _, s)| *pid == plugin_id && s.id == source_id)
        else {
            self.cancel_remote_requests();
            self.show_toast("该插件数据源未启用或不可用", cx);
            return;
        };
        let (pid, pname, sname, supports_search, auth_cmd) = (
            pid.to_string(),
            pname.to_string(),
            s.name.clone(),
            s.supports_search,
            s.auth_command_id.clone(),
        );
        let append = page.is_some()
            && matches!(&self.catalog.active_source,
            ActiveSource::Plugin(ps) if ps.plugin_id == pid && ps.source_id == source_id && ps.folder_id == folder_id);
        let req_id =
            self.begin_source_request(&pid, source_id, SourceOperation::Browse { append }, cx);
        if append && let ActiveSource::Plugin(ps) = &mut self.catalog.active_source {
            ps.loading = true;
            ps.error = None;
        } else {
            self.catalog.sidebar_mode = SidebarMode::Files;
            self.catalog.active_source = ActiveSource::Plugin(Box::new(PluginSourceView {
                plugin_id: pid.clone(),
                plugin_name: pname,
                source_id: source_id.to_string(),
                source_name: sname.clone(),
                supports_search,
                auth_command_id: auth_cmd,
                folder_id: folder_id.clone(),
                parent_id: None,
                title: sname,
                entries: Vec::new(),
                next_page: None,
                authenticated: true,
                auth_hint: None,
                loading: true,
                error: None,
            }));
        }
        self.rebuild_directory_rows();
        let report = self.plugins.send_to(
            &pid,
            HostEvent::SourceBrowse {
                request_id: req_id,
                source_id: source_id.to_string(),
                folder_id,
                page,
                query,
            },
        );
        self.apply_poll_report(report, None, cx);
        cx.notify();
    }

    pub(super) fn manage_plugin_source(
        &mut self,
        plugin_id: &str,
        source_id: &str,
        action: SourceManageAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action == SourceManageAction::Remove
            && self.catalog.has_remote_work_for(plugin_id, Some(source_id))
        {
            self.cancel_remote_requests();
        }
        if action == SourceManageAction::Remove
            && matches!(
                &self.catalog.active_source,
                ActiveSource::Plugin(ps) if ps.plugin_id == plugin_id && ps.source_id == source_id
            )
        {
            self.catalog.active_source = ActiveSource::Local;
            self.rebuild_directory_rows();
        }
        let report = self.plugins.send_to(
            plugin_id,
            HostEvent::SourceManage {
                source_id: source_id.to_string(),
                action,
            },
        );
        self.apply_poll_report(report, Some(window), cx);
        cx.notify();
    }

    pub(super) fn resolve_plugin_entry(
        &mut self,
        plugin_id: &str,
        source_id: &str,
        entry_id: &str,
        title: Option<String>,
        variant: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let req_id = self.begin_source_request(
            plugin_id,
            source_id,
            SourceOperation::Resolve {
                entry_id: entry_id.to_owned(),
                title,
            },
            cx,
        );
        let folder_id = match &self.catalog.active_source {
            ActiveSource::Plugin(ps) if ps.plugin_id == plugin_id && ps.source_id == source_id => {
                ps.folder_id.clone()
            }
            _ => None,
        };
        let report = self.plugins.send_to(
            plugin_id,
            HostEvent::SourceResolve {
                request_id: req_id,
                source_id: source_id.to_string(),
                folder_id,
                entry_id: entry_id.to_string(),
                variant,
            },
        );
        self.apply_poll_report(report, None, cx);
        cx.notify();
    }

    pub(super) fn switch_stream_variant(&mut self, variant_id: &str, cx: &mut Context<Self>) {
        let Some(meta) = self.catalog.active_stream_meta.clone() else {
            return;
        };
        let (Some(pid), Some(sid), Some(eid)) = (meta.plugin_id, meta.source_id, meta.entry_id)
        else {
            return;
        };
        let title = self.active().session.current_title().map(str::to_string);
        self.overlays.popover = None;
        self.resolve_plugin_entry(&pid, &sid, &eid, title, Some(variant_id.to_string()), cx);
    }

    pub(super) fn open_direct_stream_url(
        &mut self,
        raw_url: &str,
        title: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let trimmed = raw_url.trim();
        if trimmed.is_empty() {
            return;
        }
        if !trimmed.contains("://") {
            let p = Path::new(trimmed);
            if p.exists() {
                self.open_path(p, cx);
                return;
            }
        }
        let Ok(parsed) = vfs::validate_stream_url(trimmed) else {
            self.show_toast("仅支持合法的 http:// 或 https:// 媒体地址", cx);
            return;
        };
        let clean_url = parsed.to_string();
        let name = title.unwrap_or_else(|| {
            parsed
                .path_segments()
                .and_then(|mut s| s.rfind(|seg| !seg.is_empty()))
                .map(str::to_string)
                .unwrap_or_else(|| parsed.host_str().unwrap_or("网络媒体").to_string())
        });
        let low = name.to_ascii_lowercase();
        let is_live = low.ends_with(".m3u8") || low.contains("live");
        let kind = library::kind_of(Path::new(&name)).unwrap_or(Kind::Video);
        self.open_remote_media(
            RemoteMediaRequest {
                uri: MediaUri::HttpUrl(clean_url.clone()),
                title: name,
                playback_url: clean_url,
                headers: BTreeMap::new(),
                kind,
                is_live,
                photo: None,
                subtitles: Vec::new(),
            },
            None,
            Vec::new(),
            cx,
        );
    }

    fn open_remote_media(
        &mut self,
        mut req: RemoteMediaRequest,
        meta: Option<ActiveStreamMeta>,
        attachments: Vec<SubtitleAttachment>,
        cx: &mut Context<Self>,
    ) {
        let Ok(stream_url) = vfs::validate_stream_url(&req.playback_url) else {
            self.show_toast("远程流地址不合法（仅允许 HTTP/HTTPS）", cx);
            return;
        };
        self.cancel_remote_requests();
        self.save_progress_sync();
        self.overlays.close_menus();
        let generation = {
            let vp = self.active_mut();
            vp.load_generation += 1;
            vp.pending_path = None;
            vp.loading_visible = true;
            vp.load_generation
        };
        cx.notify();

        let cancel = vfs::HttpCancel::default();
        let task = if req.kind != Kind::Photo && attachments.is_empty() {
            gpui::Task::ready(Some(req))
        } else {
            let worker_cancel = cancel.clone();
            let slot = self.catalog.remote_work_slot.clone();
            cx.background_executor().spawn(async move {
                let _permit = slot.lock().await;
                if worker_cancel.is_cancelled() {
                    return None;
                }
                if req.kind == Kind::Photo {
                    req.photo = Some(photo::probe_remote(
                        &req.playback_url,
                        &req.headers,
                        Some(&req.title),
                        &worker_cancel,
                    ));
                }
                for att in attachments {
                    if worker_cancel.is_cancelled() {
                        return None;
                    }
                    let Ok(sub_url) = vfs::validate_stream_url(&att.url) else {
                        continue;
                    };
                    if !att.headers.is_empty()
                        || (!req.headers.is_empty() && !same_origin(&stream_url, &sub_url))
                    {
                        match RemoteSubtitle::prefetch(
                            sub_url.as_str(),
                            &att.headers,
                            &worker_cancel,
                        ) {
                            Ok(s) => req.subtitles.push(s),
                            Err(e) => {
                                log::warn!(
                                    "prefetching remote subtitle '{}' failed: {e}",
                                    att.label
                                )
                            }
                        }
                    } else {
                        req.subtitles.push(RemoteSubtitle::Url(sub_url.to_string()));
                    }
                }
                (!worker_cancel.is_cancelled()).then_some(req)
            })
        };

        let plugin_id = meta.as_ref().and_then(|m| m.plugin_id.clone());
        let source_id = meta.as_ref().and_then(|m| m.source_id.clone());
        let completion = cx.spawn(async move |this, cx| {
            let Some(req) = task.await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.active().load_generation != generation {
                    return;
                }
                this.catalog.remote_load = None;
                this.active_mut().loading_visible = false;
                let resume =
                    if this.settings.resume_playback && !req.is_live && req.kind == Kind::Video {
                        this.store.resume(&req.uri)
                    } else {
                        0.
                    };
                let settings = this.settings.clone();
                match this
                    .active_mut()
                    .session
                    .open_remote(req, &settings, resume)
                {
                    Ok(()) => {
                        this.catalog.active_stream_meta = meta.filter(|m| !m.variants.is_empty());
                        this.finish_media_open(cx);
                    }
                    Err(e) => this.show_toast(format!("无法打开远程流 · {e}"), cx),
                }
                this.active_mut().image.reset();
                cx.notify();
            });
        });
        self.catalog.remote_load = Some(RemoteLoad {
            cancel,
            plugin_id,
            source_id,
            _task: completion,
        });
    }

    pub(super) fn handle_plugin_resolved_stream(
        &mut self,
        plugin_id: &str,
        request_id: Option<String>,
        stream: StreamSpec,
        cx: &mut Context<Self>,
    ) {
        let (source_id, entry_id, fallback_title) =
            match self.catalog.accept_stream(plugin_id, request_id.as_deref()) {
                None => return,
                Some(StreamReply::Proactive) => (stream.source_id, stream.entry_id, None),
                Some(StreamReply::Requested {
                    source_id,
                    entry_id,
                    title,
                }) => {
                    self.active_mut().loading_visible = false;
                    (
                        stream.source_id.or(Some(source_id)),
                        stream.entry_id.or(Some(entry_id)),
                        title,
                    )
                }
            };
        let title = stream
            .title
            .or(fallback_title)
            .unwrap_or_else(|| "远程流媒体".into());
        let kind = match stream.kind {
            Some(SourceEntryKind::Photo) => Kind::Photo,
            Some(SourceEntryKind::Audio) => Kind::Audio,
            _ => library::kind_of(Path::new(&title)).unwrap_or(Kind::Video),
        };
        let is_live = stream.is_live || stream.kind == Some(SourceEntryKind::Live);
        let uri = match (&source_id, &entry_id) {
            (Some(sid), Some(eid)) => MediaUri::Plugin {
                plugin_id: plugin_id.to_string(),
                source_id: sid.clone(),
                entry_id: eid.clone(),
            },
            _ => MediaUri::HttpUrl(stream.url.clone()),
        };
        let meta = ActiveStreamMeta {
            plugin_id: Some(plugin_id.to_string()),
            source_id,
            entry_id,
            variants: stream.variants,
            active_variant: stream.active_variant,
        };
        self.open_remote_media(
            RemoteMediaRequest {
                uri,
                title,
                playback_url: stream.url,
                headers: stream.headers,
                kind,
                is_live,
                photo: None,
                subtitles: Vec::new(),
            },
            Some(meta),
            stream.subtitles,
            cx,
        );
    }

    /// Unified previous/next navigation: steps through `PlayQueue` first when non-empty,
    /// otherwise steps through the active `CatalogItem` list.
    pub(super) fn step(&mut self, delta: isize, wrap: bool, cx: &mut Context<Self>) {
        let cur = self
            .active()
            .pending_path
            .clone()
            .map(MediaUri::Local)
            .or_else(|| self.active().session.current_media_uri());

        if !self.queue.is_empty() {
            let next_q = if let Some(u) = cur.as_ref().filter(|u| self.queue.contains(u)) {
                self.queue.step(u, delta, wrap).cloned()
            } else {
                let ix = if delta < 0 { self.queue.len() - 1 } else { 0 };
                self.queue.get(ix).cloned()
            };
            if let Some(q) = next_q {
                self.open_media_uri(&q.uri, Some(q.title), cx);
            } else if !wrap {
                self.show_toast("已播完播放队列", cx);
            }
            return;
        }

        let playable: Vec<CatalogItem> = self
            .catalog
            .items
            .iter()
            .filter(|i| !i.is_folder())
            .cloned()
            .collect();
        if playable.is_empty() {
            return;
        }
        let cur_ix = cur
            .as_ref()
            .and_then(|u| playable.iter().position(|i| &i.uri == u));
        let next_ix = match cur_ix {
            Some(i) => {
                let raw = i as isize + delta;
                if wrap {
                    Some(raw.rem_euclid(playable.len() as isize) as usize)
                } else if raw >= 0 && (raw as usize) < playable.len() {
                    Some(raw as usize)
                } else {
                    None
                }
            }
            None => Some(if delta < 0 { playable.len() - 1 } else { 0 }),
        };
        if let Some(ix) = next_ix {
            self.open_catalog_item(playable[ix].clone(), cx);
        } else if !wrap {
            self.show_toast("已播完当前列表", cx);
        }
    }
}
