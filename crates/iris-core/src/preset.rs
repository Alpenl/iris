//! Sparse `1B+` workspace presets (`LayoutPatch`, `PlaybackPatch`, `PluginPatch`)
//! and reversible personal-baseline runtime (`PresetRuntime`, `PresetRevert`).

mod runtime;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs_util;
use crate::player::{AspectMode, MAX_SPEED, MIN_SPEED};
use crate::settings::{
    DEFAULT_DIRECTORY_WIDTH, MAX_DIRECTORY_WIDTH, MIN_DIRECTORY_WIDTH, OnEof, PhotoDefault,
    Settings, Side,
};

pub use runtime::{PresetRevert, PresetRuntime};

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
        let plugins = scope
            .plugins
            .then(|| runtime.capture_plugin_patch_for(s, None));
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
        let target_plugins = scope
            .plugins
            .then(|| runtime.capture_plugin_patch_for(s, Some(prev)));
        let hooks = prev
            .plugins
            .as_ref()
            .map(|p| (p.plugin_commands.clone(), p.on_exit_commands.clone()));
        let mut scope_no_plugins = scope;
        scope_no_plugins.plugins = false;
        let mut up =
            Self::capture_from_current(id, n, d, scope_no_plugins, s, runtime, mode, win, sp, asp);
        up.plugins = target_plugins;
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
