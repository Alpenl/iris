//! Customizable keyboard shortcut schemes, `!TextInput` typing safety firewall,
//! conflict detection, and per-action/preset/plugin key bindings (`keymap.json`).

mod catalog;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs_util;

pub use catalog::{ACTIONS, ActionEntry, KeymapScheme, actions, find_action};

/// Returns the user keymap JSON path (`IRIS_KEYMAP` override or `<data_dir>/keymap.json`).
pub fn keymap_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("IRIS_KEYMAP") {
        return Some(PathBuf::from(p));
    }
    fs_util::data_dir().map(|d| d.join("keymap.json"))
}

fn split_keystroke(spec: &str) -> Option<(Vec<&str>, &str)> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "-" {
        return Some((Vec::new(), "-"));
    }
    if let Some(prefix) = trimmed.strip_suffix("--") {
        let mods: Vec<&str> = prefix.split('-').filter(|s| !s.is_empty()).collect();
        return Some((mods, "-"));
    }
    let mut parts: Vec<&str> = trimmed.split('-').collect();
    let key = parts.pop()?;
    (!key.is_empty()).then_some((parts, key))
}

fn has_primary_modifier_or_f1_f12(spec: &str) -> bool {
    let Some((mods, raw_key)) = split_keystroke(spec) else {
        return false;
    };
    mods.iter().any(|m| {
        matches!(
            m.to_ascii_lowercase().as_str(),
            "secondary"
                | "cmd"
                | "command"
                | "platform"
                | "super"
                | "meta"
                | "win"
                | "ctrl"
                | "control"
                | "alt"
                | "option"
        )
    }) || raw_key
        .to_ascii_lowercase()
        .strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=12).contains(&n))
}

/// Resolves a declared GPUI `KeyContext` predicate for `keystroke`, enforcing the
/// `!TextInput` firewall: any `"Workspace"` binding lacking `secondary-`/`cmd-`/`ctrl-`/`alt-`
/// (and not `f1..f12`) is automatically guarded as `"Workspace && !TextInput"`.
pub fn effective_gpui_context(declared_ctx: &str, keystroke: &str) -> Option<&'static str> {
    split_keystroke(keystroke)?;
    match declared_ctx.trim() {
        "Workspace" => {
            if has_primary_modifier_or_f1_f12(keystroke) {
                Some("Workspace")
            } else {
                Some("Workspace && !TextInput")
            }
        }
        "Workspace && !TextInput" => Some("Workspace && !TextInput"),
        "Viewport" => Some("Viewport"),
        "Viewport && media == video" => Some("Viewport && media == video"),
        "Viewport && media == image" => Some("Viewport && media == image"),
        "Viewport && media == audio" => Some("Viewport && media == audio"),
        _ => None,
    }
}

/// Converts raw GPUI key-down inputs into a normalized GPUI keystroke spec
/// (e.g. `"alt-1"`, `"shift-g"`, `"cmd-p"`). Returns `None` for pure modifier keys.
pub fn normalize_gpui_keystroke(
    key: &str,
    modifiers_cmd: bool,
    modifiers_ctrl: bool,
    modifiers_alt: bool,
    modifiers_shift: bool,
) -> Option<String> {
    let base = if key == " " {
        "space".to_string()
    } else {
        key.trim().to_ascii_lowercase()
    };
    if base.is_empty() || base.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let normalized_key = match base.as_str() {
        "shift" | "control" | "ctrl" | "alt" | "option" | "cmd" | "command" | "platform"
        | "super" | "meta" | "win" | "capslock" | "fn" => return None,
        "esc" => "escape",
        "return" => "enter",
        "pgup" => "pageup",
        "pgdn" => "pagedown",
        "del" => "delete",
        "arrowleft" => "left",
        "arrowright" => "right",
        "arrowup" => "up",
        "arrowdown" => "down",
        other => other,
    };

    let mut parts = Vec::with_capacity(5);
    for (on, name) in [
        (modifiers_cmd, "cmd"),
        (modifiers_ctrl, "ctrl"),
        (modifiers_alt, "alt"),
        (modifiers_shift, "shift"),
    ] {
        if on {
            parts.push(name);
        }
    }
    parts.push(normalized_key);
    Some(parts.join("-"))
}

fn parse_mods(mods: &[&str], is_macos: bool) -> Option<(bool, bool, bool, bool)> {
    let (mut cmd, mut ctrl, mut alt, mut shift) = (false, false, false, false);
    for m in mods {
        match m.to_ascii_lowercase().as_str() {
            "secondary" if is_macos => cmd = true,
            "secondary" => ctrl = true,
            "cmd" | "command" | "platform" | "super" | "meta" | "win" => cmd = true,
            "ctrl" | "control" => ctrl = true,
            "alt" | "option" => alt = true,
            "shift" => shift = true,
            _ => return None,
        }
    }
    Some((cmd, ctrl, alt, shift))
}

/// Formats a GPUI keystroke string for UI badges and menus.
pub fn format_keystroke_display(spec: &str, is_macos: bool) -> String {
    let Some((mods, raw_key)) = split_keystroke(spec) else {
        return String::new();
    };
    let (has_cmd, has_ctrl, has_alt, has_shift) =
        parse_mods(&mods, is_macos).unwrap_or((false, false, false, false));
    let key_lower = raw_key.to_ascii_lowercase();
    let is_fkey = key_lower
        .strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=24).contains(&n));
    let key_label: String = match key_lower.as_str() {
        "space" => "Space".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "enter" | "return" => "Enter".into(),
        "escape" | "esc" => "Esc".into(),
        "backspace" => if is_macos { "⌫" } else { "Backspace" }.into(),
        "delete" => if is_macos { "⌦" } else { "Del" }.into(),
        "tab" => "Tab".into(),
        "pageup" => "PgUp".into(),
        "pagedown" => "PgDn".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        other if other.len() == 1 || is_fkey => other.to_ascii_uppercase(),
        other => other.to_string(),
    };

    if is_macos {
        let mut out = String::new();
        for (on, sym) in [
            (has_ctrl, '⌃'),
            (has_alt, '⌥'),
            (has_shift, '⇧'),
            (has_cmd, '⌘'),
        ] {
            if on {
                out.push(sym);
            }
        }
        out.push_str(&key_label);
        out
    } else {
        let mut tokens: Vec<String> = [
            (has_ctrl, "Ctrl"),
            (has_alt, "Alt"),
            (has_shift, "Shift"),
            (has_cmd, "Win"),
        ]
        .into_iter()
        .filter(|&(on, _)| on)
        .map(|(_, t)| t.to_string())
        .collect();
        tokens.push(key_label);
        tokens.join("+")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CanonicalChord {
    cmd: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
    key: String,
}

fn canonicalize_for_host(spec: &str, is_macos: bool) -> Option<CanonicalChord> {
    let (mods, raw_key) = split_keystroke(spec)?;
    let (cmd, ctrl, alt, shift) = parse_mods(&mods, is_macos)?;
    let key = match raw_key.to_ascii_lowercase().as_str() {
        "esc" => "escape".into(),
        "return" => "enter".into(),
        "pgup" => "pageup".into(),
        "pgdn" => "pagedown".into(),
        "del" => "delete".into(),
        other => other.to_string(),
    };
    Some(CanonicalChord {
        cmd,
        ctrl,
        alt,
        shift,
        key,
    })
}

fn scope_mask_for_context(ctx: &str) -> u8 {
    let c = ctx.to_ascii_lowercase();
    let (has_v, has_a, has_i) = (
        c.contains("video"),
        c.contains("audio"),
        c.contains("image") || c.contains("photo"),
    );
    if !has_v && !has_a && !has_i {
        return 0b1111;
    }
    (u8::from(has_v)) | (u8::from(has_a) << 1) | (u8::from(has_i) << 2)
}

fn scope_mask_for_action(entry: &ActionEntry) -> u8 {
    entry
        .gpui_contexts(entry.default_key)
        .iter()
        .fold(0u8, |acc, c| acc | scope_mask_for_context(c))
}

#[derive(Serialize, Deserialize, Default)]
struct PersistedKeymap {
    #[serde(default)]
    scheme: KeymapScheme,
    #[serde(default)]
    overrides: BTreeMap<String, String>,
}

/// Manages active shortcut scheme and user key overrides for core actions,
/// workspace presets (`"preset:<id>"`), and plugin commands (`"plugin:<plugin_id>:<cmd_id>"`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Keymap {
    pub scheme: KeymapScheme,
    pub overrides: BTreeMap<String, String>,
    backing_path: Option<PathBuf>,
}

impl Keymap {
    pub fn load() -> Self {
        keymap_path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Self {
        let mut km = Self {
            scheme: KeymapScheme::default(),
            overrides: BTreeMap::new(),
            backing_path: Some(path.to_path_buf()),
        };
        if let Ok(raw) = std::fs::read_to_string(path)
            && let Ok(parsed) = serde_json::from_str::<PersistedKeymap>(&raw)
        {
            km.scheme = parsed.scheme;
            for (k, v) in parsed.overrides {
                let (k, v) = (k.trim().to_string(), v.trim().to_string());
                if !k.is_empty() && !v.chars().any(char::is_control) {
                    km.overrides.insert(k, v);
                }
            }
        }
        km
    }

    pub fn save(&self) {
        let Some(p) = self.backing_path.clone().or_else(keymap_path) else {
            return;
        };
        if let Err(e) = self.save_to(&p) {
            log::error!("saving keymap to {} failed: {e}", p.display());
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let payload = PersistedKeymap {
            scheme: self.scheme,
            overrides: self.overrides.clone(),
        };
        let json = serde_json::to_string_pretty(&payload)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs_util::atomic_write(path, json.as_bytes())
    }

    pub fn set_scheme(&mut self, scheme: KeymapScheme) {
        if self.scheme != scheme {
            self.scheme = scheme;
            self.save();
        }
    }

    pub fn is_overridden(&self, target_id: &str) -> bool {
        self.overrides.contains_key(target_id)
    }

    pub fn effective_key(&self, action_id: &str) -> String {
        if let Some(custom) = self.overrides.get(action_id) {
            return custom.clone();
        }
        find_action(action_id)
            .map(|a| a.key_for_scheme(self.scheme).to_string())
            .unwrap_or_default()
    }

    pub fn set_binding(&mut self, target_id: &str, keystroke: String) {
        let target = target_id.trim();
        if target.is_empty() {
            return;
        }
        let clean: String = keystroke
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .trim()
            .to_string();
        if clean.is_empty() && find_action(target).is_none() {
            self.overrides.remove(target);
        } else {
            self.overrides.insert(target.to_string(), clean);
        }
        self.save();
    }

    pub fn reset_binding(&mut self, target_id: &str) {
        if self.overrides.remove(target_id.trim()).is_some() {
            self.save();
        }
    }

    pub fn reset_all(&mut self) {
        if !self.overrides.is_empty() {
            self.overrides.clear();
            self.save();
        }
    }

    pub fn preset_bindings(&self) -> Vec<(&str, &str)> {
        self.overrides
            .iter()
            .filter_map(|(k, v)| {
                let id = k.strip_prefix("preset:")?;
                (!id.is_empty() && !v.is_empty()).then_some((id, v.as_str()))
            })
            .collect()
    }

    pub fn plugin_bindings(&self) -> Vec<(&str, &str, &str)> {
        self.overrides
            .iter()
            .filter_map(|(k, v)| {
                let rest = k.strip_prefix("plugin:")?;
                let (plugin_id, cmd_id) = rest.split_once(':')?;
                (!plugin_id.is_empty() && !cmd_id.is_empty() && !v.is_empty()).then_some((
                    plugin_id,
                    cmd_id,
                    v.as_str(),
                ))
            })
            .collect()
    }

    pub fn find_conflict(&self, context: &str, keystroke: &str, ignore_id: &str) -> Option<String> {
        self.find_conflict_for_os(context, keystroke, ignore_id, cfg!(target_os = "macos"))
    }

    fn find_conflict_for_os(
        &self,
        context: &str,
        keystroke: &str,
        ignore_id: &str,
        is_macos: bool,
    ) -> Option<String> {
        let needle = canonicalize_for_host(keystroke, is_macos)?;
        let target_mask = if let Some(entry) = find_action(ignore_id) {
            if context.trim().is_empty() {
                scope_mask_for_action(entry)
            } else {
                scope_mask_for_context(context) | scope_mask_for_action(entry)
            }
        } else {
            scope_mask_for_context(context)
        };

        for entry in ACTIONS {
            if entry.id == ignore_id || (scope_mask_for_action(entry) & target_mask) == 0 {
                continue;
            }
            let effective = self.effective_key(entry.id);
            if canonicalize_for_host(&effective, is_macos).as_ref() == Some(&needle) {
                return Some(entry.title.to_string());
            }
        }

        for (id, spec) in &self.overrides {
            if id == ignore_id || spec.is_empty() || find_action(id).is_some() {
                continue;
            }
            if canonicalize_for_host(spec, is_macos).as_ref() == Some(&needle) {
                if let Some(preset_id) = id.strip_prefix("preset:") {
                    return Some(format!("工作区预设 ({preset_id})"));
                }
                if let Some(plugin_ref) = id.strip_prefix("plugin:") {
                    return Some(format!("插件命令 ({plugin_ref})"));
                }
                return Some(id.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_schemes_have_zero_conflicts_and_lock_text_input_firewall() {
        assert_eq!(ACTIONS.len(), 52);
        for &scheme in KeymapScheme::ALL {
            let km = Keymap {
                scheme,
                overrides: BTreeMap::new(),
                backing_path: None,
            };
            for is_macos in [true, false] {
                for entry in ACTIONS {
                    let key = km.effective_key(entry.id);
                    assert!(!key.is_empty(), "missing key for {}", entry.id);
                    assert_eq!(
                        km.find_conflict_for_os(entry.context, &key, entry.id, is_macos),
                        None
                    );
                    if !has_primary_modifier_or_f1_f12(&key) {
                        for &ctx in entry.gpui_contexts(&key) {
                            assert_ne!(ctx, "Workspace", "bare Workspace leak on {key}");
                        }
                        assert_ne!(
                            effective_gpui_context(entry.context, &key),
                            Some("Workspace")
                        );
                    }
                }
            }
        }
        assert_eq!(effective_gpui_context("Workspace", ""), None);
        for single in ["q", "s", "shift-g", "space", "tab", "1", "-", "f13"] {
            assert_eq!(
                effective_gpui_context("Workspace", single),
                Some("Workspace && !TextInput")
            );
        }
        for chord in ["secondary-p", "cmd-q", "ctrl-o", "alt-w", "f1", "f5", "f12"] {
            assert_eq!(
                effective_gpui_context("Workspace", chord),
                Some("Workspace")
            );
        }
    }

    #[test]
    fn subtitle_cue_shortcuts_are_video_scoped_in_each_scheme() {
        for (id, iris_key, mpv_key) in [
            ("sub_seek_prev", "shift-,", "ctrl-left"),
            ("sub_seek_next", "shift-.", "ctrl-right"),
        ] {
            let action = find_action(id).unwrap();
            for &scheme in KeymapScheme::ALL {
                let key = action.key_for_scheme(scheme);
                let expected = if scheme == KeymapScheme::MpvClassic {
                    mpv_key
                } else {
                    iris_key
                };
                assert_eq!(key, expected);
                assert_eq!(action.gpui_contexts(key), &["Viewport && media == video"]);
            }
        }
    }

    #[test]
    fn keymap_display_overrides_photo_navigation_and_persistence() {
        assert_eq!(format_keystroke_display("secondary-p", true), "⌘P");
        assert_eq!(format_keystroke_display("secondary-p", false), "Ctrl+P");
        assert_eq!(
            normalize_gpui_keystroke(" ", false, false, false, false),
            Some("space".into())
        );

        let dir = std::env::temp_dir().join(format!("iris-keymap-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("keymap.json");

        let mut km = Keymap::load_from(&file);
        // Photo left/right navigation participates in conflict detection against image actions.
        assert_eq!(
            km.find_conflict("Viewport && media == image", "left", "zoom_in")
                .as_deref(),
            Some("上一张照片")
        );
        // Video `left` does not conflict with photo `left` (disjoint media scopes).
        assert!(
            km.find_conflict("Viewport && media == image", "left", "photo_prev")
                .is_none()
        );

        km.set_binding("preset:cinema", "alt-1".into());
        km.set_binding("plugin:iris.guides:thirds", "shift-g".into());
        let reloaded = Keymap::load_from(&file);
        assert_eq!(reloaded.preset_bindings(), vec![("cinema", "alt-1")]);
        assert_eq!(
            reloaded.plugin_bindings(),
            vec![("iris.guides", "thirds", "shift-g")]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
