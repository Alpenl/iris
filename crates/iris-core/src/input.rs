//! Pointer policy mirrors keyboard contexts; controls stop events before the viewport.
use crate::library::Kind;

#[derive(Clone, Copy)]
pub struct InputContext {
    pub media: Option<Kind>,
    pub zoomed: bool,
    pub overlay: bool,
    pub video_wheel_volume: bool,
    pub single_click_pause: bool,
    pub photo_double_click_actual: bool,
}

pub enum PointerInput {
    Wheel(f64),
    Pinch(f64),
    SingleClick,
    DoubleClick,
    BeginDrag,
}

pub enum PointerCommand {
    Zoom(f64),
    VolumeBy(f64),
    TogglePlayback,
    ToggleFullscreen,
    ToggleActualAtCursor,
    BeginPan,
    CloseOverlay,
}

pub fn resolve_pointer_input(input: PointerInput, context: InputContext) -> Option<PointerCommand> {
    if context.overlay {
        return match input {
            PointerInput::SingleClick | PointerInput::DoubleClick | PointerInput::BeginDrag => {
                Some(PointerCommand::CloseOverlay)
            }
            _ => None,
        };
    }
    match input {
        PointerInput::Pinch(delta) if context.media == Some(Kind::Photo) => {
            Some(PointerCommand::Zoom(delta))
        }
        PointerInput::Wheel(delta) if context.media == Some(Kind::Photo) => {
            Some(PointerCommand::Zoom(delta))
        }
        PointerInput::Wheel(delta)
            if matches!(context.media, Some(Kind::Video | Kind::Audio))
                && context.video_wheel_volume =>
        {
            Some(PointerCommand::VolumeBy(delta))
        }
        PointerInput::DoubleClick
            if context.media == Some(Kind::Photo) && context.photo_double_click_actual =>
        {
            Some(PointerCommand::ToggleActualAtCursor)
        }
        PointerInput::DoubleClick => Some(PointerCommand::ToggleFullscreen),
        PointerInput::SingleClick
            if matches!(context.media, Some(Kind::Video | Kind::Audio))
                && context.single_click_pause =>
        {
            Some(PointerCommand::TogglePlayback)
        }
        PointerInput::BeginDrag if context.media == Some(Kind::Photo) && context.zoomed => {
            Some(PointerCommand::BeginPan)
        }
        _ => None,
    }
}
