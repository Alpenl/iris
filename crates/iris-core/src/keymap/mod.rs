//! Customizable keyboard shortcut schemes, `!TextInput` typing safety firewall,
//! conflict detection, and per-action/preset/plugin key bindings (`keymap.json`).

mod catalog;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs_util;

pub use catalog::{ACTIONS, ActionCategory, ActionEntry, KeymapScheme, actions, find_action};

/// Fixed application shortcuts shared by runtime registration and conflict detection.
pub const RESERVED_SHORTCUTS: [(&str, &str); 4] = [
    ("secondary-i", "toggle_info"),
    ("secondary-shift-p", "open_palette"),
    ("escape", "exit_fullscreen"),
    ("secondary-alt-shift-r", "reset_appearance"),
];

/// Semantic owner of an existing conflicting shortcut binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictTarget {
    Reserved(&'static str),
    Action(&'static str),
    Preset(String),
    Plugin(String),
    Other(String),
}

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
        let mods: Vec<&str> = prefix.split('-').collect();
        return Some((mods, "-"));
    }
    let mut parts: Vec<&str> = trimmed.split('-').collect();
    let key = parts.pop()?;
    (!key.is_empty()).then_some((parts, key))
}

fn safe_while_typing(chord: &CanonicalChord, is_macos: bool) -> bool {
    if chord.alt {
        return false; // Option and AltGr can produce text, including dead keys.
    }
    let primary = if is_macos { chord.cmd } else { chord.ctrl };
    if primary {
        return !matches!(
            chord.key.as_str(),
            "a" | "c"
                | "v"
                | "x"
                | "y"
                | "z"
                | "backspace"
                | "delete"
                | "left"
                | "right"
                | "up"
                | "down"
                | "home"
                | "end"
                | "insert"
        );
    }
    !chord.cmd
        && !chord.ctrl
        && chord
            .key
            .strip_prefix('f')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=12).contains(&n))
}

/// Resolves a GPUI predicate while preserving text entry, navigation, and editing chords.
pub fn effective_gpui_context(declared_ctx: &str, keystroke: &str) -> Option<&'static str> {
    effective_gpui_context_for_os(declared_ctx, keystroke, cfg!(target_os = "macos"))
}

fn effective_gpui_context_for_os(
    declared_ctx: &str,
    keystroke: &str,
    is_macos: bool,
) -> Option<&'static str> {
    let chord = canonicalize_for_host(keystroke, is_macos)?;
    match declared_ctx.trim() {
        "Workspace" => {
            if safe_while_typing(&chord, is_macos) {
                Some("Workspace && !Modal")
            } else {
                Some("Workspace && !TextInput && !Modal")
            }
        }
        "Workspace && !TextInput" | "Workspace && !TextInput && !Modal" => {
            Some("Workspace && !TextInput && !Modal")
        }
        "Viewport" => Some("Viewport"),
        "Viewport && media == video" => Some("Viewport && media == video"),
        "Viewport && media == image" => Some("Viewport && media == image"),
        "Viewport && media == audio" => Some("Viewport && media == audio"),
        _ => None,
    }
}

/// Converts a GPUI key-down to a portable shortcut, recording the native primary modifier
/// as `secondary` and preserving the other physical modifiers.
pub fn normalize_gpui_keystroke(
    key: &str,
    modifiers_cmd: bool,
    modifiers_ctrl: bool,
    modifiers_alt: bool,
    modifiers_shift: bool,
) -> Option<String> {
    let is_macos = cfg!(target_os = "macos");
    let key = if key == " " { "space" } else { key };
    let mut spec = String::new();
    for (on, prefix) in [
        (
            if is_macos {
                modifiers_cmd
            } else {
                modifiers_ctrl
            },
            "secondary-",
        ),
        (modifiers_cmd && !is_macos, "cmd-"),
        (modifiers_ctrl && is_macos, "ctrl-"),
        (modifiers_alt, "alt-"),
        (modifiers_shift, "shift-"),
    ] {
        if on {
            spec.push_str(prefix);
        }
    }
    spec.push_str(key);
    canonical_binding(&spec)
}

fn normalize_key(raw: &str) -> Option<String> {
    let base = raw.to_ascii_lowercase();
    if base.is_empty()
        || (base.contains('-') && base != "-")
        || base.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    Some(match base.as_str() {
        "shift" | "control" | "ctrl" | "alt" | "option" | "cmd" | "command" | "platform"
        | "super" | "meta" | "win" | "capslock" | "fn" | "function" | "secondary" => return None,
        "esc" => "escape".into(),
        "return" => "enter".into(),
        "pgup" => "pageup".into(),
        "pgdn" => "pagedown".into(),
        "del" => "delete".into(),
        "arrowleft" => "left".into(),
        "arrowright" => "right".into(),
        "arrowup" => "up".into(),
        "arrowdown" => "down".into(),
        _ => base,
    })
}

/// Normalizes spelling and rejects malformed chords for storage and conflict checks.
pub fn canonical_binding(spec: &str) -> Option<String> {
    let (mods, raw_key) = split_keystroke(spec)?;
    let (cmd, ctrl, alt, mut shift) = parse_mods(&mods, false)?;
    let secondary = mods.iter().any(|m| m.eq_ignore_ascii_case("secondary"));
    let explicit_ctrl = mods
        .iter()
        .any(|m| m.eq_ignore_ascii_case("ctrl") || m.eq_ignore_ascii_case("control"));
    let normalized_key = normalize_key(raw_key)?;
    let mut key = normalized_key.as_str();
    shift |= raw_key.len() == 1 && raw_key.as_bytes()[0].is_ascii_uppercase();
    if cmd || ctrl || alt {
        // Modified glyph aliases carry an implicit Shift: Ctrl+? is Ctrl+Shift+/.
        if let Some(base) = punctuation_equivalent(key, SHIFTED_KEYS, UNSHIFTED_KEYS) {
            key = base;
            shift = true;
        }
    } else if shift {
        // Bare symbols keep their glyph spelling (e.g. the default + binding).
        if let Some(glyph) = punctuation_equivalent(key, UNSHIFTED_KEYS, SHIFTED_KEYS) {
            key = glyph;
            shift = false;
        } else if key.len() == 1 && SHIFTED_KEYS.contains(key) {
            shift = false;
        }
    }
    let mut out = String::with_capacity(spec.len());
    for (on, prefix) in [
        (secondary, "secondary-"),
        (cmd, "cmd-"),
        (ctrl && explicit_ctrl, "ctrl-"),
        (alt, "alt-"),
        (shift, "shift-"),
    ] {
        if on {
            out.push_str(prefix);
        }
    }
    out.push_str(key);
    Some(out)
}

const UNSHIFTED_KEYS: &str = "`1234567890-=[]\\;',./";
const SHIFTED_KEYS: &str = "~!@#$%^&*()_+{}|:\"<>?";

fn punctuation_equivalent(key: &str, from: &str, to: &'static str) -> Option<&'static str> {
    if key.len() != 1 {
        return None;
    }
    let i = from.find(key)?;
    to.get(i..i + 1)
}

/// Converts a portable shortcut into GPUI's keybinding spelling. GPUI reports the
/// shifted glyph and clears Shift for these keys, even with other modifiers held.
pub fn gpui_binding(spec: &str) -> Option<String> {
    let mut binding = canonical_binding(spec)?;
    if let Some((prefix, key)) = binding.rsplit_once("shift-")
        && let Some(glyph) = punctuation_equivalent(key, UNSHIFTED_KEYS, SHIFTED_KEYS)
    {
        binding.truncate(prefix.len());
        binding.push_str(glyph);
    }
    Some(binding)
}

/// Whether a chord is owned by an invariant application shortcut on this platform.
pub fn reserved_shortcut(keystroke: &str) -> Option<&'static str> {
    reserved_for_os(keystroke, cfg!(target_os = "macos"))
}

fn reserved_for_os(keystroke: &str, is_macos: bool) -> Option<&'static str> {
    let needle = canonicalize_for_host(keystroke, is_macos)?;
    RESERVED_SHORTCUTS.into_iter().find_map(|(key, id)| {
        (canonicalize_for_host(key, is_macos).as_ref() == Some(&needle)).then_some(id)
    })
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
    let Some(normalized) = canonical_binding(spec) else {
        return String::new();
    };
    let (mods, raw_key) = split_keystroke(&normalized).expect("canonical binding has a key");
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
    let normalized = canonical_binding(spec)?;
    let (mods, raw_key) = split_keystroke(&normalized)?;
    let (cmd, ctrl, alt, shift) = parse_mods(&mods, is_macos)?;
    let key = raw_key.to_string();
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
        if let Ok(raw) = fs_util::retry_sharing_violation(|| std::fs::read_to_string(path))
            && let Ok(parsed) = serde_json::from_str::<PersistedKeymap>(&raw)
        {
            km.scheme = parsed.scheme;
            for (k, v) in parsed.overrides {
                let k = k.trim();
                if k.is_empty() || k.chars().any(char::is_control) {
                    continue;
                }
                let v = v.trim();
                if v.is_empty() {
                    if find_action(k).is_some() {
                        km.overrides.insert(k.to_string(), String::new());
                    }
                } else if let Some(spec) = canonical_binding(v)
                    && reserved_shortcut(&spec).is_none()
                {
                    km.overrides.insert(k.to_string(), spec);
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
        let clean = if keystroke.trim().is_empty() {
            String::new()
        } else if let Some(spec) = canonical_binding(&keystroke) {
            if reserved_shortcut(&spec).is_some() {
                return;
            }
            spec
        } else {
            return;
        };
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

    pub fn find_conflict(
        &self,
        context: &str,
        keystroke: &str,
        ignore_id: &str,
    ) -> Option<ConflictTarget> {
        self.find_conflict_for_os(context, keystroke, ignore_id, cfg!(target_os = "macos"))
    }

    fn find_conflict_for_os(
        &self,
        context: &str,
        keystroke: &str,
        ignore_id: &str,
        is_macos: bool,
    ) -> Option<ConflictTarget> {
        let needle = canonicalize_for_host(keystroke, is_macos)?;
        if let Some(id) = reserved_for_os(keystroke, is_macos) {
            return Some(ConflictTarget::Reserved(id));
        }
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
                return Some(ConflictTarget::Action(entry.id));
            }
        }

        for (id, spec) in &self.overrides {
            if id == ignore_id || spec.is_empty() || find_action(id).is_some() {
                continue;
            }
            if canonicalize_for_host(spec, is_macos).as_ref() == Some(&needle) {
                if let Some(preset_id) = id.strip_prefix("preset:") {
                    return Some(ConflictTarget::Preset(preset_id.to_string()));
                }
                if let Some(plugin_ref) = id.strip_prefix("plugin:") {
                    return Some(ConflictTarget::Plugin(plugin_ref.to_string()));
                }
                return Some(ConflictTarget::Other(id.clone()));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ACTIONS, ConflictTarget, Keymap, KeymapScheme, RESERVED_SHORTCUTS, canonical_binding,
        canonicalize_for_host, effective_gpui_context, effective_gpui_context_for_os, find_action,
        format_keystroke_display, normalize_gpui_keystroke, reserved_for_os, safe_while_typing,
    };
    use crate::test_util::TestDir;
    use std::collections::BTreeMap;

    #[test]
    fn builtin_schemes_have_zero_conflicts_and_lock_text_input_firewall() {
        assert_eq!(ACTIONS.len(), 54);
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
                    if !safe_while_typing(&canonicalize_for_host(&key, is_macos).unwrap(), is_macos)
                    {
                        for &ctx in entry.gpui_contexts_for_os(&key, is_macos) {
                            assert_ne!(ctx, "Workspace", "bare Workspace leak on {key}");
                        }
                        assert_ne!(
                            effective_gpui_context_for_os(entry.context, &key, is_macos),
                            Some("Workspace")
                        );
                    }
                    assert_eq!(
                        entry.gpui_contexts(&key),
                        entry.gpui_contexts_for_os(&key, cfg!(target_os = "macos"))
                    );
                }
            }
        }
        assert_eq!(effective_gpui_context("Workspace", ""), None);
        for single in [
            "q",
            "s",
            "shift-g",
            "space",
            "tab",
            "1",
            "-",
            "f13",
            "alt-w",
            "secondary-v",
            "secondary-shift-left",
        ] {
            assert_eq!(
                effective_gpui_context("Workspace", single),
                Some("Workspace && !TextInput && !Modal")
            );
        }
        for chord in [
            "secondary-p",
            "secondary-q",
            "secondary-o",
            "secondary-,",
            "f1",
            "f5",
            "f12",
        ] {
            assert_eq!(
                effective_gpui_context("Workspace", chord),
                Some("Workspace && !Modal")
            );
        }
    }

    #[test]
    fn reserved_and_equivalent_chords_are_detected_on_both_hosts() {
        let km = Keymap::default();
        assert_eq!(canonical_binding("ctrl--a"), None);
        assert_eq!(canonical_binding("ctrl---"), None);
        assert_eq!(canonical_binding("control--").as_deref(), Some("ctrl--"));
        for (is_macos, primary) in [(true, "cmd"), (false, "ctrl")] {
            for (key, action) in RESERVED_SHORTCUTS {
                let physical = key.replace("secondary", primary);
                assert_eq!(reserved_for_os(&physical, is_macos), Some(action));
                assert_eq!(
                    km.find_conflict_for_os("Viewport", &physical, "zoom_in", is_macos),
                    Some(ConflictTarget::Reserved(action)),
                );
            }
            assert_eq!(
                canonicalize_for_host("shift-,", is_macos),
                canonicalize_for_host("<", is_macos)
            );
            let mut km = Keymap::default();
            km.overrides
                .insert("take_screenshot".into(), "secondary-?".into());
            let query = format!("{primary}-shift-/");
            assert_eq!(
                km.find_conflict_for_os("Workspace", &query, "preset:review", is_macos),
                Some(ConflictTarget::Action("take_screenshot")),
            );
            assert!(
                km.find_conflict_for_os(
                    "Workspace",
                    &format!("{primary}-/"),
                    "preset:review",
                    is_macos,
                )
                .is_none()
            );
            assert_eq!(
                canonicalize_for_host("arrowleft", is_macos),
                canonicalize_for_host("left", is_macos)
            );
        }
        let mac = cfg!(target_os = "macos");
        assert_eq!(
            normalize_gpui_keystroke("p", mac, !mac, false, true).as_deref(),
            Some("secondary-shift-p")
        );
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

        let dir = TestDir::new("keymap-split");
        let file = dir.join("keymap.json");

        let mut km = Keymap::load_from(&file);
        // Photo left/right navigation participates in conflict detection against image actions.
        assert_eq!(
            km.find_conflict("Viewport && media == image", "left", "zoom_in"),
            Some(ConflictTarget::Action("photo_prev"))
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

        // R1-L09: `load_from` normalizes via `canonical_binding` and drops malformed/reserved entries.
        std::fs::write(
            &file,
            r#"{
                "scheme": "iris_default",
                "overrides": {
                    "zoom_in": "control--",
                    "photo_next": "ArrowRight",
                    "toggle_info": "escape",
                    "preset:bad": "ctrl--a",
                    "preset:empty": "",
                    "toggle_favorite": ""
                }
            }"#,
        )
        .unwrap();
        let sanitized = Keymap::load_from(&file);
        assert_eq!(
            sanitized.overrides.get("zoom_in").map(String::as_str),
            Some("ctrl--")
        );
        assert_eq!(
            sanitized.overrides.get("photo_next").map(String::as_str),
            Some("right")
        );
        assert!(!sanitized.overrides.contains_key("toggle_info"));
        assert!(!sanitized.overrides.contains_key("preset:bad"));
        assert!(!sanitized.overrides.contains_key("preset:empty"));
        assert_eq!(
            sanitized
                .overrides
                .get("toggle_favorite")
                .map(String::as_str),
            Some("")
        );
    }
}
