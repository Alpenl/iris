//! Unified virtual media source controller: local folders, `MediaSource` plugins,
//! bookmarks, direct HTTP(S) streams, async remote photo header probing, and `step`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use gpui::{Context, Window};
use iris_api::SourceManageAction;
use iris_base::net::http_client::{HttpCancel, validate_stream_url};
use iris_core::bookmarks::PinnedBookmark;
use iris_core::catalog::{CatalogItem, CatalogItemKind};
use iris_core::library::{self, Kind};
use iris_core::media_uri::MediaUri;
use iris_core::photo;
use iris_core::player::RemoteMediaRequest;
use iris_core::preset::SidebarMode;

use super::AppModel;
use super::state::{ActiveSource, RemoteLoad};
use super::views::DirtyRegions;

impl AppModel {
    /// Cancels in-flight remote/plugin media loads without interrupting directory browsing.
    pub(super) fn cancel_media_loads(&mut self) {
        let was_resolving = self.plugins.cancel_resolve_load();
        if was_resolving {
            self.plugin_ui.resolve_timeout = None;
        }
        let was_remote = self.catalog.remote_load.take().is_some();
        let _ = self
            .playback
            .cancel_in_flight_load(was_remote || was_resolving);
    }

    pub(super) fn browse_local_source(&mut self, cx: &mut Context<Self>) {
        self.close_plugin_browse();
        self.catalog.active_source = ActiveSource::Local;
        self.catalog.sidebar_mode = SidebarMode::Files;
        self.sync_view_timer(cx);
        self.rebuild_directory_rows();
        self.ensure_filmstrip_thumbnails(cx);
        self.notify_regions(
            DirtyRegions {
                root: true,
                sidebar: true,
                viewport: true,
                dock: true,
                ..DirtyRegions::NONE
            },
            cx,
        );
    }

    pub(super) fn refresh_plugin_source(
        &mut self,
        plugin_id: &str,
        source_id: &str,
        cx: &mut Context<Self>,
    ) {
        let ActiveSource::Plugin(view) = &self.catalog.active_source else {
            return;
        };
        if view.plugin_id != plugin_id || view.source_id != source_id {
            return;
        }
        let folder = view.folder_id.clone();
        let query = super::plugin_runtime::remote_search_query(
            view.supports_search && self.text_inputs.search_open,
            self.text_inputs.search_input.text(),
        );
        self.browse_plugin_source(plugin_id, source_id, folder, None, query, cx);
    }

    pub(super) fn pin_current_location(&mut self, cx: &mut Context<Self>) {
        let (name, uri, folder_id) = match &self.catalog.active_source {
            ActiveSource::Local => {
                let Some(folder) = self.catalog.local_dir.folder().map(Path::to_path_buf) else {
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
        self.pin_bookmark(name, uri, folder_id, cx);
    }

    pub(super) fn pin_bookmark(
        &mut self,
        name: String,
        uri: MediaUri,
        folder_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let id = (1..)
            .map(|n| format!("bm-{n}"))
            .find(|id| !self.locations.bookmarks.iter().any(|bm| &bm.id == id))
            .expect("finite bookmark list");
        self.locations.add_bookmark(PinnedBookmark {
            id,
            name: name.clone(),
            uri,
            folder_id,
        });
        self.notify_sidebar(cx);
        self.show_toast(format!("已固定书签 · {name}"), cx);
    }

    pub(super) fn open_bookmark(&mut self, bm: &PinnedBookmark, cx: &mut Context<Self>) {
        match &bm.uri {
            MediaUri::Local(path) => {
                self.catalog.active_source = ActiveSource::Local;
                self.catalog.sidebar_mode = SidebarMode::Files;
                self.sync_view_timer(cx);
                self.notify_sidebar(cx);
                self.open_path(path, cx);
            }
            MediaUri::Plugin {
                plugin_id,
                source_id,
                entry_id,
            } => {
                self.catalog.sidebar_mode = SidebarMode::Files;
                self.sync_view_timer(cx);
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
        let is_new_view = page.is_none();
        let (started, out) = self.plugins.browse_source(
            &mut self.catalog.active_source,
            plugin_id,
            source_id,
            folder_id,
            page,
            query,
            Instant::now(),
        );
        if started && is_new_view {
            self.catalog.sidebar_mode = SidebarMode::Files;
        }
        let regions = self.apply_plugin_outcome(out, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }

    pub(super) fn manage_plugin_source(
        &mut self,
        plugin_id: &str,
        source_id: &str,
        action: SourceManageAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let out = self.with_plugin_ctx(|ctrl, ctx| {
            ctrl.manage_source(ctx, plugin_id, source_id, action, Instant::now())
        });
        let regions = self
            .apply_plugin_outcome(out, Some(window), cx)
            .union(DirtyRegions {
                root: true,
                sidebar: true,
                settings: true,
                viewport: true,
                dock: true,
                ..DirtyRegions::NONE
            });
        self.notify_regions(regions, cx);
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
        let had_remote = self.catalog.remote_load.take().is_some();
        let _ = self.playback.cancel_in_flight_load(had_remote);
        let out = self.with_plugin_ctx(|ctrl, ctx| {
            ctrl.resolve_entry(
                ctx,
                plugin_id,
                source_id,
                entry_id,
                title,
                variant,
                Instant::now(),
            )
        });
        let regions = self.apply_plugin_outcome(out, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }

    pub(super) fn switch_stream_variant(&mut self, variant_id: &str, cx: &mut Context<Self>) {
        self.overlays.popover = None;
        let Some(out) = self.with_plugin_ctx(|ctrl, ctx| {
            ctrl.switch_stream_variant(ctx, variant_id, Instant::now())
        }) else {
            return;
        };
        let regions = self.apply_plugin_outcome(out, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
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
            self.open_path(Path::new(trimmed), cx);
            return;
        }
        let Ok(parsed) = validate_stream_url(trimmed) else {
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
        self.close_plugin_browse();
        self.catalog.active_source = ActiveSource::Local;
        self.plugins.clear_active_stream_meta();
        self.overlays.close_variants_popover();
        self.rebuild_directory_rows();
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
            cx,
        );
    }

    fn open_remote_media(&mut self, mut req: RemoteMediaRequest, cx: &mut Context<Self>) {
        self.cancel_media_loads();
        self.save_progress_sync();
        if !self.advance_plugin_media(cx) {
            return;
        }
        self.overlays.close_menus();
        let generation = self.playback.begin_remote_load();
        self.notify_all(cx);

        let cancel = HttpCancel::default();
        let task = if req.kind != Kind::Photo {
            gpui::Task::ready(Some(req))
        } else {
            let worker_cancel = cancel.clone();
            let slot = self.catalog.remote_work_slot.clone();
            cx.background_executor().spawn(async move {
                let _permit = slot.lock().await;
                if worker_cancel.is_cancelled() {
                    return None;
                }
                req.photo = Some(photo::probe_remote(
                    &req.playback_url,
                    &req.headers,
                    Some(&req.title),
                    &worker_cancel,
                ));
                (!worker_cancel.is_cancelled()).then_some(req)
            })
        };

        let completion = cx.spawn(async move |this, cx| {
            let Some(req) = task.await else { return };
            let _ = this.update(cx, |this, cx| {
                if !this.playback.accept_prepared_load(generation) {
                    return;
                }
                this.catalog.remote_load = None;
                let resume =
                    this.settings.resume_playback && !req.is_live && req.kind == Kind::Video;
                let resume = if resume {
                    this.store.resume(&req.uri)
                } else {
                    0.
                };
                let settings = this.settings.clone();
                match this.session_mut().open_remote(req, &settings, resume) {
                    Ok(()) => {
                        this.finish_media_open(cx);
                    }
                    Err(e) => {
                        log::warn!("open_remote failed: {e:#}");
                        if let Some(np) = this.playback.on_media_open_failed(Instant::now()) {
                            this.apply_now_playing_snapshot(np);
                        }
                        this.show_error_toast("无法打开远程流，请检查地址与网络状态", cx);
                    }
                }
                this.active_mut().image.reset();
                this.sync_viewport_effects(cx);
                this.notify_all(cx);
            });
        });
        self.catalog.remote_load = Some(RemoteLoad {
            cancel,
            _task: completion,
        });
    }

    /// Unified previous/next navigation: steps through `PlayQueue` first when non-empty,
    /// otherwise steps through the active `CatalogItem` list.
    pub(super) fn step(&mut self, delta: isize, wrap: bool, cx: &mut Context<Self>) {
        let cur = self
            .playback
            .pending_path()
            .map(Path::to_path_buf)
            .map(MediaUri::Local)
            .or_else(|| self.session().current_media_uri());

        if !self.queue.is_empty() {
            let next_q = if let Some(u) = cur.as_ref() {
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

        if self.step_plugin_playlist(cur.as_ref(), delta, wrap, cx) {
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
