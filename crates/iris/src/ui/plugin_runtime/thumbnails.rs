use std::sync::Arc;
use std::time::Instant;

use gpui::{Context, RenderImage};
use iris_core::media_uri::MediaUri;

use super::super::WindowShell;

impl WindowShell {
    /// Side-effect-free lookup of a decoded remote plugin thumbnail from cache.
    pub(in crate::ui) fn cached_plugin_thumbnail(
        &self,
        uri: &MediaUri,
    ) -> Option<Arc<RenderImage>> {
        self.plugin_ui
            .thumbnails
            .cached(self.app.plugins(), uri)
            .cloned()
    }

    /// Side-effect-free check whether `uri` is eligible to schedule a remote thumbnail fetch.
    pub(in crate::ui) fn needs_plugin_thumbnail(&self, uri: &MediaUri) -> bool {
        self.plugin_ui
            .thumbnails
            .needs_fetch(self.app.plugins(), uri, Instant::now())
    }

    /// Schedules bounded background fetches for uncached visible remote thumbnails post-frame.
    pub(in crate::ui) fn request_plugin_thumbnails(
        &mut self,
        uris: Vec<MediaUri>,
        cx: &mut Context<Self>,
    ) {
        for uri in uris {
            self.start_plugin_thumbnail(&uri, cx);
        }
    }

    fn start_plugin_thumbnail(&mut self, uri: &MediaUri, cx: &mut Context<Self>) {
        let Some(started) = self.plugin_ui.thumbnails.begin_fetch(
            self.app.plugins(),
            uri,
            Instant::now(),
            crate::platform::decode_remote_thumbnail,
        ) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = started.rx.await;
            let _ = this.update(cx, |this, cx| {
                if this.plugin_ui.thumbnails.finish_fetch(
                    this.app.plugins(),
                    &started.plugin_id,
                    started.view,
                    &started.binding,
                    started.generation,
                    started.key,
                    result,
                    Instant::now(),
                ) {
                    this.notify_sidebar(cx);
                }
            });
        })
        .detach();
    }
}
