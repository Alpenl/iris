//! Iris — mpv-based photo/video player (technical spike).
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

// Headless modules live in iris-core; re-exported so `crate::player` etc. resolve.
use iris_core::{geometry, input, library, logging, player, settings, store};

mod assets;
mod debug;
mod instance;
mod platform;
mod theme;
mod ui;
mod video;

use futures::StreamExt;
use std::borrow::Cow;
use std::path::PathBuf;

use gpui::{
    App, AppContext as _, Application, Bounds, TitlebarOptions, WindowBackgroundAppearance,
    WindowBounds, WindowOptions, px, size,
};

fn main() {
    match std::env::args_os().nth(1).as_deref().and_then(|arg| arg.to_str()) {
        Some("--version") => {
            println!("Iris {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help") => {
            println!("Iris 照片 / 视频播放器\n用法：iris [本地文件或文件夹]\n  --version  显示版本\n  --help     显示本说明");
            return;
        }
        _ => {}
    }
    // This variable makes GPUI draw into an opaque swap chain, which hides
    // the video layer underneath (docs/ARCHITECTURE.md).
    #[cfg(target_os = "windows")]
    // SAFETY: first thing in main, before any other thread exists.
    unsafe {
        std::env::remove_var("GPUI_DISABLE_DIRECT_COMPOSITION")
    };
    logging::init();
    let initial = std::env::args_os().nth(1).map(PathBuf::from);
    // Paths from the OS (file associations, Finder, a second Windows process).
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<PathBuf>();
    #[cfg(target_os = "windows")]
    let _instance = match instance::claim(initial.as_deref(), open_tx.clone()) {
        instance::Claim::Primary(guard) => guard,
        instance::Claim::Forwarded => return,
    };
    let app = Application::new().with_assets(assets::Assets);
    app.on_open_urls(move |urls| {
        for value in urls {
            // Local file URLs only. A query, another scheme or any other
            // external string never becomes a backend option or command.
            if let Ok(url) = url::Url::parse(&value)
                && url.scheme() == "file"
                && let Ok(path) = url.to_file_path()
            {
                let _ = open_tx.unbounded_send(path);
            }
        }
    });
    app.run(move |cx: &mut App| {
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
        let window = match cx.open_window(options, move |window, cx| {
            cx.new(|cx| ui::IrisWindow::new(session, initial, window, cx))
        }) {
            Ok(window) => window,
            Err(e) => {
                log::error!("opening window: {e:#}");
                cx.quit();
                return;
            }
        };
        cx.spawn(async move |cx| {
            while let Some(path) = open_rx.next().await {
                let opened = window.update(cx, |this, window, cx| {
                    window.activate_window();
                    if !path.as_os_str().is_empty() {
                        this.open_path(&path, cx);
                    }
                });
                if opened.is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
