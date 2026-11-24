//! Sparse `1B+` workspace presets (`LayoutPatch`, `PlaybackPatch`, `PluginPatch`)
//! and reversible personal-baseline runtime (`PresetRuntime`, `PresetRevert`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs_util;
use crate::player::{AspectMode, MAX_SPEED, MIN_SPEED};
use crate::settings::{
    self, DEFAULT_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH, MIN_DIRECTORY_WIDTH, OnEof, PhotoDefault,
    Settings, Side,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetCaptureScope {
    pub layout: bool,
    pub playback: bool,
    pub plugins: bool,
}

impl Default for PresetCaptureScope {
    fn default() -> Self {
        Self {
            layout: true,
            playback: true,
            plugins: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarMode {
    #[default]
    Files,
    Sources,
    Queue,
    Plugins,
}

impl SidebarMode {
    pub fn label(self) -> &'static str {
        match self {
            SidebarMode::Files => "目录",
            SidebarMode::Sources => "数据源",
            SidebarMode::Queue => "队列",
            SidebarMode::Plugins => "插件",
        }
    }
}

/// Sparse layout, window geometry, theme, and control-bar chrome patch.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutPatch {
    pub directory_side: Option<Side>,
    pub directory_width: Option<f32>,
    pub directory_open: Option<bool>,
    pub sidebar_mode: Option<SidebarMode>,
    pub window_size: Option<(f32, f32)>,
    pub always_on_top: Option<bool>,
    pub photo_filmstrip: Option<bool>,
    pub theme_id: Option<String>,
    pub floating_dock: Option<bool>,
    pub show_titlebar_meta: Option<bool>,
    pub compact_controls: Option<bool>,
    pub hidden_dock_buttons: Option<Vec<String>>,
}

/// Sparse playback policy patch.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackPatch {
    pub speed: Option<f64>,
    pub on_eof: Option<OnEof>,
    pub seek_step: Option<f64>,
    pub sub_scale: Option<f64>,
    pub aspect: Option<AspectMode>,
    pub photo_default: Option<PhotoDefault>,
    pub single_click_pause: Option<bool>,
}

/// Explicit plugin enable/disable delta and entry/exit command hooks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginPatch {
    #[serde(default)]
    pub enable_plugins: Vec<String>,
    #[serde(default)]
    pub disable_plugins: Vec<String>,
    #[serde(default)]
    pub plugin_commands: Vec<(String, String)>,
    #[serde(default)]
    pub on_exit_commands: Vec<(String, String)>,
}

/// Named sparse workspace preset (`1B+` model).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspacePreset {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub layout: Option<LayoutPatch>,
    #[serde(default)]
    pub playback: Option<PlaybackPatch>,
    #[serde(default)]
    pub plugins: Option<PluginPatch>,
}

macro_rules! def_preset_bits {
    ($(($idx:expr, $name:ident, $eq:expr, $cp:expr)),+ $(,)?) => {
        mod mask { $(pub const $name: u32 = 1 << $idx;)+ }
        const ALL_BITS: &[u32] = &[$(mask::$name),+];
        fn field_matches(bit: u32, a: &Settings, b: &Settings) -> bool {
            match bit { $(mask::$name => ($eq as fn(&Settings, &Settings) -> bool)(a, b),)+ _ => true }
        }
        fn copy_masked_fields(bits: u32, dst: &mut Settings, src: &Settings) {
            $(if bits & mask::$name != 0 { ($cp as fn(&mut Settings, &Settings))(dst, src); })+
        }
    };
}

fn window_dimension_matches(a: Option<f32>, b: Option<f32>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() <= 1.,
        _ => a == b,
    }
}

def_preset_bits! {
    (0, DIR_SIDE, |a, b| a.directory_side == b.directory_side, |d, s| d.directory_side = s.directory_side),
    (1, DIR_WIDTH, |a, b| (a.directory_width - b.directory_width).abs() <= 0.5, |d, s| d.directory_width = s.directory_width),
    (2, DIR_OPEN, |a, b| a.directory_open == b.directory_open, |d, s| d.directory_open = s.directory_open),
    (3, WIN_SIZE, |a, b| window_dimension_matches(a.window_width, b.window_width) && window_dimension_matches(a.window_height, b.window_height), |d, s| { d.window_width = s.window_width; d.window_height = s.window_height; }),
    (4, ON_TOP, |a, b| a.always_on_top == b.always_on_top, |d, s| d.always_on_top = s.always_on_top),
    (5, FILMSTRIP, |a, b| a.photo_filmstrip == b.photo_filmstrip, |d, s| d.photo_filmstrip = s.photo_filmstrip),
    (6, THEME, |a, b| a.theme_id == b.theme_id, |d, s| d.theme_id.clone_from(&s.theme_id)),
    (7, FLOAT_DOCK, |a, b| a.floating_dock == b.floating_dock, |d, s| d.floating_dock = s.floating_dock),
    (8, TITLE_META, |a, b| a.show_titlebar_meta == b.show_titlebar_meta, |d, s| d.show_titlebar_meta = s.show_titlebar_meta),
    (9, COMPACT, |a, b| a.compact_controls == b.compact_controls, |d, s| d.compact_controls = s.compact_controls),
    (10, HIDDEN_BTNS, |a, b| a.hidden_dock_buttons == b.hidden_dock_buttons, |d, s| d.hidden_dock_buttons.clone_from(&s.hidden_dock_buttons)),
    (11, SPEED, |a, b| a.speed.zip(b.speed).map_or(a.speed.is_none() && b.speed.is_none(), |(x, y)| (x - y).abs() <= 1e-4), |d, s| d.speed = s.speed),
    (12, ON_EOF, |a, b| a.on_eof == b.on_eof, |d, s| d.on_eof = s.on_eof),
    (13, SEEK_STEP, |a, b| (a.seek_step - b.seek_step).abs() <= 1e-4, |d, s| d.seek_step = s.seek_step),
    (14, SUB_SCALE, |a, b| (a.sub_scale - b.sub_scale).abs() <= 1e-4, |d, s| d.sub_scale = s.sub_scale),
    (15, PHOTO_DEF, |a, b| a.photo_default == b.photo_default, |d, s| d.photo_default = s.photo_default),
    (16, CLICK_PAUSE, |a, b| a.single_click_pause == b.single_click_pause, |d, s| d.single_click_pause = s.single_click_pause),
}

pub const DEFAULT_WINDOW_SIZE: (f32, f32) = (1440., 900.);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PresetLayout {
    pub sidebar_mode: SidebarMode,
    pub window_size: Option<(f32, f32)>,
}

/// In-memory revert state tracking fields owned by the active preset.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PresetRevert {
    initial_mask: u32,
    active_mask: u32,
    applied: Settings,
    previous_layout: PresetLayout,
    had_sidebar_mode: bool,
    had_aspect: bool,
    plugin_overrides: BTreeMap<String, bool>,
    on_exit_commands: Vec<(String, String)>,
}

/// Imperative side-effects produced when switching workspace presets.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PresetTransition {
    pub preset_id: String,
    pub sidebar_mode: Option<SidebarMode>,
    pub window_size: Option<(f32, f32)>,
    pub aspect: Option<AspectMode>,
    pub speed: Option<f64>,
    pub on_exit_commands: Vec<(String, String)>,
    pub plugin_commands: Vec<(String, String)>,
}

/// Holds the user's clean daily `baseline` (`settings.conf`) and active [`PresetRevert`].
#[derive(Clone, Debug)]
pub struct PresetRuntime {
    baseline: Settings,
    plugin_capture_baseline: Vec<String>,
    revert: PresetRevert,
    has_active_preset: bool,
    initial_plugins: BTreeMap<String, bool>,
    settings_path: Option<PathBuf>,
}

pub fn presets_path() -> Option<PathBuf> {
    std::env::var_os("IRIS_PRESETS")
        .map(PathBuf::from)
        .or_else(|| fs_util::data_dir().map(|d| d.join("presets.json")))
}

fn is_valid_slug(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn set_plugin_disabled(list: &mut Vec<String>, id: &str, disabled: bool) {
    if disabled && !list.iter().any(|x| x == id) {
        list.push(id.to_string());
    } else if !disabled {
        list.retain(|x| x != id);
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
            plugin_capture_baseline: loaded.disabled_plugins.clone(),
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
        (active, rt, cmds)
    }

    pub fn baseline(&self) -> &Settings {
        &self.baseline
    }
    pub fn revert_state(&self) -> &PresetRevert {
        &self.revert
    }
    pub fn set_settings_path(&mut self, path: Option<PathBuf>) {
        self.settings_path = path;
    }

    fn persist_baseline(&self) {
        if let Some(path) = &self.settings_path
            && let Err(e) = self.baseline.save_to(path)
        {
            log::error!("saving baseline settings to {} failed: {e}", path.display());
        }
    }

    fn apply_in_memory(
        &mut self,
        target: Option<&WorkspacePreset>,
        active: &mut Settings,
        current_layout: PresetLayout,
    ) -> PresetTransition {
        let prev = std::mem::take(&mut self.revert);
        copy_masked_fields(prev.active_mask, active, &self.baseline);
        for id in prev.plugin_overrides.keys() {
            set_plugin_disabled(
                &mut active.disabled_plugins,
                id,
                self.baseline.disabled_plugins.contains(id),
            );
        }
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
            return trans;
        };

        let (mut bits, mut had_aspect) = (0u32, false);
        let previous_layout = PresetLayout {
            sidebar_mode: trans.sidebar_mode.unwrap_or(current_layout.sidebar_mode),
            window_size: trans.window_size.or(current_layout.window_size),
        };
        let mut had_sidebar_mode = false;
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
                trans.sidebar_mode = Some(mode);
                had_sidebar_mode = true;
            }
            if let Some((w, h)) = l
                .window_size
                .filter(|(w, h)| w.is_finite() && h.is_finite())
            {
                let (cw, ch) = (w.clamp(480., 7680.), h.clamp(300., 4320.));
                (active.window_width, active.window_height, trans.window_size) =
                    (Some(cw), Some(ch), Some((cw, ch)));
                bits |= mask::WIN_SIZE;
            }
        }
        if let Some(pb) = &preset.playback {
            if let Some(sp) = pb.speed.map(|s| s.clamp(MIN_SPEED, MAX_SPEED)) {
                (active.speed, trans.speed) = (Some(sp), Some(sp));
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
                (had_aspect, trans.aspect) = (true, Some(asp));
            }
        }

        let (mut plugin_overrides, mut on_exit_commands) = (BTreeMap::new(), Vec::new());
        if let Some(pl) = &preset.plugins {
            let en = pl.enable_plugins.iter().map(|id| (id, false));
            for (id, dis) in en.chain(pl.disable_plugins.iter().map(|id| (id, true))) {
                set_plugin_disabled(&mut active.disabled_plugins, id, dis);
                plugin_overrides.insert(id.clone(), dis);
            }
            trans.plugin_commands.clone_from(&pl.plugin_commands);
            on_exit_commands.clone_from(&pl.on_exit_commands);
        }

        active.active_preset.clone_from(&preset.id);
        self.has_active_preset = true;
        self.initial_plugins.clone_from(&plugin_overrides);
        self.revert = PresetRevert {
            initial_mask: bits,
            active_mask: bits,
            applied: active.clone(),
            previous_layout,
            had_sidebar_mode,
            had_aspect,
            plugin_overrides,
            on_exit_commands,
        };
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
        self.persist_baseline();
        trans
    }

    pub fn set_plugin_enabled(&mut self, active_settings: &mut Settings, id: &str, enabled: bool) {
        set_plugin_disabled(&mut active_settings.disabled_plugins, id, !enabled);
        self.record_user_edit(active_settings);
    }

    pub fn record_user_edit(&mut self, active_settings: &Settings) {
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
            .chain(self.revert.plugin_overrides.keys())
            .cloned()
            .collect();
        next.disabled_plugins
            .clone_from(&self.baseline.disabled_plugins);
        for id in all_ids {
            let dis = active_settings.disabled_plugins.contains(&id);
            match self.revert.plugin_overrides.get(&id).copied() {
                None => set_plugin_disabled(&mut next.disabled_plugins, &id, dis),
                Some(forced) if forced != dis => {
                    set_plugin_disabled(&mut next.disabled_plugins, &id, dis);
                    self.revert.plugin_overrides.remove(&id);
                }
                Some(_) => {}
            }
        }
        self.baseline = next;
        self.persist_baseline();
    }

    fn capture_plugin_patch(&self, active: &Settings) -> PluginPatch {
        // Manual toggles are persisted immediately. Compare to the session's starting
        // baseline so recording the settings edit does not erase the capture delta.
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
        // Keep explicit overrides the active preset still applies, including after
        // restart when a previously captured toggle may already match the baseline.
        for (id, &forced_disabled) in &self.initial_plugins {
            if forced_disabled == disabled.contains(id) && forced_disabled == baseline.contains(id)
            {
                if forced_disabled {
                    patch.disable_plugins.push(id.clone());
                } else {
                    patch.enable_plugins.push(id.clone());
                }
            }
        }
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

fn clamp_opt(v: Option<f64>, min: f64, max: f64) -> Option<f64> {
    v.filter(|x| x.is_finite()).map(|x| x.clamp(min, max))
}

fn sanitize_preset(preset: &mut WorkspacePreset) {
    preset.builtin = false;
    let name = preset.name.trim();
    preset.name = if name.is_empty() {
        "自定义工作区".into()
    } else {
        name.to_string()
    };
    preset.description = preset.description.trim().to_string();
    if let Some(l) = &mut preset.layout {
        l.directory_width = l.directory_width.map(|w| {
            if w.is_finite() {
                w.clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH)
            } else {
                DEFAULT_DIRECTORY_WIDTH
            }
        });
        l.window_size = l.window_size.and_then(|(w, h)| {
            (w.is_finite() && h.is_finite()).then_some((w.clamp(480., 7680.), h.clamp(300., 4320.)))
        });
        l.theme_id = l.theme_id.as_ref().and_then(|t| {
            let clean: String = t.chars().filter(|c| !c.is_control()).collect();
            (!clean.trim().is_empty()).then(|| clean.trim().to_string())
        });
    }
    if let Some(pb) = &mut preset.playback {
        pb.speed = clamp_opt(pb.speed, MIN_SPEED, MAX_SPEED);
        pb.seek_step = clamp_opt(pb.seek_step, 1.0, 60.0);
        pb.sub_scale = clamp_opt(pb.sub_scale, 0.5, 2.5);
    }
}

#[derive(Clone, Debug)]
pub struct PresetManager {
    presets: Vec<WorkspacePreset>,
    backing_path: Option<PathBuf>,
}

impl Default for PresetManager {
    fn default() -> Self {
        Self {
            presets: Self::builtin_presets(),
            backing_path: None,
        }
    }
}

macro_rules! builtin_list {
    ($(($id:expr, $name:expr, $desc:expr, $l:expr, $pb:expr, $pl:expr)),+ $(,)?) => {
        vec![$(WorkspacePreset {
            id: $id.into(), name: $name.into(), description: $desc.into(),
            builtin: true, layout: $l, playback: $pb, plugins: $pl,
        }),+]
    };
}

impl PresetManager {
    pub fn builtin_presets() -> Vec<WorkspacePreset> {
        let pip_hidden = ["screenshot", "loop", "aspect", "info", "favorite"]
            .into_iter()
            .map(String::from)
            .collect();
        builtin_list! {
            ("default", "默认工作区", "回到个人日常基线布局与播放偏好", None, None, None),
            ("cinema", "沉浸观影", "收起侧栏、悬浮控制栏、OLED 暗房主题并自动连播下一集",
                Some(LayoutPatch { theme_id: Some("oled".into()), directory_open: Some(false), floating_dock: Some(true), ..Default::default() }),
                Some(PlaybackPatch { on_eof: Some(OnEof::Next), ..Default::default() }), None),
            ("review", "审片与选片", "右侧宽目录、开启照片缩略图条、胶片暖灰主题并开启三分构图线",
                Some(LayoutPatch { theme_id: Some("studio".into()), directory_side: Some(Side::Right), directory_width: Some(360.0), directory_open: Some(true), photo_filmstrip: Some(true), ..Default::default() }),
                Some(PlaybackPatch { on_eof: Some(OnEof::Pause), ..Default::default() }),
                Some(PluginPatch { plugin_commands: vec![("iris.guides".into(), "thirds".into())], on_exit_commands: vec![("iris.guides".into(), "off".into())], ..Default::default() })),
            ("study", "精听与学习", "左侧插件面板就绪、3 秒精细回退、1.25× 大字幕",
                Some(LayoutPatch { directory_side: Some(Side::Left), directory_open: Some(true), sidebar_mode: Some(SidebarMode::Plugins), ..Default::default() }),
                Some(PlaybackPatch { seek_step: Some(3.0), sub_scale: Some(1.25), ..Default::default() }), None),
            ("pip", "小窗置顶", "560×320 迷你窗口、自动置顶、隐藏冗余按钮与目录栏",
                Some(LayoutPatch { window_size: Some((560.0, 320.0)), always_on_top: Some(true), directory_open: Some(false), show_titlebar_meta: Some(false), compact_controls: Some(true), hidden_dock_buttons: Some(pip_hidden), ..Default::default() }),
                None, None),
        }
    }

    pub fn load() -> Self {
        presets_path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Self {
        let mut presets = Self::builtin_presets();
        if let Ok(raw) = std::fs::read_to_string(path)
            && let Ok(custom_list) = serde_json::from_str::<Vec<WorkspacePreset>>(&raw)
        {
            for mut item in custom_list {
                let id = item.id.trim().to_string();
                if is_valid_slug(&id) && !presets.iter().any(|existing| existing.id == id) {
                    item.id = id;
                    sanitize_preset(&mut item);
                    presets.push(item);
                }
            }
        }
        Self {
            presets,
            backing_path: Some(path.to_path_buf()),
        }
    }

    pub fn save(&self) {
        if let Some(p) = self.backing_path.clone().or_else(presets_path)
            && let Err(e) = self.save_to(&p)
        {
            log::error!("saving presets to {} failed: {e}", p.display());
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let custom: Vec<&WorkspacePreset> = self.presets.iter().filter(|p| !p.builtin).collect();
        let json = serde_json::to_string_pretty(&custom).map_err(std::io::Error::other)?;
        fs_util::atomic_write(path, json.as_bytes())
    }

    pub fn list(&self) -> &[WorkspacePreset] {
        &self.presets
    }
    pub fn get(&self, id: &str) -> Option<&WorkspacePreset> {
        self.presets.iter().find(|p| p.id == id)
    }

    pub fn next_id(&self, current_id: &str) -> &str {
        let ix = self
            .presets
            .iter()
            .position(|p| p.id == current_id)
            .map_or(0, |i| (i + 1) % self.presets.len());
        self.presets[ix].id.as_str()
    }

    pub fn restore_on_startup(
        &self,
        loaded_baseline: Settings,
    ) -> (Settings, PresetRuntime, Vec<(String, String)>) {
        PresetRuntime::restore_on_startup(self, loaded_baseline)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn capture_from_current(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        scope: PresetCaptureScope,
        s: &Settings,
        runtime: &PresetRuntime,
        mode: SidebarMode,
        win: Option<(f32, f32)>,
        sp: f64,
        asp: AspectMode,
    ) -> WorkspacePreset {
        macro_rules! some_struct {
            ($T:ident { $($f:ident : $v:expr),+ $(,)? }) => { $T { $($f: Some($v)),+ } };
        }
        let w = s
            .directory_width
            .clamp(MIN_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH);
        let ws = win.or_else(|| s.window_width.zip(s.window_height));
        let layout = scope.layout.then(|| {
            let mut l = some_struct! { LayoutPatch {
                directory_side: s.directory_side, directory_width: w, directory_open: s.directory_open,
                sidebar_mode: mode, always_on_top: s.always_on_top, photo_filmstrip: s.photo_filmstrip,
                theme_id: s.theme_id.clone(), floating_dock: s.floating_dock,
                show_titlebar_meta: s.show_titlebar_meta, compact_controls: s.compact_controls,
                hidden_dock_buttons: s.hidden_dock_buttons.clone(), window_size: (0., 0.),
            }};
            l.window_size = ws;
            l
        });
        let eff_sp = Some(sp)
            .filter(|v| v.is_finite() && *v > 0.0)
            .or(s.speed)
            .unwrap_or(1.0);
        let playback = scope.playback.then(|| some_struct! { PlaybackPatch {
            speed: eff_sp.clamp(MIN_SPEED, MAX_SPEED), on_eof: s.on_eof,
            seek_step: s.seek_step.clamp(1.0, 60.0), sub_scale: s.sub_scale.clamp(0.5, 2.5),
            aspect: asp, photo_default: s.photo_default, single_click_pause: s.single_click_pause,
        }});
        let plugins = scope.plugins.then(|| runtime.capture_plugin_patch(s));
        let mut p = WorkspacePreset {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            builtin: false,
            layout,
            playback,
            plugins,
        };
        sanitize_preset(&mut p);
        p
    }

    pub fn save_custom(&mut self, mut preset: WorkspacePreset) -> String {
        sanitize_preset(&mut preset);
        let t = preset.id.trim().to_string();
        let ok = is_valid_slug(&t) && !self.presets.iter().any(|p| p.builtin && p.id == t);
        preset.id = if ok {
            t
        } else {
            (1..)
                .map(|n| format!("custom-{n}"))
                .find(|c| !self.presets.iter().any(|p| &p.id == c))
                .unwrap()
        };
        let id = preset.id.clone();
        match self.presets.iter_mut().find(|p| !p.builtin && p.id == id) {
            Some(ex) => *ex = preset,
            None => self.presets.push(preset),
        }
        self.save();
        id
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_from_current(
        &mut self,
        id: &str,
        scope: PresetCaptureScope,
        s: &Settings,
        runtime: &PresetRuntime,
        mode: SidebarMode,
        win: Option<(f32, f32)>,
        sp: f64,
        asp: AspectMode,
    ) -> bool {
        let Some(ix) = self.presets.iter().position(|p| !p.builtin && p.id == id) else {
            return false;
        };
        let prev = &self.presets[ix];
        let (n, d) = (prev.name.clone(), prev.description.clone());
        let hooks = prev
            .plugins
            .as_ref()
            .map(|p| (p.plugin_commands.clone(), p.on_exit_commands.clone()));
        let mut up = Self::capture_from_current(id, n, d, scope, s, runtime, mode, win, sp, asp);
        if let (Some(pl), Some((cmds, exit))) = (&mut up.plugins, hooks) {
            (pl.plugin_commands, pl.on_exit_commands) = (cmds, exit);
        }
        self.presets[ix] = up;
        self.save();
        true
    }

    pub fn delete_custom(&mut self, id: &str) -> bool {
        let Some(ix) = self.presets.iter().position(|p| !p.builtin && p.id == id) else {
            return false;
        };
        self.presets.remove(ix);
        self.save();
        true
    }
}
