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
    time::{Duration, Instant},
};

const MAX_REMOTE_THUMB_CONCURRENT: usize = 3;
const REMOTE_THUMB_SCRATCH_BYTES: u64 = 4 << 20;
const REMOTE_THUMB_LEASE_BYTES: u64 = 128 << 10;
const QUOTA_RETRY_COOLDOWN: Duration = Duration::from_secs(2);

type Key = (String, String);
struct Cached {
    image: Option<Arc<RenderImage>>,
    _lease: Option<Reservation>,
}
impl Cached {
    fn new(image: Option<Arc<RenderImage>>, lease: Reservation) -> Self {
        let lease = image.is_some().then_some(lease);
        Self {
            image,
            _lease: lease,
        }
    }
}
#[derive(Default)]
pub(super) struct Thumbnails {
    cache: BTreeMap<Key, Cached>,
    order: VecDeque<Key>,
    pending: BTreeSet<Key>,
    cooldown: BTreeMap<Key, Instant>,
    cancel: HttpCancel,
}
impl Thumbnails {
    fn can_start(&self, key: &Key, now: Instant) -> bool {
        self.pending.len() < MAX_REMOTE_THUMB_CONCURRENT
            && !self.pending.contains(key)
            && self.cooldown.get(key).is_none_or(|&until| now >= until)
    }

    fn record_cooldown(&mut self, key: Key, now: Instant) {
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
        let now = Instant::now();
        if !self.plugin_view.thumbnails.can_start(&key, now) {
            return None;
        }
        let (owner, view) = self.plugin_view.view.clone()?;
        if owner != *plugin_id {
            return None;
        }
        let resource = self
            .plugin_source_entries(source_id)
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
        let result = match access.background(move |access| {
            if !live.is_live() {
                return Err(ErrorCode::Cancelled);
            }
            let scratch = access.resources.ledger.reserve(
                &access.owner,
                &[(Resource::HostBytes, REMOTE_THUMB_SCRATCH_BYTES)],
            )?;
            let lease = access.resources.ledger.reserve(
                &access.owner,
                &[(Resource::HostBytes, REMOTE_THUMB_LEASE_BYTES)],
            )?;
            let fetched =
                RemoteFactory::new(access.network.clone(), access.resources.ledger.clone())?
                    .fetch_thumbnail(&resource, &cancel);
            let image = fetched
                .ok()
                .and_then(|data| crate::platform::decode_remote_thumbnail(data.bytes()));
            drop(scratch);
            let cached = Cached::new(image, lease);
            if cancel.is_cancelled() || !live.is_live() {
                return Err(ErrorCode::Cancelled);
            }
            Ok(cached)
        }) {
            Ok(rx) => rx,
            Err(ErrorCode::QuotaExceeded | ErrorCode::Backpressure) => {
                self.plugin_view.thumbnails.record_cooldown(key, now);
                return None;
            }
            Err(_) => return None,
        };
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
                match result {
                    Ok(Ok(result)) => {
                        state.cooldown.remove(&key);
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
                    Ok(Err(ErrorCode::QuotaExceeded | ErrorCode::Backpressure)) => {
                        state.record_cooldown(key, Instant::now());
                    }
                    _ => {}
                }
            });
        })
        .detach();
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cached, MAX_REMOTE_THUMB_CONCURRENT, QUOTA_RETRY_COOLDOWN, REMOTE_THUMB_LEASE_BYTES,
        REMOTE_THUMB_SCRATCH_BYTES, Thumbnails,
    };
    use gpui::RenderImage;
    use image::{Frame, RgbaImage};
    use iris_core::plugin_v2::ledger::{Ledger, Resource};
    use std::{sync::Arc, time::Instant};

    #[test]
    fn thumbnails_allow_bounded_concurrency_and_release_lease_on_none() {
        assert_eq!(REMOTE_THUMB_SCRATCH_BYTES, 4 << 20);
        assert_eq!(REMOTE_THUMB_LEASE_BYTES, 128 << 10);

        let now = Instant::now();
        let mut thumbs = Thumbnails::default();
        let keys: Vec<_> = (0..=MAX_REMOTE_THUMB_CONCURRENT)
            .map(|i| ("src".to_owned(), format!("e{i}")))
            .collect();
        for key in &keys[..MAX_REMOTE_THUMB_CONCURRENT] {
            assert!(thumbs.can_start(key, now));
            thumbs.pending.insert(key.clone());
            assert!(!thumbs.can_start(key, now));
        }
        assert!(!thumbs.can_start(&keys[MAX_REMOTE_THUMB_CONCURRENT], now));
        thumbs.pending.remove(&keys[0]);
        assert!(thumbs.can_start(&keys[MAX_REMOTE_THUMB_CONCURRENT], now));

        thumbs.record_cooldown(keys[0].clone(), now);
        assert!(!thumbs.can_start(&keys[0], now));
        assert!(thumbs.can_start(&keys[0], now + QUOTA_RETRY_COOLDOWN));

        let ledger = Ledger::default();
        let lease_none = ledger
            .reserve("owner", &[(Resource::HostBytes, REMOTE_THUMB_LEASE_BYTES)])
            .unwrap();
        let neg = Cached::new(None, lease_none);
        assert!(neg.image.is_none());
        assert_eq!(ledger.used(Some("owner"), Resource::HostBytes), 0);

        let lease_some = ledger
            .reserve("owner", &[(Resource::HostBytes, REMOTE_THUMB_LEASE_BYTES)])
            .unwrap();
        let img = Arc::new(RenderImage::new([Frame::new(RgbaImage::new(1, 1))]));
        let pos = Cached::new(Some(img), lease_some);
        assert_eq!(
            ledger.used(Some("owner"), Resource::HostBytes),
            REMOTE_THUMB_LEASE_BYTES
        );
        drop(pos);
        assert_eq!(ledger.used(Some("owner"), Resource::HostBytes), 0);
    }
}
