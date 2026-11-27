//! Single-owner source browsing, request correlation, URL resolver routing,
//! plugin playlists, and stream variant metadata for [`super::PluginController`].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iris_api::StreamVariant;
use iris_api::v2::{
    Capability, ErrorCode, Id,
    source::{PlaylistEntry, SourceEntry, SourceFilter},
};
use iris_base::net::http_client::validate_stream_url;
use iris_core::media_uri::MediaUri;
use iris_core::store::Store;
use iris_plugin_host::{AcceptedPage, Binding, BrowseTarget, PluginStatus, WindowSession};

use super::ports::{
    OpenUrlOutcome, PlayerProgress, PluginBatchOutcome, PluginPlayer, PluginSideEffect,
    error_code_label,
};

pub const SOURCE_TIMEOUT: Duration = Duration::from_secs(30);

/// Active plugin `MediaSource` browsing state inside [`ActiveSource::Plugin`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginSourceView {
    pub plugin_id: String,
    pub plugin_name: String,
    pub source_id: String,
    pub source_name: String,
    pub supports_search: bool,
    pub auth_command_id: Option<String>,
    pub folder_id: Option<String>,
    pub parent_id: Option<String>,
    pub title: String,
    pub next_page: Option<String>,
    pub authenticated: bool,
    pub auth_hint: Option<String>,
    pub loading: bool,
    pub error: Option<String>,
}

/// Tracks which media source is currently browsed in the catalog Files view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ActiveSource {
    #[default]
    Local,
    Plugin(Box<PluginSourceView>),
}

impl ActiveSource {
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    pub fn display_badge(&self) -> Option<String> {
        match self {
            Self::Local => None,
            Self::Plugin(ps) => Some(format!("{} · {}", ps.plugin_name, ps.source_name)),
        }
    }
}

/// Metadata for a currently playing plugin stream with switchable quality variants.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActiveStreamMeta {
    pub plugin_id: Option<String>,
    pub source_id: Option<String>,
    pub entry_id: Option<String>,
    pub variants: Vec<StreamVariant>,
    pub active_variant: Option<String>,
}

impl ActiveStreamMeta {
    pub fn matches_source(&self, pid: Option<&str>, sid: Option<&str>) -> bool {
        pid.is_none_or(|p| self.plugin_id.as_deref() == Some(p))
            && sid.is_none_or(|s| self.source_id.as_deref() == Some(s))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingBrowseRequest {
    pub req_id: Id,
    pub deadline: Instant,
    pub plugin_id: String,
    pub source_id: String,
    pub append: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingResolveRequest {
    pub req_id: Id,
    pub deadline: Instant,
    pub plugin_id: String,
    pub source_id: String,
    pub entry_id: String,
    pub title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceErrorKind {
    Browse { source_id: String },
    Resolve,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamReply {
    Requested {
        source_id: String,
        entry_id: String,
        title: Option<String>,
    },
}

#[derive(Default)]
pub(super) struct BrowseState {
    pub view: Option<(String, Id)>,
    pub pages: Vec<Arc<AcceptedPage>>,
    pub page_binding: Option<Arc<Binding>>,
    pub source_filters: Vec<SourceFilter>,
    pub filter_values: BTreeMap<String, String>,
    pub pending_browse: Option<PendingBrowseRequest>,
    pub pending_resolve: Option<PendingResolveRequest>,
}

impl BrowseState {
    pub fn cancel_browse(
        &mut self,
        active_source: &mut ActiveSource,
    ) -> Option<PendingBrowseRequest> {
        let pending = self.pending_browse.take()?;
        if let ActiveSource::Plugin(view) = active_source
            && view.plugin_id == pending.plugin_id
            && view.source_id == pending.source_id
        {
            view.loading = false;
        }
        Some(pending)
    }

    pub fn cancel_resolve(&mut self) -> Option<PendingResolveRequest> {
        self.pending_resolve.take()
    }

    pub fn take_error(
        &mut self,
        active_source: &mut ActiveSource,
        plugin_id: &str,
        req_id: Id,
    ) -> Option<SourceErrorKind> {
        if self
            .pending_resolve
            .as_ref()
            .is_some_and(|r| r.plugin_id == plugin_id && r.req_id == req_id)
        {
            self.pending_resolve = None;
            return Some(SourceErrorKind::Resolve);
        }
        if self
            .pending_browse
            .as_ref()
            .is_some_and(|b| b.plugin_id == plugin_id && b.req_id == req_id)
        {
            let pending = self.cancel_browse(active_source)?;
            return Some(SourceErrorKind::Browse {
                source_id: pending.source_id,
            });
        }
        None
    }

    pub fn accept_stream(
        &mut self,
        plugin_id: &str,
        req_id: Option<Id>,
        now: Instant,
    ) -> Option<StreamReply> {
        let id = req_id?;
        let matches = self
            .pending_resolve
            .as_ref()
            .is_some_and(|r| r.plugin_id == plugin_id && r.req_id == id && now < r.deadline);
        if !matches {
            return None;
        }
        let pending = self.pending_resolve.take()?;
        Some(StreamReply::Requested {
            source_id: pending.source_id,
            entry_id: pending.entry_id,
            title: pending.title,
        })
    }

    pub fn take_page(
        &mut self,
        active_source: &mut ActiveSource,
        plugin_id: &str,
        req_id: Id,
        source_id: &str,
        now: Instant,
    ) -> Option<bool> {
        let matches = self.pending_browse.as_ref().is_some_and(|b| {
            b.plugin_id == plugin_id
                && b.req_id == req_id
                && b.source_id == source_id
                && now < b.deadline
        });
        if !matches {
            return None;
        }
        let pending = self.cancel_browse(active_source)?;
        Some(pending.append)
    }

    pub fn has_browse_for(&self, pid: Option<&str>, sid: Option<&str>) -> bool {
        self.pending_browse.as_ref().is_some_and(|b| {
            pid.is_none_or(|p| p == b.plugin_id) && sid.is_none_or(|s| s == b.source_id)
        })
    }

    pub fn has_media_load_for(&self, pid: Option<&str>, sid: Option<&str>) -> bool {
        self.pending_resolve.as_ref().is_some_and(|r| {
            pid.is_none_or(|p| p == r.plugin_id) && sid.is_none_or(|s| s == r.source_id)
        })
    }

    pub fn entries<'a>(&'a self, source_id: &'a str) -> impl Iterator<Item = &'a SourceEntry> {
        self.pages
            .iter()
            .filter(move |p| p.page.source_id == source_id)
            .flat_map(|p| &p.page.entries)
    }

    pub fn close_view(
        &mut self,
        window: &WindowSession,
        active_source: &mut ActiveSource,
    ) -> PluginBatchOutcome {
        if let Some((plugin, view)) = self.view.take() {
            let _ = window.close_view(&plugin, view);
        }
        self.cancel_browse(active_source);
        self.pages.clear();
        self.page_binding = None;
        self.source_filters.clear();
        self.filter_values.clear();
        let mut out = PluginBatchOutcome::default();
        out.effects.push(PluginSideEffect::ResetThumbnails);
        out
    }
}

#[derive(Default)]
pub(super) struct StreamPlaybackState {
    pub stream_plugin: Option<String>,
    pub scoped_released: bool,
    pub playlist: Vec<PlaylistEntry>,
    pub playlist_binding: Option<Arc<Binding>>,
    pub playlist_current: Option<(String, String)>,
    pub active_stream_meta: Option<ActiveStreamMeta>,
}

impl StreamPlaybackState {
    pub fn clear_meta_for(&mut self, pid: Option<&str>, sid: Option<&str>) -> bool {
        if self
            .active_stream_meta
            .as_ref()
            .is_some_and(|meta| meta.matches_source(pid, sid))
        {
            self.active_stream_meta = None;
            return true;
        }
        false
    }

    pub fn clear_playlist(&mut self) {
        self.playlist.clear();
        self.playlist_binding = None;
        self.playlist_current = None;
    }

    pub fn reconcile_live(&mut self, window: &WindowSession) -> bool {
        let mut meta_cleared = false;
        if self.playlist_binding.as_ref().is_some_and(|b| !b.is_live()) {
            self.clear_playlist();
        }
        if let Some(pid) = self
            .active_stream_meta
            .as_ref()
            .and_then(|m| m.plugin_id.clone())
            && !window
                .list()
                .iter()
                .any(|p| p.id == pid && p.enabled && p.status == PluginStatus::Ready)
        {
            meta_cleared = self.clear_meta_for(Some(&pid), None);
        }
        meta_cleared
    }
}

pub fn resolve_playlist_current(
    playlist: &[PlaylistEntry],
    playback_source_id: Option<&str>,
    playback_entry_id: Option<&str>,
    requested: Option<(&str, &str)>,
) -> Option<(String, String)> {
    if playlist.is_empty() {
        return None;
    }
    if let (Some(sid), Some(eid)) = (playback_source_id, playback_entry_id)
        && playlist
            .iter()
            .any(|p| p.source_id == sid && p.entry_id == eid)
    {
        return Some((sid.to_owned(), eid.to_owned()));
    }
    if let Some(eid) = playback_entry_id {
        if let Some((req_sid, _)) = requested
            && playlist
                .iter()
                .any(|p| p.source_id == req_sid && p.entry_id == eid)
        {
            return Some((req_sid.to_owned(), eid.to_owned()));
        }
        if let Some(hit) = playlist.iter().find(|p| p.entry_id == eid) {
            return Some((hit.source_id.clone(), hit.entry_id.clone()));
        }
    }
    if let Some((req_sid, req_eid)) = requested {
        if playlist
            .iter()
            .any(|p| p.source_id == req_sid && p.entry_id == req_eid)
        {
            return Some((req_sid.to_owned(), req_eid.to_owned()));
        }
        if let Some(hit) = playlist.iter().find(|p| p.entry_id == req_eid) {
            return Some((hit.source_id.clone(), hit.entry_id.clone()));
        }
    }
    playlist
        .first()
        .map(|first| (first.source_id.clone(), first.entry_id.clone()))
}

pub fn find_plugin_playlist_index(
    playlist: &[PlaylistEntry],
    playlist_plugin_id: &str,
    stream_plugin_id: Option<&str>,
    playlist_current: Option<&(String, String)>,
    current_uri: Option<&MediaUri>,
) -> Option<usize> {
    if playlist.len() <= 1 {
        return None;
    }
    match current_uri {
        Some(MediaUri::Local(_)) => return None,
        Some(MediaUri::Plugin { plugin_id, .. }) if plugin_id != playlist_plugin_id => {
            return None;
        }
        Some(MediaUri::HttpUrl(_)) | None if stream_plugin_id != Some(playlist_plugin_id) => {
            return None;
        }
        _ => {}
    }
    if let Some((sid, eid)) = playlist_current
        && let Some(ix) = playlist
            .iter()
            .position(|p| p.source_id == *sid && p.entry_id == *eid)
    {
        return Some(ix);
    }
    if let Some(MediaUri::Plugin {
        plugin_id,
        source_id,
        entry_id,
    }) = current_uri
        && plugin_id == playlist_plugin_id
    {
        return playlist
            .iter()
            .position(|p| p.source_id == *source_id && p.entry_id == *entry_id);
    }
    None
}

pub fn remote_search_query(supports_search: bool, raw: &str) -> Option<String> {
    supports_search
        .then(|| raw.trim())
        .filter(|q| !q.is_empty())
        .map(str::to_owned)
}

pub const fn should_suppress_playback_error_toast(
    media_load_pending: bool,
    scoped_released: bool,
) -> bool {
    media_load_pending || scoped_released
}

pub const fn should_record_progress(resume_playback: bool, scoped_released: bool) -> bool {
    resume_playback && !scoped_released
}

pub(super) fn save_player_progress_sync<P: PluginPlayer>(
    player: &P,
    store: &mut Store,
    resume_playback: bool,
    scoped_released: bool,
) {
    if should_record_progress(resume_playback, scoped_released)
        && let Some(progress) = player.progress()
    {
        match progress {
            PlayerProgress::Local {
                path,
                pos,
                dur,
                eof,
            } => {
                let _ = store.remember_path(&path, pos, dur, eof);
            }
            PlayerProgress::Uri { uri, pos, dur, eof } => {
                let parsed = MediaUri::parse(&uri);
                store.remember(&parsed, pos, dur, eof);
            }
        }
    }
    if store.is_dirty() {
        store.save();
    }
}

pub(super) fn validate_resolve_args(
    source_id: &str,
    entry_id: &str,
    variant: Option<&str>,
) -> bool {
    iris_api::is_valid_id(source_id)
        && !entry_id.is_empty()
        && entry_id.len() <= 4096
        && !entry_id.chars().any(char::is_control)
        && variant
            .is_none_or(|v| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control))
}

pub(super) fn select_url_resolver(
    window: &WindowSession,
    url: &str,
    selected: &str,
) -> Result<Option<(String, String)>, OpenUrlOutcome> {
    let matches = window.url_resolvers(Some(url));
    let plugin = match selected {
        "direct" => None,
        "auto" | "" if matches.len() > 1 => {
            return Err(OpenUrlOutcome::ValidationError(
                "多个插件匹配此地址，请选择打开方式".into(),
            ));
        }
        "auto" | "" => matches.first().map(|(id, _)| id.clone()),
        id if window.url_resolvers(None).iter().any(|(p, _)| p == id) => Some(id.into()),
        _ => {
            return Err(OpenUrlOutcome::ValidationError(
                "所选解析插件已不可用".into(),
            ));
        }
    };
    if let Some(plugin) = plugin {
        let Ok(parsed) = validate_stream_url(url) else {
            return Err(OpenUrlOutcome::ValidationError(
                "请输入合法的 HTTP/HTTPS 地址".into(),
            ));
        };
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(OpenUrlOutcome::ValidationError(
                "解析地址不能内嵌账号密码，请通过插件登录".into(),
            ));
        }
        if let Err(error) = window.precheck_capability(&plugin, Capability::UrlResolver) {
            return Err(OpenUrlOutcome::ValidationError(format!(
                "解析请求未能提交 · {}",
                error_code_label(error)
            )));
        }
        Ok(Some((plugin, parsed.to_string())))
    } else {
        let trimmed = url.trim();
        if trimmed.contains("://") && validate_stream_url(trimmed).is_err() {
            return Err(OpenUrlOutcome::ValidationError(
                "仅支持合法的 http:// 或 https:// 媒体地址".into(),
            ));
        }
        Ok(None)
    }
}

pub(super) fn build_browse_target(
    source_id: &str,
    folder_id: Option<String>,
    query: Option<String>,
    filters: BTreeMap<String, String>,
) -> BrowseTarget {
    BrowseTarget {
        source: source_id.to_owned(),
        folder: folder_id,
        query,
        filters,
    }
}

pub(super) fn should_fallback_error_toast(matched_source: bool, code: ErrorCode) -> bool {
    !matched_source && code != ErrorCode::Cancelled
}
