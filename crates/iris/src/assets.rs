//! Icons compiled into the binary and served to GPUI's `svg()` element.
//!
//! The SVGs in `Resources/Icons` are drawn on a 16x16 grid with square
//! corners; GPUI uses only their alpha, tinted with the element's text colour.
use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

macro_rules! icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub(crate) enum Icon {
            $($variant),*
        }

        impl Icon {
            pub(crate) fn path(self) -> &'static str {
                match self {
                    $(Icon::$variant => concat!("icons/", $file, ".svg")),*
                }
            }

            pub(crate) fn from_name(name: &str) -> Option<Self> {
                let normalized = name.trim();
                $(
                    if normalized.eq_ignore_ascii_case($file) {
                        return Some(Icon::$variant);
                    }
                )*
                if normalized.eq_ignore_ascii_case("star_fill") {
                    return Some(Icon::StarFill);
                }
                None
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
    VolumeMute => "volume-mute",
    Video => "video",
    Photo => "photo",
    Swap => "swap",
    Fit => "fit",
    Plus => "plus",
    Minus => "minus",
    Settings => "settings",
    FolderOpen => "folder-open",
    Refresh => "refresh",
    Repeat => "repeat",
    Rotate => "rotate",
    Close => "close",
    Minimize => "minimize",
    Maximize => "maximize",
    Check => "check",
    Pin => "pin",
    Search => "search",
    Sort => "sort",
    Camera => "camera",
    Info => "info",
    More => "more",
    Folder => "folder",
    ArrowUp => "arrow-up",
    Filmstrip => "filmstrip",
    Music => "music",
    FlipH => "flip-h",
    Aspect => "aspect",
    Puzzle => "puzzle",
    Command => "command",
    Globe => "globe",
    Layers => "layers",
    Sliders => "sliders",
    Bookmark => "bookmark",
    Link => "link",
}

pub(crate) struct Assets;

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

#[cfg(test)]
mod tests {
    use gpui::AssetSource;

    use super::{Assets, Icon};

    #[test]
    fn maps_plugin_icon_names_and_loads_embedded_svgs() {
        let expected = [
            ("globe", Icon::Globe),
            ("layers", Icon::Layers),
            ("sliders", Icon::Sliders),
            ("bookmark", Icon::Bookmark),
            ("link", Icon::Link),
            ("puzzle", Icon::Puzzle),
            ("star", Icon::Star),
            ("star-fill", Icon::StarFill),
            ("video", Icon::Video),
            ("photo", Icon::Photo),
            ("music", Icon::Music),
            ("camera", Icon::Camera),
            ("repeat", Icon::Repeat),
            ("aspect", Icon::Aspect),
            ("rotate", Icon::Rotate),
            ("info", Icon::Info),
            ("settings", Icon::Settings),
            ("search", Icon::Search),
            ("folder", Icon::Folder),
            ("pin", Icon::Pin),
            ("play", Icon::Play),
            ("pause", Icon::Pause),
            ("subtitles", Icon::Subtitles),
            ("audio", Icon::Audio),
        ];

        for (name, icon) in expected {
            assert_eq!(Icon::from_name(name), Some(icon), "failed for {name}");
            assert_eq!(Icon::from_name(&name.to_ascii_uppercase()), Some(icon));
            let bytes = Assets
                .load(icon.path())
                .unwrap()
                .expect("embedded icon bytes");
            let svg = std::str::from_utf8(&bytes).expect("valid utf-8 svg").trim();
            assert!(svg.starts_with("<svg ") && svg.ends_with("</svg>"));
        }

        assert_eq!(Icon::from_name("nonexistent-icon"), None);
    }
}
