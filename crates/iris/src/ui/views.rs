//! Regional GPUI `Render` entities (`Sidebar`, `PluginPanel`, `SettingsPanel`,
//! `ViewportView`, `DockView`), weak `ViewHandles` registry, `model_listener`
//! adapter, and localized `DirtyRegions` notification routing.

use gpui::{App, Context, Entity, IntoElement, Render, WeakEntity, Window};
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

/// Pure routing of plugin `DirtySlots` / `status_changed` to minimal regional GPUI entities.
#[allow(clippy::too_many_arguments)]
pub(in crate::ui) const fn dirty_regions_for_poll(
    dirty: DirtySlots,
    status_changed: bool,
    fullscreen: bool,
    chrome_visible: bool,
    sidebar_plugins_visible: bool,
    settings_open: bool,
    palette_open: bool,
) -> DirtyRegions {
    if dirty.theme {
        return DirtyRegions::ALL;
    }
    let dock_visible = !fullscreen || chrome_visible;
    let mut out = DirtyRegions::NONE;
    if dirty.sidebar && sidebar_plugins_visible {
        out.plugin_panel = true;
    }
    if (dirty.status || dirty.toolbar) && dock_visible {
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
        out.settings |= settings_open;
        out.dock |= dock_visible;
    }
    if status_changed {
        out.sidebar = true;
        out.plugin_panel |= sidebar_plugins_visible;
        out.settings |= settings_open;
        out.root |= palette_open;
        out.viewport = true;
        out.dock |= dock_visible;
    }
    out
}

/// Pure routing of 1–4Hz playback timeline/buffer ticks to minimal regional GPUI entities.
pub(in crate::ui) const fn dirty_regions_for_playback_tick(
    kind: Option<Kind>,
    info_refreshed: bool,
    dock_visible: bool,
) -> DirtyRegions {
    DirtyRegions {
        root: false,
        sidebar: false,
        plugin_panel: false,
        settings: false,
        viewport: matches!(kind, Some(Kind::Audio)) || info_refreshed,
        dock: dock_visible,
    }
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
        DirtyRegions, dirty_regions_for_playback_tick, dirty_regions_for_poll,
        dirty_regions_for_slot_edit,
    };
    use crate::ui::kit::text_field::ActiveSlot;
    use iris_core::library::Kind;
    use iris_plugin_host::DirtySlots;

    #[test]
    fn regional_dirty_routing_isolates_plugin_slots_ticks_and_text_inputs() {
        let sb_slot = DirtySlots {
            sidebar: true,
            ..Default::default()
        };
        assert_eq!(
            dirty_regions_for_poll(sb_slot, false, false, true, true, false, false),
            DirtyRegions::plugin_panel_only()
        );
        assert_eq!(
            dirty_regions_for_poll(sb_slot, false, false, true, false, false, false),
            DirtyRegions::NONE
        );

        let dock_slot = DirtySlots {
            status: true,
            toolbar: true,
            ..Default::default()
        };
        assert_eq!(
            dirty_regions_for_poll(dock_slot, false, false, true, true, true, false),
            DirtyRegions::dock_only()
        );
        assert_eq!(
            dirty_regions_for_poll(dock_slot, false, true, false, false, false, false),
            DirtyRegions::NONE
        );

        assert_eq!(
            dirty_regions_for_poll(
                DirtySlots {
                    overlay: true,
                    ..Default::default()
                },
                false,
                false,
                true,
                true,
                true,
                false,
            ),
            DirtyRegions::viewport_only()
        );
        assert_eq!(
            dirty_regions_for_poll(
                DirtySlots {
                    modal: true,
                    ..Default::default()
                },
                false,
                false,
                true,
                true,
                true,
                false,
            ),
            DirtyRegions::root_only()
        );

        assert_eq!(
            dirty_regions_for_playback_tick(Some(Kind::Video), false, true),
            DirtyRegions::dock_only()
        );
        assert_eq!(
            dirty_regions_for_playback_tick(Some(Kind::Video), false, false),
            DirtyRegions::NONE
        );
        assert_eq!(
            dirty_regions_for_playback_tick(Some(Kind::Audio), false, true),
            DirtyRegions {
                viewport: true,
                dock: true,
                ..DirtyRegions::NONE
            }
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
}
