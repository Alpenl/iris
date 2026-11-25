use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub fn is_valid_hex_color(s: &str) -> bool {
    s.strip_prefix('#')
        .is_some_and(|h| (h.len() == 6 || h.len() == 8) && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Semantic host token (`white`, `muted`, `dim`, `dark`, `accent`) or arbitrary
/// `#RRGGBB` / `#RRGGBBAA` hex colour.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    White,
    Muted,
    Dim,
    Dark,
    Accent,
    Hex(String),
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Color::White => serializer.serialize_str("white"),
            Color::Muted => serializer.serialize_str("muted"),
            Color::Dim => serializer.serialize_str("dim"),
            Color::Dark => serializer.serialize_str("dark"),
            Color::Accent => serializer.serialize_str("accent"),
            Color::Hex(hex) => serializer.serialize_str(hex),
        }
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = Color;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("white, muted, dim, dark, accent, or #RRGGBB[AA]")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                match v {
                    "white" | "text" => Ok(Color::White),
                    "muted" => Ok(Color::Muted),
                    "dim" => Ok(Color::Dim),
                    "dark" | "panel" => Ok(Color::Dark),
                    "accent" => Ok(Color::Accent),
                    hex if is_valid_hex_color(hex) => Ok(Color::Hex(hex.to_string())),
                    _ => Err(E::custom("invalid color token or hex string")),
                }
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

/// Runtime visual theme palette (`#RRGGBB` / `#RRGGBBAA`) and layout metrics.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiThemeSpec {
    pub name: Option<String>,
    pub window_bg: Option<String>,
    pub panel: Option<String>,
    pub hover: Option<String>,
    pub selected: Option<String>,
    pub line: Option<String>,
    pub line_strong: Option<String>,
    pub text: Option<String>,
    pub text_muted: Option<String>,
    pub text_dim: Option<String>,
    pub text_faint: Option<String>,
    pub accent: Option<String>,
    pub overlay: Option<String>,
    pub scrim: Option<String>,
    pub titlebar_height: Option<f32>,
    pub dock_height: Option<f32>,
    pub row_height: Option<f32>,
    pub corner_radius: Option<f32>,
}

/// Runtime window and control-bar structural customization (`hide_titlebar`,
/// `floating_dock`, `show_titlebar_meta`, `compact_controls`, `hidden_dock_buttons`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChromeLayoutSpec {
    pub hide_titlebar: Option<bool>,
    pub floating_dock: Option<bool>,
    pub show_titlebar_meta: Option<bool>,
    pub compact_controls: Option<bool>,
    pub hidden_dock_buttons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ButtonSpec {
    pub label: String,
    pub action_id: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub accent: bool,
}

/// Unified interactive widget tree used inside both [`SidebarPanel`] and [`PluginModal`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Widget {
    Label {
        text: String,
        #[serde(default)]
        muted: bool,
        #[serde(default)]
        mono: bool,
    },
    Stat {
        label: String,
        value: String,
        #[serde(default)]
        accent: bool,
    },
    Progress {
        label: String,
        ratio: f32,
    },
    Slider {
        id: String,
        label: String,
        value: f64,
        min: f64,
        max: f64,
        #[serde(default = "default_slider_step")]
        step: f64,
        #[serde(default)]
        unit: Option<String>,
        action_id: String,
    },
    Button {
        label: String,
        action_id: String,
        #[serde(default)]
        icon: Option<String>,
        #[serde(default)]
        accent: bool,
    },
    ButtonGroup {
        buttons: Vec<ButtonSpec>,
    },
    Input {
        id: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        value: String,
        #[serde(default)]
        placeholder: String,
        #[serde(default)]
        secret: bool,
        /// Optional `action_id` fired immediately on Enter in sidebar mode.
        #[serde(default)]
        action_id: Option<String>,
    },
    Toggle {
        id: String,
        label: String,
        #[serde(default)]
        checked: bool,
        #[serde(default)]
        action_id: Option<String>,
    },
    Select {
        id: String,
        #[serde(default)]
        label: Option<String>,
        selected: String,
        options: Vec<SelectOption>,
        #[serde(default)]
        action_id: Option<String>,
    },
    ListItem {
        id: String,
        title: String,
        #[serde(default)]
        subtitle: Option<String>,
        #[serde(default)]
        badge: Option<String>,
        #[serde(default)]
        active: bool,
        action_id: String,
    },
    /// Scannable 2D QR grid (`matrix` rows of `'1'`/`'0'` or `'#'`/`' '`) for mobile app auth.
    QrCode {
        label: String,
        #[serde(default)]
        data: String,
        #[serde(default)]
        matrix: Vec<String>,
        #[serde(default)]
        hint: Option<String>,
        #[serde(default)]
        poll_action_id: Option<String>,
    },
    Link {
        label: String,
        url: String,
        #[serde(default)]
        button_text: Option<String>,
    },
    Divider,
}

fn default_slider_step() -> f64 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarSection {
    pub heading: String,
    #[serde(default)]
    pub widgets: Vec<Widget>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarPanel {
    pub title: String,
    #[serde(default)]
    pub sections: Vec<SidebarSection>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginModal {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub widgets: Vec<Widget>,
    #[serde(default)]
    pub submit_label: Option<String>,
    #[serde(default)]
    pub cancel_label: Option<String>,
}

/// Compact badge rendered inside the control bar status slot (`Permission::UiStatus`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusBadge {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub tooltip: Option<String>,
    #[serde(default)]
    pub accent: bool,
    #[serde(default)]
    pub action_id: Option<String>,
}

/// Custom button contributed onto the main control bar (`Permission::UiToolbar`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolbarButton {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub tooltip: Option<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub accent: bool,
    pub action_id: String,
}
