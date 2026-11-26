use serde::{Deserialize, Serialize};

use crate::workbench::Color;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// Normalized (`0.0..=1.0`) vector & HUD nodes drawn over the viewport (`Permission::UiOverlay`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OverlayNode {
    /// Unified box primitive supporting fill, border stroke, and corner radius.
    Box {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        #[serde(default)]
        fill: Option<Color>,
        #[serde(default)]
        border: Option<Color>,
        #[serde(default = "default_stroke")]
        border_width: f32,
        #[serde(default)]
        radius: f32,
        #[serde(default = "default_opacity")]
        opacity: f32,
    },
    /// Crisp horizontal or vertical guide line.
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        #[serde(default)]
        color: Color,
        #[serde(default = "default_stroke")]
        width: f32,
        #[serde(default = "default_opacity")]
        opacity: f32,
    },
    /// Evenly spaced `cols × rows` grid across the viewport.
    Grid {
        cols: u8,
        rows: u8,
        #[serde(default)]
        color: Color,
        #[serde(default = "default_opacity")]
        opacity: f32,
    },
    /// Centered crosshair at `(x, y)` with arm length `size`.
    Crosshair {
        x: f32,
        y: f32,
        size: f32,
        #[serde(default)]
        color: Color,
        #[serde(default = "default_opacity")]
        opacity: f32,
    },
    /// Unified text node for timecodes, labels, subtitles, or danmaku (`size` in px `9..=64`).
    Text {
        x: f32,
        y: f32,
        text: String,
        #[serde(default = "default_text_size")]
        size: f32,
        #[serde(default)]
        color: Color,
        #[serde(default)]
        bg: Option<Color>,
        #[serde(default)]
        mono: bool,
        #[serde(default)]
        align: TextAlign,
        #[serde(default = "default_text_opacity")]
        opacity: f32,
    },
    /// Vertical bar series (`values` in `0.0..=1.0`) for spectrums, waveforms, or histograms.
    Bars {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        values: Vec<f32>,
        #[serde(default)]
        color: Color,
        #[serde(default = "default_opacity")]
        opacity: f32,
    },
    /// Interactive clickable HUD button anchored at normalized `(x, y)` without stealing focus.
    Button {
        id: String,
        x: f32,
        y: f32,
        label: String,
        #[serde(default)]
        icon: Option<String>,
        #[serde(default)]
        accent: bool,
        action_id: String,
    },
}

fn default_opacity() -> f32 {
    0.6
}

fn default_text_opacity() -> f32 {
    0.92
}

fn default_text_size() -> f32 {
    12.0
}

fn default_stroke() -> f32 {
    1.0
}

#[cfg(test)]
mod tests {
    use super::{OverlayNode, TextAlign};
    use crate::workbench::Color;

    #[test]
    fn overlay_nodes_deserialize_defaults_and_colors() {
        let json =
            r#"{"kind":"text","x":0.5,"y":0.1,"text":"01:23:45","color":"text","align":"center"}"#;
        let node: OverlayNode = serde_json::from_str(json).unwrap();
        assert_eq!(
            node,
            OverlayNode::Text {
                x: 0.5,
                y: 0.1,
                text: "01:23:45".into(),
                size: 12.0,
                color: Color::White,
                bg: None,
                mono: false,
                align: TextAlign::Center,
                opacity: 0.92,
            }
        );

        let bad_color = r##"{"kind":"grid","cols":3,"rows":3,"color":"#12345"}"##;
        assert!(serde_json::from_str::<OverlayNode>(bad_color).is_err());

        let unknown_field = r#"{"kind":"grid","cols":3,"rows":3,"unexpected":true}"#;
        assert!(serde_json::from_str::<OverlayNode>(unknown_field).is_err());
    }
}
