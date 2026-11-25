use super::{Id, stream::RemoteResource};
use crate::{Color, OverlayNode};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "layout", rename_all = "snake_case", deny_unknown_fields)]
pub enum CueLayout {
    Scroll,
    Top,
    Bottom,
    Positioned { x: f32, y: f32 },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "content", rename_all = "snake_case", deny_unknown_fields)]
pub enum CueContent {
    Text {
        text: String,
        size: u16,
        color: Color,
        #[serde(default)]
        bold: bool,
    },
    /// Only passive vector nodes are permitted; interactive Button is rejected.
    Vector { nodes: Vec<OverlayNode> },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimedCue {
    pub id: String,
    pub start_ms: Id,
    pub duration_ms: Id,
    pub layout: CueLayout,
    pub content: CueContent,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayWindow {
    pub revision: Id,
    pub start_ms: Id,
    pub end_ms: Id,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeMarker {
    pub id: String,
    pub position_ms: Id,
    pub label: String,
    pub color: Option<ColorLabel>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorLabel {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Gray,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "edit", rename_all = "snake_case", deny_unknown_fields)]
pub enum AnnotationEdit {
    Rating { rating: u8 },
    Color { color: Option<ColorLabel> },
    UpsertMarker { marker: TimeMarker },
    RemoveMarker { id: String },
}

/// One selected item's metadata, with at most 32 markers per page. No media URI
/// or local path is disclosed through the annotations capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotationPage {
    pub index: u32,
    pub rating: u8,
    pub color: Option<ColorLabel>,
    pub markers: Vec<TimeMarker>,
    pub next_marker: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnhancementResource {
    Remote { request: RemoteResource },
    Artifact { handle: Id },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleFormat {
    SubRip,
    WebVtt,
    Ass,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chapter {
    pub start_ms: Id,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeatRange {
    pub start_ms: Id,
    pub end_ms: Id,
    /// Relative intensity, 0..=100. The host chooses presentation colors.
    pub intensity: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkipRange {
    pub start_ms: Id,
    pub end_ms: Id,
    pub label: String,
}
