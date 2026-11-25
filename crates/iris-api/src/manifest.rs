use serde::{Deserialize, Serialize};

fn is_windows_reserved_stem(stem: &str) -> bool {
    let b = stem.as_bytes();
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (b.len() == 4
        && b[3].is_ascii_digit()
        && (b[..3].eq_ignore_ascii_case(b"COM") || b[..3].eq_ignore_ascii_case(b"LPT")))
}

pub fn is_valid_id(s: &str) -> bool {
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && !s.contains("..")
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        && !s.split('.').any(is_windows_reserved_stem)
}

/// Command contributed to the host Command Palette (`⌘P` / `Ctrl+P`) and Keymap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub shortcut_hint: Option<String>,
    /// Also show this command in the current viewport's context menu. This
    /// grants no additional state, selection or filesystem access.
    #[serde(default)]
    pub context_menu: bool,
}

/// Declares that a `MediaSource` plugin can dynamically create user-configured
/// connection instances (e.g. multiple WebDAV servers, S3 buckets, or NAS shares).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceProviderSpec {
    pub label: String,
    pub add_command_id: String,
}
