//! Bounded remote plugin thumbnail cache and quota-cooldown scheduler
//! owned alongside [`super::PluginController`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::oneshot::Canceled;
use iris_api::v2::{ErrorCode, Generation, Id};
use iris_base::net::http_client::HttpCancel;
use iris_core::media_uri::MediaUri;
use iris_plugin_host::{BackgroundResult, Binding, RetainedThumbnail, ThumbnailReceiver};

use super::PluginController;

pub const MAX_REMOTE_THUMB_CONCURRENT: usize = 3;
pub const QUOTA_RETRY_COOLDOWN: Duration = Duration::from_secs(2);
const MAX_CACHED_REMOTE_THUMBS: usize = 96;

pub type ThumbKey = (String, String);

/// In-flight background thumbnail fetch handle returned by [`ThumbnailCache::begin_fetch`].
pub struct StartedThumbnailFetch<T> {
    pub plugin_id: String,
    pub view: Id,
    pub binding: Arc<Binding>,
    pub generation: Generation,
    pub key: ThumbKey,
    pub rx: ThumbnailReceiver<T>,
}

/// Generic LRU cache + concurrency/cooldown tracker for decoded remote plugin thumbnails.
pub struct ThumbnailCache<T> {
    cache: BTreeMap<ThumbKey, RetainedThumbnail<T>>,
    order: VecDeque<ThumbKey>,
    pending: BTreeSet<ThumbKey>,
    cooldown: BTreeMap<ThumbKey, Instant>,
    cancel: HttpCancel,
}

impl<T> Default for ThumbnailCache<T> {
    fn default() -> Self {
        Self {
            cache: BTreeMap::new(),
            order: VecDeque::new(),
            pending: BTreeSet::new(),
            cooldown: BTreeMap::new(),
            cancel: HttpCancel::default(),
        }
    }
}

impl<T> Drop for ThumbnailCache<T> {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl<T> ThumbnailCache<T> {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn can_start(&self, key: &ThumbKey, now: Instant) -> bool {
        self.pending.len() < MAX_REMOTE_THUMB_CONCURRENT
            && !self.pending.contains(key)
            && self.cooldown.get(key).is_none_or(|&until| now >= until)
    }

    pub fn record_cooldown(&mut self, key: ThumbKey, now: Instant) {
        if self.cooldown.len() >= 128 {
            self.cooldown.retain(|_, &mut until| now < until);
            if self.cooldown.len() >= 128
                && let Some(first) = self.cooldown.keys().next().cloned()
            {
                self.cooldown.remove(&first);
            }
        }
        self.cooldown.insert(key, now + QUOTA_RETRY_COOLDOWN);
    }

    pub fn cached(&self, ctrl: &PluginController, uri: &MediaUri) -> Option<&T> {
        let MediaUri::Plugin {
            plugin_id,
            source_id,
            entry_id,
        } = uri
        else {
            return None;
        };
        let binding = ctrl.page_binding()?;
        if !binding.is_live() || binding.installed.plugin_id != *plugin_id {
            return None;
        }
        let key = (source_id.clone(), entry_id.clone());
        self.cache.get(&key).and_then(|v| v.image.as_ref())
    }

    pub fn needs_fetch(&self, ctrl: &PluginController, uri: &MediaUri, now: Instant) -> bool {
        let MediaUri::Plugin {
            plugin_id,
            source_id,
            entry_id,
        } = uri
        else {
            return false;
        };
        let Some(binding) = ctrl.page_binding() else {
            return false;
        };
        if !binding.is_live() || binding.installed.plugin_id != *plugin_id {
            return false;
        }
        if ctrl
            .active_browse_view()
            .is_none_or(|(owner, _)| owner != plugin_id)
        {
            return false;
        }
        let key = (source_id.clone(), entry_id.clone());
        if self.cache.contains_key(&key) || !self.can_start(&key, now) {
            return false;
        }
        ctrl.pages()
            .iter()
            .filter(|p| p.page.source_id == *source_id)
            .flat_map(|p| &p.page.entries)
            .any(|e| e.id == *entry_id && e.thumbnail.is_some())
    }

    pub fn begin_fetch(
        &mut self,
        ctrl: &PluginController,
        uri: &MediaUri,
        now: Instant,
        decode: impl FnOnce(&[u8]) -> Option<T> + Send + 'static,
    ) -> Option<StartedThumbnailFetch<T>>
    where
        T: Send + 'static,
    {
        let MediaUri::Plugin {
            plugin_id,
            source_id,
            entry_id,
        } = uri
        else {
            return None;
        };
        let binding = ctrl.page_binding()?.clone();
        if !binding.is_live() || binding.installed.plugin_id != *plugin_id {
            return None;
        }
        let key = (source_id.clone(), entry_id.clone());
        if self.cache.contains_key(&key) || !self.can_start(&key, now) {
            return None;
        }
        let (owner, view) = ctrl.active_browse_view()?.clone();
        if owner != *plugin_id {
            return None;
        }
        let resource = ctrl
            .pages()
            .iter()
            .filter(|p| p.page.source_id == *source_id)
            .flat_map(|p| &p.page.entries)
            .find(|e| e.id == *entry_id)
            .and_then(|e| e.thumbnail.clone())?;
        let (generation, rx) = match ctrl.window().spawn_thumbnail_fetch(
            plugin_id,
            binding.clone(),
            resource,
            &self.cancel,
            decode,
        ) {
            Ok(Some(started)) => started,
            Ok(None) => return None,
            Err(ErrorCode::QuotaExceeded | ErrorCode::Backpressure) => {
                self.record_cooldown(key, now);
                return None;
            }
            Err(_) => return None,
        };
        self.pending.insert(key.clone());
        Some(StartedThumbnailFetch {
            plugin_id: owner,
            view,
            binding,
            generation,
            key,
            rx,
        })
    }

    /// Completes a background thumbnail fetch, returning `true` when the catalog view changed.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn finish_fetch(
        &mut self,
        ctrl: &PluginController,
        plugin_id: &str,
        view: Id,
        binding: &Arc<Binding>,
        generation: Generation,
        key: ThumbKey,
        outcome: Result<Result<BackgroundResult<RetainedThumbnail<T>>, ErrorCode>, Canceled>,
        now: Instant,
    ) -> bool {
        self.pending.remove(&key);
        let view_live = ctrl
            .active_browse_view()
            .is_some_and(|(cur_owner, cur_view)| cur_owner == plugin_id && *cur_view == view);
        if !binding.is_live() || !view_live || ctrl.generation().context != generation.context {
            return false;
        }
        match outcome {
            Ok(Ok(result)) => {
                self.cooldown.remove(&key);
                let retained = result.value;
                let changed = retained.image.is_some();
                while self.cache.len() >= MAX_CACHED_REMOTE_THUMBS
                    && let Some(oldest) = self.order.pop_front()
                {
                    self.cache.remove(&oldest);
                }
                self.order.retain(|k| k != &key);
                self.order.push_back(key.clone());
                self.cache.insert(key, retained);
                changed
            }
            Ok(Err(ErrorCode::QuotaExceeded | ErrorCode::Backpressure)) => {
                self.record_cooldown(key, now);
                false
            }
            Ok(Err(_)) | Err(_) => false,
        }
    }
}
