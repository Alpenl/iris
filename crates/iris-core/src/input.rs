//! Pointer policy mirrors keyboard contexts; controls stop events before the viewport.
use crate::library::Kind;

#[derive(Clone, Copy)]
pub struct InputContext {
    pub media: Option<Kind>,
    pub zoomed: bool,
    pub overlay: bool,
    pub video_wheel_volume: bool,
}

pub enum PointerInput {
    Wheel(f64),
    DoubleClick,
    BeginDrag,
}

pub enum PointerCommand {
    Zoom(f64),
    VolumeBy(f64),
    ToggleFullscreen,
    BeginPan,
    CloseOverlay,
}

pub fn resolve_pointer_input(input: PointerInput, context: InputContext) -> Option<PointerCommand> {
    if context.overlay {
        return match input {
            PointerInput::DoubleClick | PointerInput::BeginDrag => {
                Some(PointerCommand::CloseOverlay)
            }
            _ => None,
        };
    }
    match input {
        PointerInput::Wheel(delta) if context.media == Some(Kind::Photo) => {
            Some(PointerCommand::Zoom(delta))
        }
        PointerInput::Wheel(delta)
            if context.media == Some(Kind::Video) && context.video_wheel_volume =>
        {
            Some(PointerCommand::VolumeBy(delta))
        }
        PointerInput::DoubleClick => Some(PointerCommand::ToggleFullscreen),
        PointerInput::BeginDrag if context.media == Some(Kind::Photo) && context.zoomed => {
            Some(PointerCommand::BeginPan)
        }
        _ => None,
    }
}
