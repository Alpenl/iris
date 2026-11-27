//! Reversible personal-baseline runtime (`PresetRuntime`) for sparse presets.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::player::AspectMode;
use crate::settings::{
    self, MAX_DIRECTORY_WIDTH, MAX_SPEED, MIN_DIRECTORY_WIDTH, MIN_SPEED, Settings,
};

use super::{
    ALL_BITS, DEFAULT_WINDOW_SIZE, PluginPatch, PresetLayout, PresetManager, PresetTransition,
    SidebarMode, WorkspacePreset, copy_masked_fields, field_matches, mask, set_plugin_disabled,
};

/// In-memory revert state tracking fields owned by the active preset.
#[derive(Clone, Debug, Default, PartialEq)]
struct PresetRevert {
    initial_mask: u32,
    active_mask: u32,
    applied: Settings,
    previous_layout: PresetLayout,
    had_sidebar_mode: bool,
    had_aspect: bool,
    plugin_overrides: BTreeMap<String, bool>,
    on_exit_commands: Vec<(String, String)>,
}

/// Holds the user's clean daily `baseline` (`settings.conf`) and active revert state.
#[derive(Clone, Debug)]
pub struct PresetRuntime {
    baseline: Settings,
    pre_preset_baseline: Settings,
    plugin_capture_baseline: Vec<String>,
    revert: PresetRevert,
    has_active_preset: bool,
    initial_plugins: BTreeMap<String, bool>,
    settings_path: Option<PathBuf>,
    observed_plugins: Vec<String>,
    persisted_plugins: Vec<String>,
}

struct EvaluatedPreset {
    bits: u32,
    had_sidebar_mode: bool,
    sidebar_mode: Option<SidebarMode>,
    window_size: Option<(f32, f32)>,
    had_aspect: bool,
    aspect: Option<AspectMode>,
    speed: Option<f64>,
    plugin_overrides: BTreeMap<String, bool>,
    plugin_commands: Vec<(String, String)>,
    on_exit_commands: Vec<(String, String)>,
}

fn evaluate_preset(preset: &WorkspacePreset, active: &mut Settings) -> EvaluatedPreset {
    let (mut bits, mut had_aspect, mut aspect, mut speed) = (0u32, false, None, None);
    let (mut had_sidebar_mode, mut sidebar_mode, mut window_size) = (false, None, None);
    macro_rules! put {
        ($(($opt:expr, $field:ident, $m:ident)),+ $(,)?) => {
            $(if let Some(v) = $opt { active.$field = v; bits |= mask::$m; })+
        };
    }
    if let Some(l) = &preset.layout {
        let clean_theme = l
            .theme_id
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        put! {
            (l.directory_side, directory_side, DIR_SIDE),
            (l.directory_width.map(|w| w.clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH)), directory_width, DIR_WIDTH),
            (l.directory_open, directory_open, DIR_OPEN),
            (l.always_on_top, always_on_top, ON_TOP),
            (l.photo_filmstrip, photo_filmstrip, FILMSTRIP),
            (clean_theme, theme_id, THEME),
            (l.floating_dock, floating_dock, FLOAT_DOCK),
            (l.show_titlebar_meta, show_titlebar_meta, TITLE_META),
            (l.compact_controls, compact_controls, COMPACT),
            (l.hidden_dock_buttons.clone(), hidden_dock_buttons, HIDDEN_BTNS),
        }
        if let Some(mode) = l.sidebar_mode {
            sidebar_mode = Some(mode);
            had_sidebar_mode = true;
        }
        if let Some((w, h)) = l
            .window_size
            .filter(|(w, h)| w.is_finite() && h.is_finite())
        {
            let (cw, ch) = (w.clamp(480., 7680.), h.clamp(300., 4320.));
            (active.window_width, active.window_height, window_size) =
                (Some(cw), Some(ch), Some((cw, ch)));
            bits |= mask::WIN_SIZE;
        }
    }
    if let Some(pb) = &preset.playback {
        if let Some(sp) = pb.speed.map(|s| s.clamp(MIN_SPEED, MAX_SPEED)) {
            (active.speed, speed) = (Some(sp), Some(sp));
            bits |= mask::SPEED;
        }
        put! {
            (pb.on_eof, on_eof, ON_EOF),
            (pb.seek_step.map(|s| s.clamp(1.0, 60.0)), seek_step, SEEK_STEP),
            (pb.sub_scale.map(|s| s.clamp(0.5, 2.5)), sub_scale, SUB_SCALE),
            (pb.photo_default, photo_default, PHOTO_DEF),
            (pb.single_click_pause, single_click_pause, CLICK_PAUSE),
        }
        if let Some(asp) = pb.aspect {
            (had_aspect, aspect) = (true, Some(asp));
        }
    }

    let (mut plugin_overrides, mut plugin_commands, mut on_exit_commands) =
        (BTreeMap::new(), Vec::new(), Vec::new());
    if let Some(pl) = &preset.plugins {
        let en = pl.enable_plugins.iter().map(|id| (id, false));
        for (id, dis) in en.chain(pl.disable_plugins.iter().map(|id| (id, true))) {
            set_plugin_disabled(&mut active.disabled_plugins, id, dis);
            plugin_overrides.insert(id.clone(), dis);
        }
        plugin_commands.clone_from(&pl.plugin_commands);
        on_exit_commands.clone_from(&pl.on_exit_commands);
    }

    EvaluatedPreset {
        bits,
        had_sidebar_mode,
        sidebar_mode,
        window_size,
        had_aspect,
        aspect,
        speed,
        plugin_overrides,
        plugin_commands,
        on_exit_commands,
    }
}

impl PresetRuntime {
    pub fn restore_on_startup(
        mgr: &PresetManager,
        loaded: Settings,
    ) -> (Settings, Self, Vec<(String, String)>) {
        Self::restore_on_startup_with_path(mgr, loaded, settings::path())
    }

    pub fn restore_on_startup_with_path(
        mgr: &PresetManager,
        mut loaded: Settings,
        settings_path: Option<PathBuf>,
    ) -> (Settings, Self, Vec<(String, String)>) {
        let resolved = (loaded.active_preset != "default")
            .then(|| mgr.get(&loaded.active_preset))
            .flatten()
            .cloned();
        if resolved.is_none() {
            loaded.active_preset = "default".into();
        }
        let mut active = loaded.clone();
        let (default_w, default_h) = DEFAULT_WINDOW_SIZE;
        let initial_layout = PresetLayout {
            sidebar_mode: SidebarMode::default(),
            window_size: Some(if loaded.remember_window_size {
                (
                    loaded.window_width.unwrap_or(default_w).clamp(480., 7680.),
                    loaded.window_height.unwrap_or(default_h).clamp(300., 4320.),
                )
            } else {
                DEFAULT_WINDOW_SIZE
            }),
        };
        let mut rt = Self {
            observed_plugins: loaded.disabled_plugins.clone(),
            persisted_plugins: loaded.disabled_plugins.clone(),
            plugin_capture_baseline: loaded.disabled_plugins.clone(),
            pre_preset_baseline: loaded.clone(),
            baseline: loaded,
            revert: PresetRevert::default(),
            has_active_preset: false,
            initial_plugins: BTreeMap::new(),
            settings_path,
        };
        let cmds = resolved.map_or_else(Vec::new, |p| {
            rt.apply_in_memory(Some(&p), &mut active, initial_layout)
                .plugin_commands
        });
        rt.observed_plugins.clone_from(&active.disabled_plugins);
        (active, rt, cmds)
    }

    pub fn baseline(&self) -> &Settings {
        &self.baseline
    }

    fn persist_baseline(&mut self) {
        // Windows own their layout/preset state, but plugin installation enable
        // flags are global. Merge only this window's explicit per-ID changes.
        if let Some(path) = &self.settings_path
            && path.is_file()
        {
            let mut latest = Settings::load_from(path).disabled_plugins;
            let ids: BTreeSet<_> = self
                .persisted_plugins
                .iter()
                .chain(&self.baseline.disabled_plugins)
                .cloned()
                .collect();
            for id in ids {
                let old = self.persisted_plugins.contains(&id);
                let new = self.baseline.disabled_plugins.contains(&id);
                if old != new {
                    set_plugin_disabled(&mut latest, &id, new);
                }
            }
            self.baseline.disabled_plugins = latest;
        }
        if let Some(path) = &self.settings_path
            && let Err(e) = self.baseline.save_to(path)
        {
            log::error!("saving baseline settings to {} failed: {e}", path.display());
        }
        self.persisted_plugins
            .clone_from(&self.baseline.disabled_plugins);
    }

    fn apply_in_memory(
        &mut self,
        target: Option<&WorkspacePreset>,
        active: &mut Settings,
        current_layout: PresetLayout,
    ) -> PresetTransition {
        if let Some(path) = &self.settings_path
            && path.is_file()
        {
            self.baseline.disabled_plugins = Settings::load_from(path).disabled_plugins;
            self.persisted_plugins
                .clone_from(&self.baseline.disabled_plugins);
        }
        active
            .disabled_plugins
            .clone_from(&self.baseline.disabled_plugins);
        let prev = std::mem::take(&mut self.revert);
        copy_masked_fields(prev.active_mask, active, &self.baseline);
        for id in prev.plugin_overrides.keys() {
            set_plugin_disabled(
                &mut active.disabled_plugins,
                id,
                self.baseline.disabled_plugins.contains(id),
            );
        }
        self.pre_preset_baseline.clone_from(&self.baseline);
        self.plugin_capture_baseline
            .clone_from(&self.baseline.disabled_plugins);

        let mut trans = PresetTransition {
            preset_id: target.map_or_else(|| "default".into(), |p| p.id.clone()),
            sidebar_mode: prev
                .had_sidebar_mode
                .then_some(prev.previous_layout.sidebar_mode),
            window_size: (prev.active_mask & mask::WIN_SIZE != 0)
                .then_some(prev.previous_layout.window_size)
                .flatten(),
            aspect: prev.had_aspect.then_some(AspectMode::Original),
            speed: (prev.active_mask & mask::SPEED != 0).then(|| active.speed.unwrap_or(1.0)),
            on_exit_commands: prev.on_exit_commands,
            plugin_commands: Vec::new(),
        };
        let Some(preset) = target.filter(|p| p.id != "default") else {
            (active.active_preset, self.has_active_preset) = ("default".into(), false);
            self.initial_plugins.clear();
            self.observed_plugins.clone_from(&active.disabled_plugins);
            return trans;
        };

        let previous_layout = PresetLayout {
            sidebar_mode: trans.sidebar_mode.unwrap_or(current_layout.sidebar_mode),
            window_size: trans.window_size.or(current_layout.window_size),
        };
        let eval = evaluate_preset(preset, active);
        if let Some(mode) = eval.sidebar_mode {
            trans.sidebar_mode = Some(mode);
        }
        if let Some(ws) = eval.window_size {
            trans.window_size = Some(ws);
        }
        if let Some(sp) = eval.speed {
            trans.speed = Some(sp);
        }
        if let Some(asp) = eval.aspect {
            trans.aspect = Some(asp);
        }
        trans.plugin_commands = eval.plugin_commands;

        active.active_preset.clone_from(&preset.id);
        self.has_active_preset = true;
        self.initial_plugins.clone_from(&eval.plugin_overrides);
        self.revert = PresetRevert {
            initial_mask: eval.bits,
            active_mask: eval.bits,
            applied: active.clone(),
            previous_layout,
            had_sidebar_mode: eval.had_sidebar_mode,
            had_aspect: eval.had_aspect,
            plugin_overrides: eval.plugin_overrides,
            on_exit_commands: eval.on_exit_commands,
        };
        self.observed_plugins.clone_from(&active.disabled_plugins);
        trans
    }

    pub fn switch_preset(
        &mut self,
        mgr: &PresetManager,
        id: &str,
        active_settings: &mut Settings,
        current_layout: PresetLayout,
    ) -> PresetTransition {
        let target = (id != "default").then(|| mgr.get(id)).flatten().cloned();
        let trans = self.apply_in_memory(target.as_ref(), active_settings, current_layout);
        self.baseline.active_preset.clone_from(&trans.preset_id);
        self.pre_preset_baseline
            .active_preset
            .clone_from(&trans.preset_id);
        self.persist_baseline();
        trans
    }

    /// Re-synchronizes [`PresetRuntime`] after merging live edits into the currently active preset,
    /// clearing the `"· 已微调"` badge and restoring the pre-preset daily baseline for merged fields.
    pub fn sync_updated_active_preset(
        &mut self,
        preset: &WorkspacePreset,
        active_settings: &Settings,
    ) {
        if !self.has_active_preset || preset.id != active_settings.active_preset {
            return;
        }
        let mut applied = active_settings.clone();
        let eval = evaluate_preset(preset, &mut applied);

        copy_masked_fields(eval.bits, &mut self.baseline, &self.pre_preset_baseline);
        for id in eval.plugin_overrides.keys() {
            set_plugin_disabled(
                &mut self.baseline.disabled_plugins,
                id,
                self.pre_preset_baseline.disabled_plugins.contains(id),
            );
        }
        self.plugin_capture_baseline
            .clone_from(&self.baseline.disabled_plugins);
        self.initial_plugins.clone_from(&eval.plugin_overrides);
        self.revert = PresetRevert {
            initial_mask: eval.bits,
            active_mask: eval.bits,
            applied,
            previous_layout: self.revert.previous_layout,
            had_sidebar_mode: self.revert.had_sidebar_mode || eval.had_sidebar_mode,
            had_aspect: self.revert.had_aspect || eval.had_aspect,
            plugin_overrides: eval.plugin_overrides,
            on_exit_commands: eval.on_exit_commands,
        };
        self.persist_baseline();
    }

    pub fn set_plugin_enabled(&mut self, active_settings: &mut Settings, id: &str, enabled: bool) {
        if let Some(path) = &self.settings_path
            && path.is_file()
        {
            self.baseline.disabled_plugins = Settings::load_from(path).disabled_plugins;
            self.persisted_plugins
                .clone_from(&self.baseline.disabled_plugins);
        }
        // A click is explicit even if a stale local settings snapshot already
        // happens to contain that value. It also releases a preset's ownership.
        self.revert.plugin_overrides.remove(id);
        set_plugin_disabled(&mut active_settings.disabled_plugins, id, !enabled);
        self.record_user_edit_with_forced(active_settings, Some(id));
    }

    pub fn record_user_edit(&mut self, active_settings: &Settings) {
        self.record_user_edit_with_forced(active_settings, None);
    }

    fn record_user_edit_with_forced(
        &mut self,
        active_settings: &Settings,
        forced_plugin: Option<&str>,
    ) {
        let mut next = active_settings.clone();
        next.active_preset.clone_from(&self.baseline.active_preset);
        let mut keep_mask = 0u32;
        for &bit in ALL_BITS {
            if self.revert.active_mask & bit != 0 {
                if field_matches(bit, active_settings, &self.revert.applied) {
                    keep_mask |= bit;
                } else {
                    self.revert.active_mask &= !bit;
                }
            }
        }
        copy_masked_fields(keep_mask, &mut next, &self.baseline);

        let all_ids: BTreeSet<String> = self
            .baseline
            .disabled_plugins
            .iter()
            .chain(&active_settings.disabled_plugins)
            .chain(&self.observed_plugins)
            .chain(self.revert.plugin_overrides.keys())
            .cloned()
            .chain(forced_plugin.map(str::to_owned))
            .collect();
        next.disabled_plugins
            .clone_from(&self.baseline.disabled_plugins);
        for id in all_ids {
            let dis = active_settings.disabled_plugins.contains(&id);
            if forced_plugin != Some(id.as_str()) && dis == self.observed_plugins.contains(&id) {
                continue;
            }
            match self.revert.plugin_overrides.get(&id).copied() {
                None => set_plugin_disabled(&mut next.disabled_plugins, &id, dis),
                Some(forced) if forced != dis => {
                    set_plugin_disabled(&mut next.disabled_plugins, &id, dis);
                    self.revert.plugin_overrides.remove(&id);
                }
                Some(_) => {}
            }
        }
        self.observed_plugins
            .clone_from(&active_settings.disabled_plugins);
        self.baseline = next;
        if !self.has_active_preset {
            self.pre_preset_baseline.clone_from(&self.baseline);
        }
        self.persist_baseline();
    }

    pub(super) fn capture_plugin_patch_for(
        &self,
        active: &Settings,
        target_existing: Option<&WorkspacePreset>,
    ) -> PluginPatch {
        // Manual toggles are persisted immediately. Compare to the baseline captured on
        // preset entry so recording the settings edit does not erase the capture delta.
        let baseline = &self.plugin_capture_baseline;
        let disabled = &active.disabled_plugins;
        let mut patch = PluginPatch {
            enable_plugins: baseline
                .iter()
                .filter(|id| !disabled.contains(id))
                .cloned()
                .collect(),
            disable_plugins: disabled
                .iter()
                .filter(|id| !baseline.contains(id))
                .cloned()
                .collect(),
            ..Default::default()
        };

        let is_active_target =
            target_existing.is_none_or(|existing| existing.id == active.active_preset);
        if is_active_target {
            for (id, &forced_disabled) in &self.initial_plugins {
                if forced_disabled == disabled.contains(id)
                    && forced_disabled == baseline.contains(id)
                {
                    if forced_disabled {
                        patch.disable_plugins.push(id.clone());
                    } else {
                        patch.enable_plugins.push(id.clone());
                    }
                }
            }
        } else if let Some(prev_pl) = target_existing.and_then(|p| p.plugins.as_ref()) {
            let en = prev_pl.enable_plugins.iter().map(|id| (id, false));
            for (id, forced_disabled) in
                en.chain(prev_pl.disable_plugins.iter().map(|id| (id, true)))
            {
                if forced_disabled == disabled.contains(id)
                    && forced_disabled == baseline.contains(id)
                {
                    if forced_disabled {
                        patch.disable_plugins.push(id.clone());
                    } else {
                        patch.enable_plugins.push(id.clone());
                    }
                }
            }
        }
        patch.enable_plugins.sort();
        patch.enable_plugins.dedup();
        patch.disable_plugins.sort();
        patch.disable_plugins.dedup();
        patch
    }

    pub fn is_modified(&self, active_settings: &Settings) -> bool {
        self.has_active_preset
            && (self.revert.active_mask != self.revert.initial_mask
                || ALL_BITS.iter().any(|&bit| {
                    self.revert.initial_mask & bit != 0
                        && !field_matches(bit, active_settings, &self.revert.applied)
                })
                || self
                    .initial_plugins
                    .iter()
                    .any(|(id, &dis)| active_settings.disabled_plugins.contains(id) != dis))
    }
}
