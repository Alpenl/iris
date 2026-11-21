//! Iris — mpv-based photo/video player (technical spike).
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod debug;
mod geometry;
mod icc;
mod input;
mod library;
mod logging;
mod photo;
mod platform;
mod player;
mod settings;
mod theme;
mod ui;
mod video;

use std::borrow::Cow;
use std::path::PathBuf;

use gpui::{
    App, AppContext as _, Application, Bounds, TitlebarOptions, WindowBackgroundAppearance,
    WindowBounds, WindowOptions, px, size,
};

fn main() {
    // This variable makes GPUI draw into an opaque swap chain, which hides
    // the video layer underneath (docs/ARCHITECTURE.md).
    #[cfg(target_os = "windows")]
    // SAFETY: first thing in main, before any other thread exists.
    unsafe {
        std::env::remove_var("GPUI_DISABLE_DIRECT_COMPOSITION")
    };
    logging::init();
    let initial = std::env::args_os().nth(1).map(PathBuf::from);
    Application::new().run(move |cx: &mut App| {
        if let Err(e) = cx.text_system().add_fonts(vec![
            Cow::Borrowed(include_bytes!(
                "../../../Resources/Fonts/SpaceGrotesk-VariableFont_wght.ttf"
            )),
            Cow::Borrowed(include_bytes!(
                "../../../Resources/Fonts/IBMPlexMono-Regular.ttf"
            )),
            Cow::Borrowed(include_bytes!(
                "../../../Resources/Fonts/IBMPlexMono-Medium.ttf"
            )),
        ]) {
            log::error!("fonts: {e:#}");
        }
        ui::bind_keys(cx);
        cx.on_action(|_: &ui::Quit, cx| cx.quit());

        let session = match player::Session::new() {
            Ok(m) => m,
            Err(e) => {
                log::error!("player initialization: {e:#}");
                cx.quit();
                return;
            }
        };

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1440.), px(900.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Iris".into()),
                appears_transparent: true,
                traffic_light_position: None,
            }),
            window_min_size: Some(size(px(960.), px(600.))),
            window_background: WindowBackgroundAppearance::Opaque,
            app_id: Some("app.iris.player".into()),
            ..Default::default()
        };
        if let Err(e) = cx.open_window(options, move |window, cx| {
            cx.new(|cx| ui::IrisWindow::new(session, initial, window, cx))
        }) {
            log::error!("opening window: {e:#}");
            cx.quit();
            return;
        }
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
