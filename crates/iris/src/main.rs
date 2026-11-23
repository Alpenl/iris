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
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use futures::StreamExt;
use gpui::{
    App, AppContext as _, Application, Bounds, Menu, MenuItem, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, px, size,
};
use iris_core::{
    keymap::Keymap,
    logging,
    media_uri::MediaUri,
    player::Session,
    preset::{DEFAULT_WINDOW_SIZE, PresetManager, PresetRuntime},
    settings::Settings,
};

const HELP: &str = "Iris 照片 / 视频播放器\n用法：iris [--] [本地文件或文件夹…]\n  --version  显示版本\n  --help     显示本说明";

enum Invocation {
    Open(Vec<PathBuf>),
    Help,
    Version,
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, String> {
    let mut args = args.into_iter().peekable();
    if let Some("--help" | "--version") = args.peek().and_then(|arg| arg.to_str()) {
        let help = args.next().unwrap() == "--help";
        return if args.peek().is_some() {
            Err("--help 和 --version 必须单独使用".into())
        } else if help {
            Ok(Invocation::Help)
        } else {
            Ok(Invocation::Version)
        };
    }

    let mut literal = false;
    let mut paths = Vec::new();
    for arg in args {
        if !literal && arg == "--" {
            literal = true;
            continue;
        }
        if !literal && arg.as_encoded_bytes().starts_with(b"-") {
            return Err(format!("未知选项 {:?}，用 --help 查看用法", arg));
        }
        let file_url = arg.to_str().filter(|value| {
            !literal
                && value
                    .as_bytes()
                    .get(..7)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"file://"))
        });
        let path = if let Some(value) = file_url {
            MediaUri::local_file_uri_path(value).ok_or("无效的本地文件 URL")?
        } else {
            PathBuf::from(arg)
        };
        paths.push(path);
    }
    Ok(Invocation::Open(paths))
}

fn main() -> ExitCode {
    let initial = match parse_args(std::env::args_os().skip(1)) {
        Ok(Invocation::Open(paths)) => paths,
        Ok(Invocation::Help) => {
            attach_parent_console(false);
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Ok(Invocation::Version) => {
            attach_parent_console(false);
            println!("Iris {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            attach_parent_console(true);
            eprintln!("Iris: {message}");
            return ExitCode::FAILURE;
        }
    };

    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::LibraryLoader::{
            LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, SetDefaultDllDirectories,
        };
        // SAFETY: The flag uses the application, System32 and registered user directories.
        if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) } == 0 {
            let error = std::io::Error::last_os_error();
            attach_parent_console(true);
            eprintln!("Iris: initializing DLL search: {error}");
            return ExitCode::FAILURE;
        }
    }
    run(initial)
}

#[cfg(target_os = "windows")]
fn attach_parent_console(stderr: bool) {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    // SAFETY: This only queries the current process and attaches to its parent's console.
    unsafe {
        let handle = GetStdHandle(if stderr {
            STD_ERROR_HANDLE
        } else {
            STD_OUTPUT_HANDLE
        });
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn attach_parent_console(_: bool) {}

fn run(initial: Vec<PathBuf>) -> ExitCode {
    // This variable makes GPUI draw into an opaque swap chain, which hides
    // the video layer underneath (docs/ARCHITECTURE.md).
    #[cfg(target_os = "windows")]
    // SAFETY: first thing in main, before any other thread exists.
    unsafe {
        std::env::remove_var("GPUI_DISABLE_DIRECT_COMPOSITION")
    };
    #[cfg(target_os = "windows")]
    let (_instance, mut instance_rx) = {
        let (tx, rx) = futures::channel::mpsc::channel::<instance::OpenRequest>(0);
        match instance::claim(&initial, tx) {
            instance::Claim::Primary(guard) => (guard, rx),
            instance::Claim::Forwarded => return ExitCode::SUCCESS,
            instance::Claim::Rejected => {
                attach_parent_console(true);
                eprintln!("Iris: 无法将打开请求交给正在运行的实例");
                return ExitCode::FAILURE;
            }
        }
    };
    logging::init();
    // Native file URLs delivered by the application backend (e.g. Finder).
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<Vec<PathBuf>>();
    let app = Application::new().with_assets(assets::Assets);
    app.on_open_urls(move |urls| {
        let paths: Vec<_> = urls
            .iter()
            .filter_map(|value| MediaUri::local_file_uri_path(value))
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
                    .unwrap_or(DEFAULT_WINDOW_SIZE.0)
                    .clamp(480., 7680.),
                active_settings
                    .window_height
                    .unwrap_or(DEFAULT_WINDOW_SIZE.1)
                    .clamp(300., 4320.),
            )
        } else {
            DEFAULT_WINDOW_SIZE
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
        #[cfg(target_os = "windows")]
        cx.spawn(async move |cx| {
            while let Some(request) = instance_rx.next().await {
                let opened = window.update(cx, |this, window, cx| {
                    if request.ack.send(()).is_ok() {
                        window.activate_window();
                        this.open_paths(request.paths, cx);
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
    ExitCode::SUCCESS
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
    fn cli_validates_all_options_and_preserves_literal_paths() {
        let parse = |args: &[&str]| parse_args(args.iter().copied().map(OsString::from));
        assert!(matches!(parse(&["--help"]), Ok(Invocation::Help)));
        assert!(parse(&["--version", "clip.mp4"]).is_err());
        assert!(parse(&["clip.mp4", "--typo"]).is_err());
        assert!(parse(&["file://example.com/share/clip.mp4"]).is_err());
        let path = std::env::temp_dir().join("字幕 #1.srt");
        let url = url::Url::from_file_path(&path).unwrap();
        let Ok(Invocation::Open(paths)) = parse(&[url.as_str(), "--", "-clip.mp4"]) else {
            panic!("expected media paths");
        };
        assert_eq!(paths, [path, PathBuf::from("-clip.mp4")]);
        assert!(parse(&[&format!("{url}?option=x")]).is_err());
        assert!(parse(&[&format!("{url}#fragment")]).is_err());
        assert!(MediaUri::local_file_uri_path("https://example.com/video.mp4").is_none());
    }
}
