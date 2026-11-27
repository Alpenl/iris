//! Regional GPUI `Render` entities (`Sidebar`, `PluginPanel`, `SettingsPanel`,
//! `ViewportView`, `DockView`), weak `ViewHandles` registry, `model_listener`
//! adapter, and centralized domain-to-`DirtyRegions` pure mapping functions.

use gpui::{App, Context, Entity, IntoElement, Render, WeakEntity, Window};
use iris_app::{PlaybackDomainChanges, PluginDomainChanges};
use iris_core::library::Kind;
use iris_plugin_host::DirtySlots;

use super::kit::text_field::ActiveSlot;
use super::{AppModel, IrisWindow};

/// Bitmask-like regional dirty set routing state changes to only the affected GPUI entities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ui) struct DirtyRegions {
    pub(in crate::ui) root: bool,
    pub(in crate::ui) sidebar: bool,
    pub(in crate::ui) plugin_panel: bool,
    pub(in crate::ui) settings: bool,
    pub(in crate::ui) viewport: bool,
    pub(in crate::ui) dock: bool,
}

impl DirtyRegions {
    pub(in crate::ui) const NONE: Self = Self {
        root: false,
        sidebar: false,
        plugin_panel: false,
        settings: false,
        viewport: false,
        dock: false,
    };

    pub(in crate::ui) const ALL: Self = Self {
        root: true,
        sidebar: true,
        plugin_panel: true,
        settings: true,
        viewport: true,
        dock: true,
    };

    pub(in crate::ui) const fn root_only() -> Self {
        Self {
            root: true,
            ..Self::NONE
        }
    }

    pub(in crate::ui) const fn sidebar_only() -> Self {
        Self {
            sidebar: true,
            ..Self::NONE
        }
    }

    pub(in crate::ui) const fn plugin_panel_only() -> Self {
        Self {
            plugin_panel: true,
            ..Self::NONE
        }
    }

    pub(in crate::ui) const fn settings_only() -> Self {
        Self {
            settings: true,
            ..Self::NONE
        }
    }

    pub(in crate::ui) const fn viewport_only() -> Self {
        Self {
            viewport: true,
            ..Self::NONE
        }
    }

    pub(in crate::ui) const fn dock_only() -> Self {
        Self {
            dock: true,
            ..Self::NONE
        }
    }

    pub(in crate::ui) const fn any(self) -> bool {
        self.root
            || self.sidebar
            || self.plugin_panel
            || self.settings
            || self.viewport
            || self.dock
    }

    pub(in crate::ui) const fn union(self, other: Self) -> Self {
        Self {
            root: self.root || other.root,
            sidebar: self.sidebar || other.sidebar,
            plugin_panel: self.plugin_panel || other.plugin_panel,
            settings: self.settings || other.settings,
            viewport: self.viewport || other.viewport,
            dock: self.dock || other.dock,
        }
    }
}

/// Snapshot of current UI chrome/panel visibility owned strictly by the GPUI layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ui) struct UiVisibility {
    pub(in crate::ui) dock_visible: bool,
    pub(in crate::ui) sidebar_plugins_visible: bool,
    pub(in crate::ui) settings_open: bool,
    pub(in crate::ui) palette_open: bool,
    pub(in crate::ui) popover_open: bool,
}

/// Centralized pure mapping from headless plugin batch changes (`DirtySlots`, `status_changed`,
/// [`PluginDomainChanges`]) onto regional GPUI [`DirtyRegions`].
pub(in crate::ui) const fn dirty_regions_for_plugin_outcome(
    dirty: DirtySlots,
    status_changed: bool,
    domain: &PluginDomainChanges,
    vis: UiVisibility,
) -> DirtyRegions {
    if dirty.theme || domain.theme_changed || domain.close_overlays || domain.uninstalled_changed {
        return DirtyRegions::ALL;
    }
    let mut out = DirtyRegions::NONE;
    if dirty.sidebar && vis.sidebar_plugins_visible {
        out.plugin_panel = true;
    }
    if (dirty.status || dirty.toolbar) && vis.dock_visible {
        out.dock = true;
    }
    if dirty.overlay {
        out.viewport = true;
    }
    if dirty.modal {
        out.root = true;
    }
    if dirty.sources {
        out.root = true;
        out.sidebar = true;
        out.viewport = true;
        out.settings |= vis.settings_open;
        out.dock |= vis.dock_visible;
    }
    if status_changed {
        out.sidebar = true;
        out.plugin_panel |= vis.sidebar_plugins_visible;
        out.settings |= vis.settings_open;
        out.root |= vis.palette_open;
        out.viewport = true;
        out.dock |= vis.dock_visible;
    }
    if domain.stream_meta_cleared {
        out.root = true;
        out.dock = true;
    }
    if domain.favorites_changed {
        out.sidebar = true;
        out.viewport = true;
        out.dock = true;
    } else if domain.catalog_changed {
        out.root = true;
        out.sidebar = true;
        out.viewport = true;
        out.dock = true;
    }
    if domain.consent_changed || domain.modal_changed {
        out.root = true;
    }
    if domain.playback_changed || domain.timeline_changed || domain.track_changed {
        out.dock = true;
    }
    if domain.lut_changed || domain.overlay_changed {
        out.viewport = true;
    }
    if domain.annotations_changed {
        out.viewport = true;
        out.dock = true;
    }
    out
}

/// Centralized pure mapping from headless [`PlaybackDomainChanges`] onto regional GPUI [`DirtyRegions`].
pub(in crate::ui) const fn dirty_regions_for_playback(
    changes: &PlaybackDomainChanges,
    timeline_ticked: bool,
    stats_refreshed: bool,
    vis: UiVisibility,
) -> DirtyRegions {
    let mut out = DirtyRegions::NONE;
    let is_audio = matches!(changes.media_kind, Some(Kind::Audio));

    if changes.loaded_or_reconfigured {
        out.root = true;
        out.sidebar = true;
        out.viewport = true;
        out.dock = true;
    }
    if changes.other_session_changed || changes.buffering_changed {
        out.root = true;
        out.viewport = true;
        out.dock = true;
    }
    if changes.ended_settled {
        out.sidebar = true;
        out.viewport = true;
        out.dock = true;
    }
    if changes.error_surfaced {
        out.viewport = true;
        out.dock = true;
    }
    if changes.duration_changed || changes.pause_or_speed_changed {
        out.dock = true;
        out.viewport |= is_audio;
    }
    if changes.seeked
        || changes.audio_track_changed
        || changes.subtitle_track_changed
        || changes.volume_changed
    {
        out.dock = true;
    }
    if timeline_ticked {
        out.viewport |= is_audio || stats_refreshed;
        out.dock |= vis.dock_visible;
    } else if stats_refreshed {
        out.viewport = true;
    }
    if vis.popover_open && changes.affects_transport_popover() {
        out.root = true;
    }
    out
}

/// Pure routing of text field edits/caret moves to the entity owning that input slot.
pub(in crate::ui) fn dirty_regions_for_slot_edit(
    slot: ActiveSlot,
    active_form_key: Option<&str>,
) -> DirtyRegions {
    match slot {
        ActiveSlot::Search => DirtyRegions::sidebar_only(),
        ActiveSlot::Palette => DirtyRegions::root_only(),
        ActiveSlot::Form => {
            if active_form_key.is_some_and(|k| k.starts_with("sb:")) {
                DirtyRegions::plugin_panel_only()
            } else {
                DirtyRegions::root_only()
            }
        }
    }
}

/// Weak handles to every regional GPUI `Render` entity owned by the window.
#[derive(Default)]
pub(in crate::ui) struct ViewHandles {
    pub(in crate::ui) root: Option<WeakEntity<IrisWindow>>,
    pub(in crate::ui) docked_sidebar: Option<WeakEntity<Sidebar>>,
    pub(in crate::ui) floating_sidebar: Option<WeakEntity<Sidebar>>,
    pub(in crate::ui) plugin_panel: Option<WeakEntity<PluginPanel>>,
    pub(in crate::ui) settings: Option<WeakEntity<SettingsPanel>>,
    pub(in crate::ui) viewport: Option<WeakEntity<ViewportView>>,
    pub(in crate::ui) docked_dock: Option<WeakEntity<DockView>>,
    pub(in crate::ui) floating_dock: Option<WeakEntity<DockView>>,
}

impl ViewHandles {
    pub(in crate::ui) fn notify(&self, regions: DirtyRegions, cx: &mut App) {
        fn ping<V: 'static>(slot: &Option<WeakEntity<V>>, cx: &mut App) {
            if let Some(entity) = slot.as_ref().and_then(WeakEntity::upgrade) {
                cx.notify(entity.entity_id());
            }
        }
        if regions.root {
            ping(&self.root, cx);
        }
        if regions.sidebar {
            ping(&self.docked_sidebar, cx);
            ping(&self.floating_sidebar, cx);
        }
        if regions.plugin_panel {
            ping(&self.plugin_panel, cx);
        }
        if regions.settings {
            ping(&self.settings, cx);
        }
        if regions.viewport {
            ping(&self.viewport, cx);
        }
        if regions.dock {
            ping(&self.docked_dock, cx);
            ping(&self.floating_dock, cx);
        }
    }
}

/// Bridges any GPUI element/action callback onto `Entity<AppModel>` regardless of the rendering view.
pub(in crate::ui) fn model_listener<E: 'static>(
    model: &Entity<AppModel>,
    f: impl Fn(&mut AppModel, &E, &mut Window, &mut Context<AppModel>) + 'static,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let weak = model.downgrade();
    move |ev, window, cx| {
        let _ = weak.update(cx, |this, cx| f(this, ev, window, cx));
    }
}

/// Independent GPUI entity for plugin sidebar panels (`SidebarMode::Plugins`).
pub(in crate::ui) struct PluginPanel {
    pub(in crate::ui) model: Entity<AppModel>,
}

impl Render for PluginPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::plugin_ui::render_plugin_sidebar_entity(&self.model, window, cx)
    }
}

/// Independent GPUI entity for the docked or fullscreen-floating workbench sidebar.
pub(in crate::ui) struct Sidebar {
    pub(in crate::ui) model: Entity<AppModel>,
    pub(in crate::ui) plugin_panel: Entity<PluginPanel>,
    pub(in crate::ui) floating: bool,
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_sidebar_entity(window, cx)
    }
}

/// Independent GPUI entity for the 7-tab centered settings modal.
pub(in crate::ui) struct SettingsPanel {
    pub(in crate::ui) model: Entity<AppModel>,
}

impl Render for SettingsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::settings::render_settings_entity(&self.model, window, cx)
    }
}

/// Independent GPUI entity for the responsive transport & timeline control bar.
pub(in crate::ui) struct DockView {
    pub(in crate::ui) model: Entity<AppModel>,
    pub(in crate::ui) floating: bool,
}

impl Render for DockView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::controls::render_dock_entity(&self.model, self.floating, window, cx)
    }
}

/// Independent GPUI entity for the media viewport, HUD overlays, and floating fullscreen chrome.
pub(in crate::ui) struct ViewportView {
    pub(in crate::ui) model: Entity<AppModel>,
    pub(in crate::ui) floating_sidebar: Entity<Sidebar>,
    pub(in crate::ui) floating_dock: Entity<DockView>,
}

impl Render for ViewportView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_viewport_entity(window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DirtyRegions, UiVisibility, dirty_regions_for_playback, dirty_regions_for_plugin_outcome,
        dirty_regions_for_slot_edit,
    };
    use crate::ui::kit::text_field::ActiveSlot;
    use iris_app::{PlaybackDomainChanges, PluginDomainChanges};
    use iris_core::library::Kind;
    use iris_plugin_host::DirtySlots;

    #[test]
    fn regional_dirty_routing_isolates_plugin_outcomes_and_text_inputs() {
        let vis_plugins = UiVisibility {
            dock_visible: true,
            sidebar_plugins_visible: true,
            ..Default::default()
        };
        let vis_files_fs_hidden = UiVisibility::default();
        let empty_dom = PluginDomainChanges::default();

        let sb_slot = DirtySlots {
            sidebar: true,
            ..Default::default()
        };
        assert_eq!(
            dirty_regions_for_plugin_outcome(sb_slot, false, &empty_dom, vis_plugins),
            DirtyRegions::plugin_panel_only()
        );
        assert_eq!(
            dirty_regions_for_plugin_outcome(sb_slot, false, &empty_dom, vis_files_fs_hidden),
            DirtyRegions::NONE
        );

        let dock_slot = DirtySlots {
            status: true,
            toolbar: true,
            ..Default::default()
        };
        assert_eq!(
            dirty_regions_for_plugin_outcome(dock_slot, false, &empty_dom, vis_plugins),
            DirtyRegions::dock_only()
        );
        assert_eq!(
            dirty_regions_for_plugin_outcome(dock_slot, false, &empty_dom, vis_files_fs_hidden),
            DirtyRegions::NONE
        );

        assert_eq!(
            dirty_regions_for_plugin_outcome(
                DirtySlots {
                    overlay: true,
                    ..Default::default()
                },
                false,
                &empty_dom,
                vis_plugins,
            ),
            DirtyRegions::viewport_only()
        );
        assert_eq!(
            dirty_regions_for_plugin_outcome(
                DirtySlots {
                    modal: true,
                    ..Default::default()
                },
                false,
                &empty_dom,
                vis_plugins,
            ),
            DirtyRegions::root_only()
        );

        assert_eq!(
            dirty_regions_for_slot_edit(ActiveSlot::Search, None),
            DirtyRegions::sidebar_only()
        );
        assert_eq!(
            dirty_regions_for_slot_edit(ActiveSlot::Palette, None),
            DirtyRegions::root_only()
        );
        assert_eq!(
            dirty_regions_for_slot_edit(ActiveSlot::Form, Some("sb:iris.guides:q")),
            DirtyRegions::plugin_panel_only()
        );
        assert_eq!(
            dirty_regions_for_slot_edit(ActiveSlot::Form, Some("modal:iris.webdav:auth:url")),
            DirtyRegions::root_only()
        );
    }

    #[test]
    fn fixed_playback_sequence_load_pause_seek_speed_subtitle_prevents_over_invalidation() {
        let vis = UiVisibility {
            dock_visible: true,
            sidebar_plugins_visible: true,
            settings_open: true,
            palette_open: false,
            popover_open: false,
        };

        // 1. Load: updates root/sidebar/viewport/dock, never invalidating plugin_panel or settings!
        let load = PlaybackDomainChanges {
            media_kind: Some(Kind::Video),
            loaded_or_reconfigured: true,
            ..Default::default()
        };
        assert_eq!(
            dirty_regions_for_playback(&load, false, false, vis),
            DirtyRegions {
                root: true,
                sidebar: true,
                viewport: true,
                dock: true,
                ..DirtyRegions::NONE
            }
        );

        // 2. Pause (Video): updates dock ONLY! (Audio also updates viewport card.)
        let pause_video = PlaybackDomainChanges {
            media_kind: Some(Kind::Video),
            pause_or_speed_changed: true,
            ..Default::default()
        };
        assert_eq!(
            dirty_regions_for_playback(&pause_video, false, false, vis),
            DirtyRegions::dock_only()
        );
        let pause_audio = PlaybackDomainChanges {
            media_kind: Some(Kind::Audio),
            pause_or_speed_changed: true,
            ..Default::default()
        };
        assert_eq!(
            dirty_regions_for_playback(&pause_audio, false, false, vis),
            DirtyRegions {
                viewport: true,
                dock: true,
                ..DirtyRegions::NONE
            }
        );

        // 3. Seek: updates dock ONLY!
        let seek = PlaybackDomainChanges {
            media_kind: Some(Kind::Video),
            seeked: true,
            ..Default::default()
        };
        assert_eq!(
            dirty_regions_for_playback(&seek, false, false, vis),
            DirtyRegions::dock_only()
        );

        // 4. Speed: updates dock ONLY when popover closed, and root + dock when Speed popover is open!
        let speed = PlaybackDomainChanges {
            media_kind: Some(Kind::Video),
            pause_or_speed_changed: true,
            ..Default::default()
        };
        assert_eq!(
            dirty_regions_for_playback(&speed, false, false, vis),
            DirtyRegions::dock_only()
        );
        assert_eq!(
            dirty_regions_for_playback(
                &speed,
                false,
                false,
                UiVisibility {
                    popover_open: true,
                    ..vis
                }
            ),
            DirtyRegions {
                root: true,
                dock: true,
                ..DirtyRegions::NONE
            }
        );

        // 5. Subtitle switch: updates dock ONLY when popover closed, and root + dock when Subtitles popover is open!
        let sub_switch = PlaybackDomainChanges {
            media_kind: Some(Kind::Video),
            subtitle_track_changed: true,
            ..Default::default()
        };
        assert_eq!(
            dirty_regions_for_playback(&sub_switch, false, false, vis),
            DirtyRegions::dock_only()
        );

        // 6. 1-4Hz playback timeline tick: dock ONLY when dock visible, NONE when fullscreen chrome hidden!
        let idle = PlaybackDomainChanges {
            media_kind: Some(Kind::Video),
            ..Default::default()
        };
        assert_eq!(
            dirty_regions_for_playback(&idle, true, false, vis),
            DirtyRegions::dock_only()
        );
        assert_eq!(
            dirty_regions_for_playback(&idle, true, false, UiVisibility::default()),
            DirtyRegions::NONE
        );
    }
}
