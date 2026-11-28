//! Unified virtual media source controller: GPUI bridge delegating local folders,
//! `MediaSource` plugins, bookmarks, direct HTTP(S) streams, remote photo header
//! probing, and `step` navigation to [`iris_app::AppModel`].

use std::time::Instant;

use gpui::{Context, Window};
use iris_api::SourceManageAction;
use iris_app::{
    BookmarkAction, DirectStreamTarget, MediaOpenCommit, StepAction, parse_direct_stream_input,
};
use iris_core::bookmarks::PinnedBookmark;
use iris_core::catalog::{CatalogItem, CatalogItemKind};
use iris_core::library::Kind;
use iris_core::media_uri::MediaUri;
use iris_core::photo;
use iris_core::player::RemoteMediaRequest;

use super::WindowShell;
use super::views::DirtyRegions;

impl WindowShell {
    /// Cancels in-flight remote/plugin media loads without interrupting directory browsing.
    pub(super) fn cancel_media_loads(&mut self) {
        if self.plugins().has_pending_resolve() {
            self.plugin_ui.resolve_timeout = None;
        }
        if self.catalog().has_remote_load() {
            self.catalog_ui.remote_load = None;
        }
        let _ = self.app.cancel_in_flight_media_loads();
    }

    pub(super) fn browse_local_source(&mut self, cx: &mut Context<Self>) {
        let out = self.app.browse_local_source();
        self.plugin_ui.browse_timeout = None;
        if out
            .effects
            .contains(&iris_app::PluginSideEffect::ResetThumbnails)
        {
            self.plugin_ui.thumbnails.reset();
        }
        self.sync_view_timer(cx);
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
        let query = if self.text_inputs.search_open {
            self.text_inputs.search_input.text().to_owned()
        } else {
            String::new()
        };
        let Some((_, out)) = self.app.refresh_plugin_source(
            plugin_id,
            source_id,
            query,
            self.text_inputs.search_open,
            Instant::now(),
        ) else {
            return;
        };
        let regions = self.apply_plugin_outcome(out, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }

    pub(super) fn pin_current_location(&mut self, cx: &mut Context<Self>) {
        match self.app.pin_current_location() {
            Ok(bm) => {
                self.notify_sidebar(cx);
                self.show_toast(format!("已固定书签 · {}", bm.name), cx);
            }
            Err(msg) => self.show_toast(msg, cx),
        }
    }

    pub(super) fn pin_bookmark(
        &mut self,
        name: String,
        uri: MediaUri,
        folder_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.app.pin_bookmark(name.clone(), uri, folder_id);
        self.notify_sidebar(cx);
        self.show_toast(format!("已固定书签 · {name}"), cx);
    }

    pub(super) fn open_bookmark(&mut self, bm: &PinnedBookmark, cx: &mut Context<Self>) {
        match self.app.prepare_open_bookmark(bm) {
            BookmarkAction::OpenLocal(path) => {
                self.sync_view_timer(cx);
                self.notify_sidebar(cx);
                self.open_path(&path, cx);
            }
            BookmarkAction::BrowsePlugin {
                plugin_id,
                source_id,
                folder_id,
            } => {
                self.sync_view_timer(cx);
                self.browse_plugin_source(&plugin_id, &source_id, folder_id, None, None, cx);
            }
            BookmarkAction::OpenDirectStream { url, title } => {
                self.open_direct_stream_url(&url, title, cx);
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
        let (_, out) = self.app.browse_plugin_source(
            plugin_id,
            source_id,
            folder_id,
            page,
            query,
            Instant::now(),
        );
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
        let out = self
            .app
            .manage_plugin_source(plugin_id, source_id, action, Instant::now());
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
        if self.catalog().has_remote_load() {
            self.catalog_ui.remote_load = None;
        }
        let _ = self.app.cancel_remote_media_load();
        let out = self.app.resolve_plugin_entry(
            plugin_id,
            source_id,
            entry_id,
            title,
            variant,
            Instant::now(),
        );
        let regions = self.apply_plugin_outcome(out, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }

    pub(super) fn switch_stream_variant(&mut self, variant_id: &str, cx: &mut Context<Self>) {
        self.overlays.popover = None;
        let Some(out) = self.app.switch_stream_variant(variant_id, Instant::now()) else {
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
        match parse_direct_stream_input(raw_url, title) {
            DirectStreamTarget::Empty => {}
            DirectStreamTarget::LocalPath(path) => self.open_path(&path, cx),
            DirectStreamTarget::InvalidScheme => {
                self.show_toast("仅支持合法的 http:// 或 https:// 媒体地址", cx);
            }
            DirectStreamTarget::Remote(req) => {
                let out = self.app.prepare_direct_remote_entry();
                self.plugin_ui.browse_timeout = None;
                if out
                    .effects
                    .contains(&iris_app::PluginSideEffect::ResetThumbnails)
                {
                    self.plugin_ui.thumbnails.reset();
                }
                self.overlays.close_variants_popover();
                self.rebuild_directory_rows();
                self.open_remote_media(*req, cx);
            }
        }
    }

    fn open_remote_media(&mut self, mut req: RemoteMediaRequest, cx: &mut Context<Self>) {
        self.cancel_media_loads();
        self.save_progress_sync();
        if !self.advance_plugin_media(cx) {
            return;
        }
        self.overlays.close_menus();
        let (generation, worker_cancel, slot) = self.app.begin_remote_stream_open();
        self.notify_all(cx);

        let task = if req.kind != Kind::Photo {
            gpui::Task::ready(Some(req))
        } else {
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
                let commit = this
                    .app
                    .commit_prepared_remote_open(generation, req, Instant::now());
                if matches!(commit, MediaOpenCommit::StaleGeneration) {
                    return;
                }
                this.catalog_ui.remote_load = None;
                this.finish_media_open_commit(commit, cx);
            });
        });
        self.catalog_ui.remote_load = Some(completion);
    }

    /// Unified previous/next navigation delegated to [`iris_app::AppModel::step_media`].
    pub(super) fn step(&mut self, delta: isize, wrap: bool, cx: &mut Context<Self>) {
        match self.app.step_media(delta, wrap, Instant::now()) {
            StepAction::OpenMediaUri { uri, title } => {
                self.open_media_uri(&uri, title, cx);
            }
            StepAction::OpenCatalogItem(item) => {
                self.open_catalog_item(item, cx);
            }
            StepAction::PluginPlaylist(out) => {
                let regions = self.apply_plugin_outcome(out, None, cx);
                if regions.any() {
                    self.notify_regions(regions, cx);
                }
            }
            StepAction::EndOfQueue => {
                if !wrap {
                    self.show_toast("已播完播放队列", cx);
                }
            }
            StepAction::EndOfList => {
                if !wrap {
                    self.show_toast("已播完当前列表", cx);
                }
            }
            StepAction::Noop => {}
        }
    }
}
