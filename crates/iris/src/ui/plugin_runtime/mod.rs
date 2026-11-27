//! Window-owned GPUI presentation adapters (`PluginUiState`, `GlyphCache`, view
//! timers, remote thumbnail executor tasks, and consent/annotation/overlay views)
//! delegating all plugin state and orchestration to [`iris_app::PluginController`].

pub(in crate::ui) mod annotations;
mod consent;
pub(in crate::ui) use consent::render_host_consent;
pub(in crate::ui) mod overlay;
pub(in crate::ui) mod sources;
pub(in crate::ui) use sources::remote_search_query;
mod thumbnails;
pub(in crate::ui) mod timeline;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{Context, RenderImage, SharedString};
use iris_app::{ThumbnailCache, ViewTimerSlot, run_view_timer};
use iris_core::preset::SidebarMode;

use super::AppModel;
use crate::assets::Icon;

/// Maps a plugin-declared `SourceDescriptor::icon` name onto a controlled host [`Icon`].
pub(in crate::ui) fn source_icon_for_name(name: Option<&str>) -> Icon {
    match name.map(str::trim) {
        Some(n) if n.eq_ignore_ascii_case("globe") || n.eq_ignore_ascii_case("server") => {
            Icon::Globe
        }
        Some(n) if n.eq_ignore_ascii_case("folder") || n.eq_ignore_ascii_case("cloud") => {
            Icon::Folder
        }
        Some(n) if n.eq_ignore_ascii_case("link") => Icon::Link,
        Some(n) => Icon::from_name(n).unwrap_or(Icon::Puzzle),
        None => Icon::Puzzle,
    }
}

/// Pure GPUI-handle state (`gpui::Task`, shaped `GlyphCache`, decoded `RenderImage`
/// thumbnail cache, and local marker pagination) retained alongside `PluginController`.
#[derive(Default)]
pub(in crate::ui) struct PluginUiState {
    pub(super) view_timers: BTreeMap<ViewTimerSlot, ((String, u32), gpui::Task<()>)>,
    pub(in crate::ui) browse_timeout: Option<gpui::Task<()>>,
    pub(in crate::ui) resolve_timeout: Option<gpui::Task<()>>,
    pub(in crate::ui) overlay_wake: Option<gpui::Task<()>>,
    pub(super) glyphs: overlay::GlyphCache,
    pub(super) thumbnails: ThumbnailCache<Arc<RenderImage>>,
    pub(super) marker_page: usize,
}

pub(super) fn desired_view_timers_for_ui(
    modal_timer: Option<(String, String, u32)>,
    sidebar_timers: Vec<(String, String, u32)>,
    sidebar_plugins_visible: bool,
) -> BTreeMap<ViewTimerSlot, (String, u32)> {
    let mut out = BTreeMap::new();
    if let Some((pid, action_id, interval_ms)) = modal_timer {
        out.insert(ViewTimerSlot::Modal(pid), (action_id, interval_ms));
    }
    if sidebar_plugins_visible {
        for (pid, action_id, interval_ms) in sidebar_timers {
            out.insert(ViewTimerSlot::Sidebar(pid), (action_id, interval_ms));
        }
    }
    out
}

impl AppModel {
    pub(in crate::ui) fn plugin_source_icon(&self, plugin_id: &str, source_id: &str) -> Icon {
        source_icon_for_name(
            self.plugins
                .source_icon_name(plugin_id, source_id)
                .as_deref(),
        )
    }

    pub(in crate::ui) fn plugin_source_badge(
        &self,
        plugin_id: &str,
        source_id: &str,
    ) -> SharedString {
        self.plugins.source_badge(plugin_id, source_id).into()
    }

    pub(in crate::ui) fn plugin_source_entries<'a>(
        &'a self,
        source_id: &'a str,
    ) -> impl Iterator<Item = &'a iris_api::v2::source::SourceEntry> {
        self.plugins.source_entries(source_id)
    }

    pub(super) fn close_plugin_browse(&mut self) {
        let out = self.plugins.close_browse(&mut self.catalog.active_source);
        self.plugin_ui.browse_timeout = None;
        if out
            .effects
            .contains(&iris_app::PluginSideEffect::ResetThumbnails)
        {
            self.plugin_ui.thumbnails.reset();
        }
    }

    pub(super) fn advance_plugin_media(&mut self, cx: &mut Context<Self>) -> bool {
        let (ok, out) = self.plugins.advance_media(self.playback.session_mut());
        let regions = self.apply_plugin_outcome(out, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
        ok
    }

    pub(super) fn advance_plugin_seek(&mut self, cx: &mut Context<Self>) {
        let out = self.with_plugin_ctx(|ctrl, ctx| ctrl.advance_seek(ctx, Instant::now()));
        let regions = self.apply_plugin_outcome(out, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }

    pub(super) fn announce_plugin_media_if_loaded(
        &mut self,
        cx: &mut Context<Self>,
    ) -> super::views::DirtyRegions {
        let out =
            self.with_plugin_ctx(|ctrl, ctx| ctrl.announce_media_if_loaded(ctx, Instant::now()));
        self.apply_plugin_outcome(out, None, cx)
    }

    fn desired_view_timers(&self) -> BTreeMap<ViewTimerSlot, (String, u32)> {
        let sidebar_visible =
            self.sidebar_is_visible() && self.catalog.sidebar_mode == SidebarMode::Plugins;
        let modal_timer = self.plugins.active_modal_timer();
        let sidebar_timers = if sidebar_visible {
            self.plugins.sidebar_panel_timers()
        } else {
            Vec::new()
        };
        desired_view_timers_for_ui(modal_timer, sidebar_timers, sidebar_visible)
    }

    pub(super) fn sync_view_timer(&mut self, cx: &mut Context<Self>) {
        let desired = self.desired_view_timers();
        self.plugin_ui
            .view_timers
            .retain(|slot, (spec, _)| desired.get(slot) == Some(spec));
        for (slot, (act, interval_ms)) in desired {
            if self.plugin_ui.view_timers.contains_key(&slot) {
                continue;
            }
            let pid = match &slot {
                ViewTimerSlot::Modal(p) | ViewTimerSlot::Sidebar(p) => p.clone(),
            };
            let expected_slot = slot.clone();
            let expected_spec = (act.clone(), interval_ms);
            let interval = Duration::from_millis(u64::from(interval_ms));
            let task = cx.spawn(async move |this, cx| {
                let executor = cx.background_executor().clone();
                run_view_timer(
                    || executor.timer(interval),
                    || {
                        this.update(cx, |this, cx| {
                            if this.desired_view_timers().get(&expected_slot)
                                != Some(&expected_spec)
                            {
                                this.plugin_ui.view_timers.remove(&expected_slot);
                                return false;
                            }
                            let out = this.with_plugin_ctx(|ctrl, ctx| {
                                ctrl.fire_view_timer_tick(
                                    ctx,
                                    &pid,
                                    &expected_spec.0,
                                    Instant::now(),
                                )
                            });
                            let regions = this.apply_plugin_outcome(out, None, cx);
                            if regions.any() {
                                this.notify_regions(regions, cx);
                            }
                            true
                        })
                        .unwrap_or(false)
                    },
                )
                .await;
            });
            self.plugin_ui
                .view_timers
                .insert(slot, ((act, interval_ms), task));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::desired_view_timers_for_ui;
    use iris_app::ViewTimerSlot;

    #[test]
    fn ui_view_timers_gate_sidebar_panels_on_plugins_sidebar_visibility() {
        let modal = Some(("plugin.a".to_owned(), "poll_qr".to_owned(), 1_500));
        let panels = vec![
            ("plugin.a".to_owned(), "tick_a".to_owned(), 2_000),
            ("plugin.b".to_owned(), "tick_b".to_owned(), 1_000),
        ];

        let hidden = desired_view_timers_for_ui(modal.clone(), panels.clone(), false);
        assert_eq!(hidden.len(), 1);
        assert_eq!(
            hidden.get(&ViewTimerSlot::Modal("plugin.a".into())),
            Some(&("poll_qr".to_owned(), 1_500))
        );

        let visible = desired_view_timers_for_ui(modal, panels, true);
        assert_eq!(visible.len(), 3);
        assert_eq!(
            visible.get(&ViewTimerSlot::Sidebar("plugin.b".into())),
            Some(&("tick_b".to_owned(), 1_000))
        );
    }
}
