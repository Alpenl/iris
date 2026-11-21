//! Icons compiled into the binary and served to GPUI's `svg()` element.
//!
//! The SVGs in `Resources/Icons` are drawn on a 16x16 grid with square
//! corners; GPUI uses only their alpha, tinted with the element's text colour.
use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

macro_rules! icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum Icon {
            $($variant),*
        }

        impl Icon {
            pub fn path(self) -> &'static str {
                match self {
                    $(Icon::$variant => concat!("icons/", $file, ".svg")),*
                }
            }
        }

        const ICONS: &[(&str, &[u8])] = &[
            $((
                concat!("icons/", $file, ".svg"),
                include_bytes!(concat!("../../../Resources/Icons/", $file, ".svg")),
            )),*
        ];
    };
}

icons! {
    Play => "play",
    Pause => "pause",
    Prev => "prev",
    Next => "next",
    Star => "star",
    StarFill => "star-fill",
    Fullscreen => "fullscreen",
    FullscreenExit => "fullscreen-exit",
    PanelLeft => "panel-left",
    PanelRight => "panel-right",
    Subtitles => "subtitles",
    Audio => "audio",
    Volume => "volume",
    Video => "video",
    Photo => "photo",
    Swap => "swap",
    Fit => "fit",
    Plus => "plus",
    Minus => "minus",
}

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::from(*p))
            .collect())
    }
}
