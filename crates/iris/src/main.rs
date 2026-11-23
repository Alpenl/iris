//! Iris — mpv-based photo/video player.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod assets;
mod debug;
mod instance;
mod platform;
mod theme;
mod ui;
mod video;

use std::borrow::Cow;
use std::path::PathBuf;

use futures::StreamExt;
use gpui::{
    App, AppContext as _, Application, Bounds, Menu, MenuItem, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, px, size,
};
use iris_core::{
    keymap::Keymap,
    logging,
    player::Session,
    preset::{PresetManager, PresetRuntime},
    settings::Settings,
};

fn main() {
    match std::env::args_os()
        .nth(1)
        .as_deref()
        .and_then(|arg| arg.to_str())
    {
        Some("--version") => {
            println!("Iris {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help") => {
            println!(
                "Iris 照片 / 视频播放器\n用法：iris [本地文件或文件夹…]\n  --version  显示版本\n  --help     显示本说明"
            );
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
    let initial: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    // Paths from the OS (file associations, Finder, a second Windows process).
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<Vec<PathBuf>>();
    #[cfg(target_os = "windows")]
    let _instance = match instance::claim(&initial, open_tx.clone()) {
        instance::Claim::Primary(guard) => guard,
        instance::Claim::Forwarded => return,
    };
    let app = Application::new().with_assets(assets::Assets);
    app.on_open_urls(move |urls| {
        let paths: Vec<_> = urls
            .iter()
            .filter_map(|value| local_file_url(value))
            .collect();
        if !paths.is_empty() {
            let _ = open_tx.unbounded_send(paths);
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
        let initial_keymap = Keymap::load();
        ui::rebind_keys(cx, &initial_keymap);
        cx.set_menus(app_menus());
        cx.on_action(|_: &ui::Quit, cx| cx.quit());

        let session = match Session::new() {
            Ok(m) => m,
            Err(e) => {
                log::error!("player initialization: {e:#}");
                cx.quit();
                return;
            }
        };

        let presets = PresetManager::load();
        let loaded_settings = Settings::load();
        let (active_settings, preset_rt, init_plugin_cmds) =
            PresetRuntime::restore_on_startup(&presets, loaded_settings);
        cx.set_global(theme::ThemeTokens::resolve(&active_settings.theme_id, None));

        let use_saved_size =
            active_settings.remember_window_size || active_settings.active_preset != "default";
        let (win_w, win_h) = if use_saved_size {
            (
                active_settings
                    .window_width
                    .unwrap_or(1440.)
                    .clamp(480., 7680.),
                active_settings
                    .window_height
                    .unwrap_or(900.)
                    .clamp(300., 4320.),
            )
        } else {
            (1440., 900.)
        };

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(win_w), px(win_h)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Iris".into()),
                appears_transparent: true,
                traffic_light_position: None,
            }),
            window_min_size: Some(size(px(480.), px(300.))),
            window_background: WindowBackgroundAppearance::Opaque,
            app_id: Some("app.iris.player".into()),
            ..Default::default()
        };
        let window = match cx.open_window(options, move |window, cx| {
            cx.new(|cx| {
                ui::IrisWindow::new(
                    session,
                    active_settings,
                    presets,
                    preset_rt,
                    init_plugin_cmds,
                    initial_keymap,
                    initial,
                    window,
                    cx,
                )
            })
        }) {
            Ok(window) => window,
            Err(e) => {
                log::error!("opening window: {e:#}");
                cx.quit();
                return;
            }
        };
        cx.spawn(async move |cx| {
            while let Some(paths) = open_rx.next().await {
                let opened = window.update(cx, |this, window, cx| {
                    window.activate_window();
                    this.open_paths(paths, cx);
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

fn local_file_url(value: &str) -> Option<PathBuf> {
    let url = url::Url::parse(value).ok()?;
    if url.scheme() != "file" || url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    url.to_file_path().ok()
}

fn app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "Iris".into(),
            items: vec![
                MenuItem::action("设置…", ui::OpenSettings),
                MenuItem::action("切换工作区预设", ui::CyclePreset),
                MenuItem::action("恢复默认外观", ui::ResetAppearance),
                MenuItem::separator(),
                MenuItem::action("退出 Iris", ui::Quit),
            ],
        },
        Menu {
            name: "文件".into(),
            items: vec![
                MenuItem::action("打开本地文件…", ui::OpenFile),
                MenuItem::action("打开网络流 / 远程地址…", ui::OpenStreamUrl),
                MenuItem::separator(),
                MenuItem::action("搜索目录", ui::ToggleSearch),
                MenuItem::action("刷新目录", ui::RefreshDirectory),
                MenuItem::separator(),
                MenuItem::action("截图保存", ui::TakeScreenshot),
            ],
        },
        Menu {
            name: "播放".into(),
            items: vec![
                MenuItem::action("播放 / 暂停", ui::TogglePause),
                MenuItem::action("静音", ui::ToggleMute),
                MenuItem::action("单曲循环", ui::ToggleLoop),
                MenuItem::separator(),
                MenuItem::action("上一章节", ui::PrevChapter),
                MenuItem::action("下一章节", ui::NextChapter),
                MenuItem::separator(),
                MenuItem::action("上一个", ui::OpenPreviousMedia),
                MenuItem::action("下一个", ui::OpenNextMedia),
            ],
        },
        Menu {
            name: "视图".into(),
            items: vec![
                MenuItem::action("命令面板…", ui::OpenCommandPalette),
                MenuItem::action("切换工作区预设", ui::CyclePreset),
                MenuItem::action("恢复默认外观", ui::ResetAppearance),
                MenuItem::separator(),
                MenuItem::action("展开 / 收起侧栏", ui::ToggleDirectory),
                MenuItem::action("媒体详情", ui::ToggleInfo),
                MenuItem::action("窗口置顶", ui::ToggleAlwaysOnTop),
                MenuItem::action("切换画面比例", ui::CycleAspect),
                MenuItem::separator(),
                MenuItem::action("全屏", ui::ToggleFullscreen),
            ],
        },
    ]
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finder_accepts_only_unambiguous_file_urls() {
        let path = std::env::temp_dir().join("字幕 #1.srt");
        let url = url::Url::from_file_path(&path).unwrap();
        assert_eq!(local_file_url(url.as_str()), Some(path));
        assert!(local_file_url(&format!("{url}?option=x")).is_none());
        assert!(local_file_url(&format!("{url}#fragment")).is_none());
        assert!(local_file_url("https://example.com/video.mp4").is_none());
    }
}
