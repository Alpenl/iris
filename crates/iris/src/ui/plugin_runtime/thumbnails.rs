use super::super::IrisWindow;
use gpui::{Context, RenderImage};
use iris_api::v2::{Capability, ErrorCode};
use iris_core::{
    media_uri::MediaUri,
    plugin_v2::{
        ledger::{Reservation, Resource},
        stream::remote::RemoteFactory,
    },
    vfs::HttpCancel,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

type Key = (String, String);
struct Cached {
    image: Option<Arc<RenderImage>>,
    _lease: Reservation,
}
#[derive(Default)]
pub(super) struct Thumbnails {
    cache: BTreeMap<Key, Cached>,
    order: VecDeque<Key>,
    pending: BTreeSet<Key>,
    cancel: HttpCancel,
}
impl Drop for Thumbnails {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl IrisWindow {
    pub(in crate::ui) fn plugin_thumbnail(
        &mut self,
        uri: &MediaUri,
        cx: &mut Context<Self>,
    ) -> Option<Arc<RenderImage>> {
        let MediaUri::Plugin {
            plugin_id,
            source_id,
            entry_id,
        } = uri
        else {
            return None;
        };
        let binding = self.plugin_view.page_binding.clone()?;
        if !binding.is_live() || binding.installed.plugin_id != *plugin_id {
            return None;
        }
        let key = (source_id.clone(), entry_id.clone());
        if let Some(value) = self.plugin_view.thumbnails.cache.get(&key) {
            return value.image.clone();
        }
        if !self.plugin_view.thumbnails.pending.is_empty()
            || self.plugin_view.thumbnails.pending.contains(&key)
        {
            return None;
        }
        let (owner, view) = self.plugin_view.view.clone()?;
        if owner != *plugin_id {
            return None;
        }
        let resource = self
            .plugin_view
            .pages
            .iter()
            .filter(|p| p.page.source_id == *source_id)
            .flat_map(|p| &p.page.entries)
            .find(|e| e.id == *entry_id)?
            .thumbnail
            .clone()?;
        let access = self.plugins.client().access(plugin_id).ok()?;
        let generation = access.generation;
        access.check(generation, Capability::MediaSource).ok()?;
        let cancel = access
            .request_cancel(&self.plugin_view.thumbnails.cancel)
            .ok()?;
        let live = binding.clone();
        let result = access
            .background(move |access| {
                if !live.is_live() {
                    return Err(ErrorCode::Cancelled);
                }
                let scratch = access
                    .resources
                    .ledger
                    .reserve(&access.owner, &[(Resource::HostBytes, 16 << 20)])?;
                let lease = access
                    .resources
                    .ledger
                    .reserve(&access.owner, &[(Resource::HostBytes, 128 << 10)])?;
                let fetched =
                    RemoteFactory::new(access.network.clone(), access.resources.ledger.clone())?
                        .fetch_thumbnail(&resource, &cancel);
                let image = fetched
                    .ok()
                    .and_then(|data| crate::platform::decode_remote_thumbnail(data.bytes()));
                drop(scratch);
                if cancel.is_cancelled() || !live.is_live() {
                    return Err(ErrorCode::Cancelled);
                }
                Ok(Cached {
                    image,
                    _lease: lease,
                })
            })
            .ok()?;
        self.plugin_view.thumbnails.pending.insert(key.clone());
        cx.spawn(async move |this, cx| {
            let result = result.await;
            let _ = this.update(cx, |this, cx| {
                if !binding.is_live()
                    || this.plugins.generation().context != generation.context
                    || this
                        .plugin_view
                        .view
                        .as_ref()
                        .is_none_or(|(p, v)| p != &owner || *v != view)
                {
                    return;
                }
                let state = &mut this.plugin_view.thumbnails;
                state.pending.remove(&key);
                if let Ok(Ok(result)) = result {
                    while state.cache.len() >= 96 {
                        if let Some(old) = state.order.pop_front() {
                            state.cache.remove(&old);
                        } else {
                            break;
                        }
                    }
                    state.order.push_back(key.clone());
                    state.cache.insert(key, result.value);
                    cx.notify();
                }
            });
        })
        .detach();
        None
    }
}
