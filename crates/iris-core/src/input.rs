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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(media: Option<Kind>) -> InputContext {
        InputContext {
            media,
            zoomed: false,
            overlay: false,
            video_wheel_volume: true,
            single_click_pause: true,
            photo_double_click_actual: true,
        }
    }

    #[test]
    fn overlay_closes_on_click_and_drag_but_ignores_wheel_and_pinch() {
        let mut c = ctx(Some(Kind::Video));
        c.overlay = true;
        assert!(matches!(
            resolve_pointer_input(PointerInput::SingleClick, c),
            Some(PointerCommand::CloseOverlay)
        ));
        assert!(matches!(
            resolve_pointer_input(PointerInput::DoubleClick, c),
            Some(PointerCommand::CloseOverlay)
        ));
        assert!(matches!(
            resolve_pointer_input(PointerInput::BeginDrag, c),
            Some(PointerCommand::CloseOverlay)
        ));
        assert!(resolve_pointer_input(PointerInput::Wheel(1.0), c).is_none());
        assert!(resolve_pointer_input(PointerInput::Pinch(0.5), c).is_none());
    }

    #[test]
    fn video_and_audio_pointer_policies_respect_settings() {
        for kind in [Kind::Video, Kind::Audio] {
            let mut c = ctx(Some(kind));
            assert!(matches!(
                resolve_pointer_input(PointerInput::SingleClick, c),
                Some(PointerCommand::TogglePlayback)
            ));
            assert!(matches!(
                resolve_pointer_input(PointerInput::DoubleClick, c),
                Some(PointerCommand::ToggleFullscreen)
            ));
            assert!(matches!(
                resolve_pointer_input(PointerInput::Wheel(5.0), c),
                Some(PointerCommand::VolumeBy(d)) if (d - 5.0).abs() < 1e-9
            ));
            assert!(resolve_pointer_input(PointerInput::Pinch(0.3), c).is_none());
            assert!(resolve_pointer_input(PointerInput::BeginDrag, c).is_none());

            c.single_click_pause = false;
            c.video_wheel_volume = false;
            assert!(resolve_pointer_input(PointerInput::SingleClick, c).is_none());
            assert!(resolve_pointer_input(PointerInput::Wheel(5.0), c).is_none());
        }
    }

    #[test]
    fn photo_pointer_policies_handle_zoom_pan_pinch_and_double_click() {
        let mut c = ctx(Some(Kind::Photo));
        assert!(matches!(
            resolve_pointer_input(PointerInput::Wheel(0.25), c),
            Some(PointerCommand::Zoom(d)) if (d - 0.25).abs() < 1e-9
        ));
        assert!(matches!(
            resolve_pointer_input(PointerInput::Pinch(0.5), c),
            Some(PointerCommand::Zoom(d)) if (d - 0.5).abs() < 1e-9
        ));
        assert!(matches!(
            resolve_pointer_input(PointerInput::DoubleClick, c),
            Some(PointerCommand::ToggleActualAtCursor)
        ));
        assert!(resolve_pointer_input(PointerInput::SingleClick, c).is_none());
        assert!(resolve_pointer_input(PointerInput::BeginDrag, c).is_none());

        c.zoomed = true;
        assert!(matches!(
            resolve_pointer_input(PointerInput::BeginDrag, c),
            Some(PointerCommand::BeginPan)
        ));

        c.photo_double_click_actual = false;
        assert!(matches!(
            resolve_pointer_input(PointerInput::DoubleClick, c),
            Some(PointerCommand::ToggleFullscreen)
        ));
    }

    #[test]
    fn empty_viewport_only_toggles_fullscreen_on_double_click() {
        let c = ctx(None);
        assert!(matches!(
            resolve_pointer_input(PointerInput::DoubleClick, c),
            Some(PointerCommand::ToggleFullscreen)
        ));
        assert!(resolve_pointer_input(PointerInput::SingleClick, c).is_none());
        assert!(resolve_pointer_input(PointerInput::Wheel(1.0), c).is_none());
    }
}
