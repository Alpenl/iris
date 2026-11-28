//! Sparse workspace preset switching, custom preset capture/update/deletion,
//! and `ResetAppearance` baseline reset on [`super::WorkbenchController`].

use iris_core::player::AspectMode;
use iris_core::preset::{
    PresetCaptureScope, PresetLayout, PresetManager, PresetTransition, SidebarMode,
};
use iris_core::settings::Settings;

use super::{WorkbenchController, WorkbenchDomainChanges};

/// Resets theme and control-chrome fields on `settings` back to the safe built-in Graphite defaults.
pub fn apply_appearance_reset_defaults(settings: &mut Settings) {
    settings.theme_id = "graphite".into();
    settings.floating_dock = false;
    settings.compact_controls = false;
    settings.show_titlebar_meta = true;
    settings.hidden_dock_buttons.clear();
}

/// Pure workbench state delta produced when switching presets before plugin/player coordination.
#[derive(Clone, Debug)]
pub struct PresetSwitchStep {
    pub transition: PresetTransition,
    pub prev_disabled_plugins: Vec<String>,
    pub changes: WorkbenchDomainChanges,
}

/// Pure workbench state delta produced when saving current state as a new custom preset.
#[derive(Clone, Debug)]
pub struct PresetSaveStep {
    pub saved_id: String,
    pub saved_name: String,
}

/// Pure workbench state delta produced when deleting a custom preset.
#[derive(Clone, Debug)]
pub struct PresetDeleteStep {
    pub deleted_id: String,
    pub was_active: bool,
    pub changes: WorkbenchDomainChanges,
}

impl WorkbenchController {
    pub fn next_preset_id(&self) -> String {
        self.presets
            .next_id(&self.settings.active_preset)
            .to_owned()
    }

    pub fn switch_preset(
        &mut self,
        id: &str,
        current_layout: PresetLayout,
    ) -> Option<PresetSwitchStep> {
        if id != "default" && self.presets.get(id).is_none() {
            return None;
        }
        let prev_theme = self.settings.theme_id.clone();
        let prev_disabled_plugins = self.settings.disabled_plugins.clone();
        let prev_pinned = self.settings.always_on_top;
        let transition =
            self.preset_rt
                .switch_preset(&self.presets, id, &mut self.settings, current_layout);
        self.apply_doh_sync();
        let changes = WorkbenchDomainChanges {
            theme_changed: self.settings.theme_id != prev_theme
                || prev_disabled_plugins != self.settings.disabled_plugins,
            settings_changed: true,
            root_layout_changed: true,
            sidebar_changed: true,
            dock_changed: true,
            titlebar_changed: true,
            viewport_changed: true,
            keymap_changed: false,
            always_on_top_changed: (self.settings.always_on_top != prev_pinned
                || self.settings.always_on_top)
                .then_some(self.settings.always_on_top),
            window_resize: transition.window_size,
        };
        Some(PresetSwitchStep {
            transition,
            prev_disabled_plugins,
            changes,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn capture_and_save_custom_preset(
        &mut self,
        name: String,
        description: String,
        scope: PresetCaptureScope,
        default_name: &str,
        sidebar_mode: SidebarMode,
        window_size: Option<(f32, f32)>,
        speed: f64,
        aspect: AspectMode,
    ) -> PresetSaveStep {
        let clean_name = name.trim();
        let final_name = if clean_name.is_empty() {
            default_name.to_owned()
        } else {
            clean_name.to_owned()
        };
        let preset = PresetManager::capture_from_current(
            "",
            &final_name,
            description,
            scope,
            &self.settings,
            &self.preset_rt,
            sidebar_mode,
            window_size,
            speed,
            aspect,
        );
        let saved_id = self.presets.save_custom(preset);
        PresetSaveStep {
            saved_id,
            saved_name: final_name,
        }
    }

    pub fn update_custom_preset(
        &mut self,
        id: &str,
        scope: PresetCaptureScope,
        sidebar_mode: SidebarMode,
        window_size: Option<(f32, f32)>,
        speed: f64,
        aspect: AspectMode,
    ) -> Option<WorkbenchDomainChanges> {
        if !self.presets.update_from_current(
            id,
            scope,
            &self.settings,
            &self.preset_rt,
            sidebar_mode,
            window_size,
            speed,
            aspect,
        ) {
            return None;
        }
        if self.settings.active_preset == id
            && let Some(updated) = self.presets.get(id).cloned()
        {
            self.preset_rt
                .sync_updated_active_preset(&updated, &self.settings);
        }
        Some(WorkbenchDomainChanges {
            settings_changed: true,
            titlebar_changed: true,
            ..WorkbenchDomainChanges::empty()
        })
    }

    pub fn delete_custom_preset(&mut self, id: &str) -> Option<PresetDeleteStep> {
        if !self.presets.delete_custom(id) {
            return None;
        }
        let binding_id = format!("preset:{id}");
        let had_binding = self.keymap.is_overridden(&binding_id);
        self.keymap.reset_binding(&binding_id);
        let was_active = self.settings.active_preset == id;
        Some(PresetDeleteStep {
            deleted_id: id.to_owned(),
            was_active,
            changes: WorkbenchDomainChanges {
                settings_changed: true,
                titlebar_changed: true,
                keymap_changed: had_binding,
                ..WorkbenchDomainChanges::empty()
            },
        })
    }

    /// Commits the Graphite appearance reset into `Settings` and `PresetRuntime`.
    pub fn finish_appearance_reset(&mut self) -> WorkbenchDomainChanges {
        apply_appearance_reset_defaults(&mut self.settings);
        self.preset_rt.record_user_edit(&self.settings);
        WorkbenchDomainChanges {
            theme_changed: true,
            settings_changed: true,
            root_layout_changed: true,
            sidebar_changed: true,
            dock_changed: true,
            titlebar_changed: true,
            viewport_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }
}
