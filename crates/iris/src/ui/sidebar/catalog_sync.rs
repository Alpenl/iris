//! Thin GPUI catalog synchronization bridge (`rebuild_directory_rows`,
//! `scroll_directory_to_current`, `scan_visible_favorites`, `submit_directory_search`)
//! delegating domain row & generation management to [`iris_app::AppModel`].

use std::time::Instant;

use gpui::{Context, ScrollStrategy};
use iris_core::catalog::MediaFilter;

use super::super::WindowShell;

impl WindowShell {
    pub(in crate::ui) fn submit_directory_search(&mut self, cx: &mut Context<Self>) {
        let query = if self.text_inputs.search_open {
            self.text_inputs.search_input.text().to_owned()
        } else {
            String::new()
        };
        if let Some((_, out)) = self.app.submit_directory_search(query, Instant::now()) {
            let regions = self.apply_plugin_outcome(out, None, cx);
            if regions.any() {
                self.notify_regions(regions, cx);
            }
        } else {
            self.notify_sidebar(cx);
        }
    }

    pub(in crate::ui) fn scroll_directory_to_current(&self) {
        let cur = self.session().current_media_uri();
        if let Some(ix) = self.catalog().current_row_index(cur.as_ref()) {
            self.catalog_ui
                .dir_scroll
                .scroll_to_item(ix, ScrollStrategy::Center);
        }
    }

    pub(in crate::ui) fn rebuild_directory_rows(&mut self) {
        let query = if self.text_inputs.search_open {
            self.text_inputs.search_input.text().to_owned()
        } else {
            String::new()
        };
        self.app.reset_search_and_rebuild(query);
        if !self.catalog().is_favorite_scan_in_flight() {
            self.catalog_ui.favorite_scan = None;
        }
    }

    pub(in crate::ui) fn scan_visible_favorites(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.app.begin_favorite_scan() else {
            return;
        };
        let scan = cx.background_executor().spawn(async move { job.run() });
        self.catalog_ui.favorite_scan = Some(cx.spawn(async move |this, cx| {
            let completed = scan.await;
            let _ = this.update(cx, |this, cx| {
                if this.app.finish_favorite_scan(completed) {
                    this.catalog_ui.favorite_scan = None;
                    if this.catalog().filter() == MediaFilter::FavoritesOnly {
                        this.notify_sidebar(cx);
                    }
                }
            });
        }));
    }

    pub(in crate::ui) fn filter_directory_rows(&mut self) {
        let query = if self.text_inputs.search_open {
            self.text_inputs.search_input.text().to_owned()
        } else {
            String::new()
        };
        self.app.set_search_query(query);
    }
}
