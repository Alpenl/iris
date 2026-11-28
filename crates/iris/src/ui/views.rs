//! Regional GPUI `Render` entities (`Sidebar`, `PluginPanel`, `SettingsPanel`,
//! `ViewportView`, `DockView`), weak `ViewHandles` registry, `model_listener`
//! adapter, and centralized domain-to-`DirtyRegions` pure mapping functions.

use gpui::{App, Context, Entity, IntoElement, Render, WeakEntity, Window};
use iris_app::{PlaybackDomainChanges, PluginDomainChanges, WorkbenchDomainChanges};
use iris_core::library::Kind;
use iris_plugin_host::DirtySlots;

use super::kit::text_field::ActiveSlot;
use super::{IrisWindow, WindowShell};

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

/// Centralized pure mapping from headless [`WorkbenchDomainChanges`] onto regional GPUI [`DirtyRegions`].
pub(in crate::ui) const fn dirty_regions_for_workbench(
    changes: &WorkbenchDomainChanges,
    vis: UiVisibility,
) -> DirtyRegions {
    if changes.theme_changed {
        return DirtyRegions::ALL;
    }
    let mut out = DirtyRegions::NONE;
    if changes.root_layout_changed {
        out.root = true;
        out.sidebar = true;
        out.plugin_panel |= vis.sidebar_plugins_visible;
        out.settings |= vis.settings_open && changes.settings_changed;
        out.viewport = true;
        out.dock |= vis.dock_visible;
    }
    if changes.titlebar_changed {
        out.root = true;
    }
    if changes.sidebar_changed {
        out.sidebar = true;
        out.plugin_panel |= vis.sidebar_plugins_visible && !changes.keymap_changed;
    }
    if changes.dock_changed {
        out.dock |= vis.dock_visible;
        out.root |= vis.popover_open;
    }
    if changes.viewport_changed {
        out.viewport = true;
    }
    if changes.settings_changed {
        out.settings |= vis.settings_open;
    }
    if changes.keymap_changed {
        out.root = true;
        out.sidebar = true;
        out.settings |= vis.settings_open;
        out.viewport = true;
        out.dock |= vis.dock_visible;
    }
    out
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

/// Bridges any GPUI element/action callback onto `Entity<WindowShell>` regardless of the rendering view.
pub(in crate::ui) fn model_listener<E: 'static>(
    model: &Entity<WindowShell>,
    f: impl Fn(&mut WindowShell, &E, &mut Window, &mut Context<WindowShell>) + 'static,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let weak = model.downgrade();
    move |ev, window, cx| {
        let _ = weak.update(cx, |this, cx| f(this, ev, window, cx));
    }
}

/// Independent GPUI entity for plugin sidebar panels (`SidebarMode::Plugins`).
pub(in crate::ui) struct PluginPanel {
    pub(in crate::ui) model: Entity<WindowShell>,
}

impl Render for PluginPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::plugin_ui::render_plugin_sidebar_entity(&self.model, window, cx)
    }
}

/// Independent GPUI entity for the docked or fullscreen-floating workbench sidebar.
pub(in crate::ui) struct Sidebar {
    pub(in crate::ui) model: Entity<WindowShell>,
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
    pub(in crate::ui) model: Entity<WindowShell>,
}

impl Render for SettingsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::settings::render_settings_entity(&self.model, window, cx)
    }
}

/// Independent GPUI entity for the responsive transport & timeline control bar.
pub(in crate::ui) struct DockView {
    pub(in crate::ui) model: Entity<WindowShell>,
    pub(in crate::ui) floating: bool,
}

impl Render for DockView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::controls::render_dock_entity(&self.model, self.floating, window, cx)
    }
}

/// Independent GPUI entity for the media viewport, HUD overlays, and floating fullscreen chrome.
pub(in crate::ui) struct ViewportView {
    pub(in crate::ui) model: Entity<WindowShell>,
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
        dirty_regions_for_slot_edit, dirty_regions_for_workbench,
    };
    use crate::ui::kit::text_field::ActiveSlot;
    use iris_app::{
        PlaybackDomainChanges, PluginDomainChanges, ShortcutRecordOutcome, StartupWorkbench,
        WorkbenchDomainChanges,
    };
    use iris_core::library::Kind;
    use iris_core::preset::{PresetLayout, SidebarMode};
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

    #[test]
    fn fixed_workbench_sequence_preset_theme_shortcut_and_directory_prevents_over_invalidation() {
        let dir = std::env::temp_dir().join(format!("iris-views-wb-seq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let dir = iris_core::library::safe_resolve_path(&dir);
        let startup = StartupWorkbench::from_paths_with_doh_sink(
            dir.join("settings.conf"),
            dir.join("presets.json"),
            dir.join("keymap.json"),
            |_| {},
        );
        let presets = iris_core::preset::PresetManager::load_from(&dir.join("presets.json"));
        let (active_settings, preset_rt, _) =
            iris_core::preset::PresetRuntime::restore_on_startup_with_path(
                &presets,
                startup.settings().clone(),
                Some(dir.join("settings.conf")),
            );
        let mut wb = iris_app::WorkbenchController::with_doh_sink(
            active_settings,
            presets,
            preset_rt,
            startup.keymap().clone(),
            |_| {},
        );

        let vis_files_closed_settings = UiVisibility {
            dock_visible: true,
            sidebar_plugins_visible: false,
            settings_open: false,
            palette_open: false,
            popover_open: false,
        };
        let vis_settings_open = UiVisibility {
            dock_visible: true,
            sidebar_plugins_visible: true,
            settings_open: true,
            palette_open: false,
            popover_open: false,
        };

        // 1. Switch preset:
        //    - Same-theme preset ("study" keeps "graphite" & same disabled_plugins) with settings
        //      modal closed & Files sidebar active must NOT invalidate `settings` or `plugin_panel`.
        //    - Theme-changing preset ("cinema" switches to "oled" theme) invalidates `ALL`.
        let study_step = wb
            .switch_preset(
                "study",
                PresetLayout {
                    sidebar_mode: SidebarMode::Files,
                    window_size: Some((1280., 760.)),
                },
            )
            .expect("study preset");
        assert!(!study_step.changes.theme_changed);
        assert_eq!(
            dirty_regions_for_workbench(&study_step.changes, vis_files_closed_settings),
            DirtyRegions {
                root: true,
                sidebar: true,
                plugin_panel: false,
                settings: false,
                viewport: true,
                dock: true,
            }
        );

        let cinema_step = wb
            .switch_preset(
                "cinema",
                PresetLayout {
                    sidebar_mode: SidebarMode::Plugins,
                    window_size: Some((1280., 760.)),
                },
            )
            .expect("cinema preset");
        assert!(cinema_step.changes.theme_changed);
        assert_eq!(
            dirty_regions_for_workbench(&cinema_step.changes, vis_files_closed_settings),
            DirtyRegions::ALL
        );

        // 2. Adjust theme & UI font scale: global `ThemeTokens` update -> `ALL`.
        assert_eq!(
            dirty_regions_for_workbench(&wb.set_theme_id("paper"), vis_files_closed_settings),
            DirtyRegions::ALL
        );
        assert_eq!(
            dirty_regions_for_workbench(&wb.step_ui_font_scale(0.05), vis_files_closed_settings),
            DirtyRegions::ALL
        );

        // 3. Edit shortcut: updates shortcut-bearing chrome & open SettingsPanel,
        //    but NEVER invalidates `plugin_panel` (nor `dock` when fullscreen chrome is hidden).
        let ShortcutRecordOutcome::Bound {
            changes: sc_changes,
            ..
        } = wb.record_normalized_shortcut("toggle_info", "Workspace", "secondary-k", false)
        else {
            panic!("expected Bound shortcut outcome");
        };
        assert_eq!(
            dirty_regions_for_workbench(&sc_changes, vis_settings_open),
            DirtyRegions {
                root: true,
                sidebar: true,
                plugin_panel: false,
                settings: true,
                viewport: true,
                dock: true,
            }
        );
        assert_eq!(
            dirty_regions_for_workbench(&sc_changes, UiVisibility::default()),
            DirtyRegions {
                root: true,
                sidebar: true,
                plugin_panel: false,
                settings: false,
                viewport: true,
                dock: false,
            }
        );

        // 4. Toggle docked directory: with SettingsPanel closed & Files sidebar active,
        //    neither `plugin_panel` nor `settings` is invalidated; pure settings-only edits
        //    invalidate ONLY `settings` when open and `NONE` when closed.
        let (_, dir_changes) = wb.toggle_docked_directory();
        assert_eq!(
            dirty_regions_for_workbench(&dir_changes, vis_files_closed_settings),
            DirtyRegions {
                root: true,
                sidebar: true,
                plugin_panel: false,
                settings: false,
                viewport: true,
                dock: true,
            }
        );
        assert_eq!(
            dirty_regions_for_workbench(
                &WorkbenchDomainChanges::settings_only(),
                vis_settings_open
            ),
            DirtyRegions::settings_only()
        );
        assert_eq!(
            dirty_regions_for_workbench(
                &WorkbenchDomainChanges::settings_only(),
                vis_files_closed_settings
            ),
            DirtyRegions::NONE
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
