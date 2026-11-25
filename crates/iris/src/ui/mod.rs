//! Main `IrisWindow` coordinator: domain state blocks, initialization,
//! `hide_titlebar` top-hotzone overlay & 5px drag threshold, and GPUI `Render`.

mod action_playback;
mod action_view;
mod actions;
mod chrome;
mod context_menu;
mod controls;
#[cfg(any(debug_assertions, feature = "devtools"))]
mod debug_cmd;
mod dialogs;
mod events;
mod info;
mod kit;
mod palette;
mod plugin_ctrl;
mod plugin_runtime;
mod plugin_ui;
mod popover;
mod preset_ctrl;
mod settings;
mod sidebar;
mod state;
mod titlebar;
mod vfs_ctrl;
mod viewport;
mod viewport_cards;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    Context, CursorStyle, ExternalPaths, FocusHandle, InteractiveElement, IntoElement, KeyContext,
    ParentElement, Point, Render, Styled, Subscription, Task, Window, div, prelude::FluentBuilder,
    px,
};
use iris_core::catalog::PlayQueue;
use iris_core::input::{self, PointerCommand, PointerInput};
use iris_core::keymap::Keymap;
use iris_core::library::Kind;
use iris_core::player::{Command, MAX_SPEED, MIN_SPEED, PlayerActionError, Session};
use iris_core::plugin_v2::ui::PluginManager;
use iris_core::preset::{PresetManager, PresetRuntime, SidebarMode};
use iris_core::settings::{Settings, Side};
use iris_core::store::Store;
use iris_core::vfs::LocationStore;

pub(crate) use actions::{
    CycleAspect, CyclePreset, NextChapter, OpenCommandPalette, OpenFile, OpenNextMedia,
    OpenPreviousMedia, OpenSettings, OpenStreamUrl, PrevChapter, Quit, RefreshDirectory,
    ResetAppearance, TakeScreenshot, ToggleAlwaysOnTop, ToggleDirectory, ToggleFullscreen,
    ToggleInfo, ToggleLoop, ToggleMute, TogglePause, ToggleSearch, rebind_keys,
};

use chrome::WindowChromeState;
use kit::text_field::TextInputHub;
use state::{CatalogState, OverlayState, PopoverKind};
use viewport::Viewport;

use crate::platform::{self, MediaKey, MediaKeyBridge, PinchMonitor, SleepPreventer};
use crate::theme::{FONT_UI, ThemeTokens};
use crate::video::VideoSurface;

pub(crate) struct IrisWindow {
    focus: FocusHandle,
    viewports: Vec<Viewport>,
    catalog: CatalogState,
    queue: PlayQueue,
    overlays: OverlayState,
    text_inputs: TextInputHub,
    chrome: WindowChromeState,
    settings: Settings,
    presets: PresetManager,
    preset_rt: PresetRuntime,
    keymap: Keymap,
    plugins: PluginManager,
    plugin_view: plugin_runtime::PluginView,
    locations: LocationStore,
    store: Store,
    store_flush: Option<Task<()>>,
    last_plugin_tick: Instant,
    last_save: Instant,
    last_now_playing: Instant,
    shutdown_done: bool,
    sleep_preventer: SleepPreventer,
    _pinch_monitor: Option<PinchMonitor>,
    media_keys: Option<MediaKeyBridge>,
    _quit: Subscription,
    _events: Task<()>,
    _plugin_events: Task<()>,
}

pub(super) fn format_player_error(err: &anyhow::Error) -> &'static str {
    if let Some(action) = err.downcast_ref::<PlayerActionError>() {
        return match action {
            PlayerActionError::NotSeekable => "当前媒体不支持设置 A-B 循环",
            PlayerActionError::InvalidAbRange => "A-B 循环区间无效或超出时长范围",
            PlayerActionError::NoVisualMedia => "LUT 预览仅适用于已加载的视频或图片",
            PlayerActionError::InvalidSubtitlePath => "本地字幕不支持远程或内部协议路径",
            PlayerActionError::SubtitleFileNotFound => "字幕文件不存在或不可读取",
        };
    }
    "当前状态无法执行此操作"
}

impl IrisWindow {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session: Session,
        settings: Settings,
        presets: PresetManager,
        preset_rt: PresetRuntime,
        init_plugin_cmds: Vec<(String, String)>,
        keymap: Keymap,
        initial: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (video, video_reset_rx) = match VideoSurface::attach(window, session.render_handle()) {
            Ok((v, rx)) => (Some(v), Some(rx)),
            Err(e) => {
                log::error!("video surface failed: {e:#}");
                cx.quit();
                (None, None)
            }
        };
        if let Some(mut reset_rx) = video_reset_rx {
            cx.spawn(async move |this, cx| {
                while reset_rx.next().await.is_some() {
                    let is_photo = this
                        .update(cx, |this, _| {
                            this.active().session.kind() == Some(Kind::Photo)
                        })
                        .unwrap_or(false);
                    if !is_photo {
                        cx.background_executor()
                            .timer(Duration::from_millis(120))
                            .await;
                    }
                    if this
                        .update(cx, |this, cx| {
                            let (zoom, pan) = {
                                let vp = this.active();
                                (vp.image.zoom, vp.image.pan)
                            };
                            let settings = this.settings.clone();
                            this.active_mut()
                                .session
                                .recover_unloaded_after_gpu_reset(&settings, zoom, pan);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        platform::hide_traffic_lights(window);
        if settings.always_on_top {
            platform::set_always_on_top(window, true);
        }
        let focus = cx.focus_handle();
        let text_inputs = TextInputHub::new(cx);
        focus.focus(window);

        let mut rx = session.events().expect("new session event receiver");
        let events = cx.spawn(async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(ev) = rx.try_recv() {
                    batch.push(ev);
                    if batch.len() >= 64 {
                        break;
                    }
                }
                if this
                    .update(cx, |this, cx| this.on_events(batch, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let (plugins, mut plugin_wake_rx) =
            PluginManager::load_with_wake(&settings.disabled_plugins, &settings.theme_id);
        let plugin_events = cx.spawn_in(window, async move |this, cx| {
            while plugin_wake_rx.next().await.is_some() {
                if this
                    .update_in(cx, |this, window, cx| this.poll_plugins(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let pinch_monitor = PinchMonitor::attach(window).map(|(monitor, mut pinch_rx)| {
            cx.spawn(async move |this, cx| {
                while let Some(ev) = pinch_rx.next().await {
                    let ok = this.update(cx, |this, cx| {
                        let pt = Point::new(px(ev.position[0] as f32), px(ev.position[1] as f32));
                        if let Some(PointerCommand::Zoom(delta)) = input::resolve_pointer_input(
                            PointerInput::Pinch(ev.delta),
                            this.input_context(),
                        ) {
                            this.zoom_at(delta, Some(pt), cx);
                        }
                    });
                    if ok.is_err() {
                        break;
                    }
                }
            })
            .detach();
            monitor
        });

        let media_keys = MediaKeyBridge::attach(window).map(|(bridge, mut mk_rx)| {
            cx.spawn(async move |this, cx| {
                while let Some(key) = mk_rx.next().await {
                    let ok = this.update(cx, |this, cx| {
                        let state = this.active().session.state();
                        match key {
                            MediaKey::Play if state.paused || state.ended => {
                                this.command(Command::TogglePlayback, cx);
                            }
                            MediaKey::Pause if !state.paused && !state.ended => {
                                this.command(Command::TogglePlayback, cx);
                            }
                            MediaKey::Toggle => this.command(Command::TogglePlayback, cx),
                            MediaKey::Next => this.step(1, true, cx),
                            MediaKey::Prev => this.step(-1, true, cx),
                            MediaKey::Seek(seconds) => {
                                this.command(Command::SeekAbsolute(seconds), cx)
                            }
                            _ => {}
                        }
                    });
                    if ok.is_err() {
                        break;
                    }
                }
            })
            .detach();
            bridge
        });

        let quit = cx.on_app_quit(|this, _| {
            this.shutdown();
            async {}
        });

        #[cfg(any(debug_assertions, feature = "devtools"))]
        if let Some(mut control) = crate::debug::control_channel() {
            cx.spawn_in(window, async move |this, cx| {
                while let Some(line) = control.next().await {
                    if let Some(arg) = line.strip_prefix("key ") {
                        let arg = arg.trim().to_string();
                        let _ = cx.update(|window, cx| match gpui::Keystroke::parse(&arg) {
                            Ok(k) => {
                                let handled = window.dispatch_keystroke(k, cx);
                                eprintln!("[iris/control] key {arg} handled={handled}");
                            }
                            Err(e) => eprintln!("[iris/control] bad keystroke {arg}: {e:?}"),
                        });
                        continue;
                    }
                    if let Some((c, x, y)) = debug_cmd::parse_mouse_command(&line) {
                        let send = cx.update(|window, _| {
                            platform::synth_mouse(window, c == "click", x as f64, y as f64)
                        });
                        if let Ok(Some(send)) = send {
                            send();
                            eprintln!("[iris/control] {c} {x} {y}");
                        }
                        continue;
                    }
                    if this
                        .update_in(cx, |this, window, cx| this.debug_command(&line, window, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }

        let now = Instant::now();
        let initial_scale = window.scale_factor() as f64;
        let mut catalog = CatalogState::default();
        if let Some(p) = presets.get(&settings.active_preset)
            && let Some(l) = &p.layout
            && let Some(mode) = l.sidebar_mode
        {
            catalog.sidebar_mode = mode;
        }

        let mut this = Self {
            focus,
            viewports: vec![Viewport::new(session, video, now)],
            catalog,
            queue: PlayQueue::new(),
            overlays: OverlayState::default(),
            text_inputs,
            chrome: WindowChromeState::new(now, initial_scale),
            settings,
            presets,
            preset_rt,
            keymap,
            plugins,
            plugin_view: plugin_runtime::PluginView::new(init_plugin_cmds),
            locations: LocationStore::load(),
            store: Store::load(),
            store_flush: None,
            last_plugin_tick: now,
            last_save: now,
            last_now_playing: now - Duration::from_secs(10),
            shutdown_done: false,
            sleep_preventer: SleepPreventer::new(),
            _pinch_monitor: pinch_monitor,
            media_keys,
            _quit: quit,
            _events: events,
            _plugin_events: plugin_events,
        };

        this.plugin_view.modal.focus_pending = this.plugins.active_modal().is_some();
        this.sync_plugin_modal_focus(window);
        this.sync_theme_tokens(cx);
        this.rebuild_directory_rows();
        this.active().session.apply_settings(&this.settings);
        if this.settings.audio_device != "auto" && !this.settings.audio_device.is_empty() {
            this.command(
                Command::SetAudioDevice(this.settings.audio_device.clone()),
                cx,
            );
        }
        if let Some(v) = this.settings.volume {
            this.command(Command::SetVolume(v), cx);
        }
        if (this.settings.remember_speed || this.settings.active_preset != "default")
            && let Some(v) = this.settings.speed
        {
            this.command(Command::SetSpeed(v), cx);
        }
        this.open_paths(initial, cx);
        this
    }

    #[inline]
    fn active(&self) -> &Viewport {
        &self.viewports[0]
    }

    #[inline]
    fn active_mut(&mut self) -> &mut Viewport {
        &mut self.viewports[0]
    }

    pub(super) fn is_confirming(&self, key: &str) -> bool {
        self.overlays.is_confirming(key, Instant::now())
    }

    /// Two-click inline confirmation guard for destructive actions (3s auto-reset).
    pub(super) fn confirm_destructive(&mut self, key: impl Into<String>, cx: &mut Context<Self>) -> bool {
        let key = key.into();
        let now = Instant::now();
        let ttl = Duration::from_secs(3);
        if self.overlays.confirm_or_arm(key.clone(), now, ttl) {
            cx.notify();
            return true;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ttl).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .overlays
                    .pending_confirm
                    .as_ref()
                    .is_some_and(|(k, until)| k == &key && Instant::now() >= *until)
                {
                    this.overlays.pending_confirm = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
        false
    }

    pub(super) fn command(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(e) = self.active_mut().session.execute(command) {
            log::warn!("player command failed: {e:#}");
            self.show_error_toast(format_player_error(&e), cx);
        }
    }

    fn shutdown(&mut self) {
        if self.shutdown_done {
            return;
        }
        self.shutdown_done = true;
        self.sleep_preventer.set_active(false);
        self.media_keys.take();
        self.plugins.shutdown();
        self.store_flush.take();
        self.save_progress_sync();
        let (vol, spd) = {
            let st = self.active().session.state();
            (
                st.volume.clamp(0., 100.),
                st.speed.clamp(MIN_SPEED, MAX_SPEED),
            )
        };
        self.settings.volume = Some(vol);
        if self.settings.remember_speed {
            self.settings.speed = Some(spd);
        }
        if self.settings.remember_window_size
            && self.chrome.pending_window_size.is_none()
            && let Some((w, h)) = self.chrome.last_window_size
        {
            self.settings.window_width = Some(w);
            self.settings.window_height = Some(h);
        }
        self.persist_user_settings();
        drop(self.active().video.borrow_mut().take());
        self.active().session.shutdown();
    }
}

impl Drop for IrisWindow {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Render for IrisWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.chrome.render_count += 1;
        self.prepare_timed_overlay_frame(window, cx);
        self.sync_fullscreen(window, cx);
        self.sync_plugin_modal_focus(window);
        if !self.text_inputs.form_focus.is_focused(window) {
            self.text_inputs.active_form_key = None;
        }
        self.scan_visible_favorites(cx);
        let modal_visible = self.has_host_consent() || self.plugins.active_modal().is_some();
        let files_visible = self.sidebar_is_visible()
            && self.catalog.sidebar_mode == SidebarMode::Files
            && !modal_visible
            && !self.overlays.has_menu_or_modal();
        let sidebar_forms_visible =
            self.sidebar_is_visible() && self.catalog.sidebar_mode == SidebarMode::Plugins;
        let form_visible = self
            .text_inputs
            .active_form_key
            .as_deref()
            .is_some_and(|key| {
                if key.starts_with("modal:") {
                    modal_visible
                } else if key.starts_with("sb:") {
                    sidebar_forms_visible && !modal_visible && !self.overlays.has_menu_or_modal()
                } else {
                    self.overlays.dialog.is_some() && !modal_visible
                }
            });
        let recorder_focused = self.text_inputs.recorder_focus.is_focused(window);
        let recording = self.overlays.is_recording(recorder_focused);
        if !recording {
            self.overlays.recording_shortcut = None;
        }
        if (!files_visible && self.text_inputs.search_focus.is_focused(window))
            || (!self.overlays.palette_open && self.text_inputs.palette_focus.is_focused(window))
            || (!form_visible && self.text_inputs.form_focus.is_focused(window))
            || (!recording && recorder_focused)
        {
            self.focus.focus(window);
        }
        let t = ThemeTokens::of(cx);
        let fullscreen = self.chrome.fullscreen;
        let hide_tb = self.effective_hide_titlebar();
        let floating_dock = self.effective_floating_dock();

        if !fullscreen && !window.is_maximized() {
            let sz = window.viewport_size();
            let (w, h) = (f32::from(sz.width), f32::from(sz.height));
            if w >= 480. && h >= 300. {
                self.chrome.last_window_size = Some((w, h));
            }
        }
        if self.active().pending_photo_actual
            && self.active().session.kind() == Some(Kind::Photo)
            && self.image_geometry_ready()
        {
            self.active_mut().pending_photo_actual = false;
            let (viewport, media) = self.image_geometry();
            self.active_mut().image.toggle_actual_size(
                viewport,
                media,
                window.scale_factor() as f64,
            );
            let vp = self.active();
            let _ = vp.session.transform(vp.image.zoom, vp.image.pan);
        }

        let scale = window.scale_factor() as f64;
        self.chrome.last_scale = scale;
        if let Some(v) = self.active().video.borrow_mut().as_mut() {
            v.set_scale(scale);
        }

        let inset = if (fullscreen || floating_dock) && self.chrome.chrome_visible {
            (f32::from(t.dock_h) + 8.) as f64
        } else {
            0.
        };
        let height = if fullscreen {
            f32::from(window.viewport_size().height) as f64
        } else {
            self.active().rect.get().map(|r| r.0[3]).unwrap_or(0.)
        };
        self.active_mut().session.subtitle_inset(inset, height);

        let mut root_ctx = KeyContext::default();
        if recording {
            root_ctx.add("ShortcutRecorder");
            root_ctx.add("TextInput");
        } else {
            root_ctx.add("Workspace");
            if self.has_blocking_overlay() {
                root_ctx.add("Modal");
            }
            if self.text_inputs.is_any_focused(window) {
                root_ctx.add("TextInput");
            }
        }

        let show_overlay_tb = !fullscreen
            && hide_tb
            && !self.has_active_modal()
            && self.overlays.context_menu.is_none()
            && (self.chrome.titlebar_hover || self.overlays.popover == Some(PopoverKind::Presets));

        let root = div()
            .id("root")
            .relative()
            .key_context(root_ctx)
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                let mods = ev.keystroke.modifiers;
                if mods.alt
                    || mods.control
                    || mods.platform
                    || mods.shift
                    || this.overlays.recording_shortcut.is_some()
                    || this.text_inputs.is_any_focused(window)
                {
                    return;
                }
                let key = ev.keystroke.key.as_str();
                if this.handle_popover_key(key, cx) {
                    cx.stop_propagation();
                    return;
                }
                if key == "enter" {
                    if this.has_host_consent() {
                        this.approve_host_consent(cx);
                        cx.stop_propagation();
                    } else if this.plugins.active_modal().is_some() {
                        this.submit_active_plugin_modal(window, cx);
                        cx.stop_propagation();
                    } else if this.overlays.dialog.is_some() {
                        this.submit_builtin_dialog(window, cx);
                        cx.stop_propagation();
                    }
                }
            }))
            .on_any_mouse_down(cx.listener(|this, ev: &gpui::MouseDownEvent, window, _| {
                if this.overlays.recording_shortcut.is_some()
                    || ev.button != gpui::MouseButton::Left
                    || this.has_active_modal()
                {
                    return;
                }
                this.text_inputs.active_form_key = None;
                this.focus.focus(window);
            }))
            .size_full()
            .flex()
            .flex_col()
            .font_family(FONT_UI)
            .text_color(t.text);

        self.bind_window_actions(root, cx)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.open_paths(paths.paths().to_vec(), cx);
            }))
            .when(self.chrome.resizing_dir.is_some(), |d| {
                d.cursor(CursorStyle::ResizeLeftRight)
            })
            .when(!self.chrome_shown() && !self.has_host_consent(), |d| {
                d.cursor(CursorStyle::None)
            })
            .when(!fullscreen && !hide_tb, |d| {
                d.child(self.render_titlebar(false, cx))
            })
            .child({
                let sidebar = (!fullscreen && self.settings.directory_open)
                    .then(|| self.render_sidebar(false, window, cx).into_any_element());
                let player = div()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.render_video_area(fullscreen, window, cx))
                    .when(!fullscreen && !floating_dock, |d| {
                        d.child(self.render_dock(false, window, cx))
                    })
                    .into_any_element();
                let row = div().flex_1().min_h(px(0.)).flex();
                if self.settings.directory_side == Side::Left {
                    row.children(sidebar).child(player)
                } else {
                    row.child(player).children(sidebar)
                }
            })
            // Layer 1.5: frameless overlay titlebar (always below anchored overlays & modals)
            .when(show_overlay_tb, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .occlude()
                        .child(self.render_titlebar(true, cx)),
                )
            })
            // Layer 2: anchored overlays (Popover & ContextMenu) on #root
            .when_some(self.overlays.popover, |d, pop| {
                d.child(self.render_popover(pop, fullscreen, window, cx))
            })
            .when_some(self.overlays.context_menu.clone(), |d, menu| {
                d.child(self.render_context_menu(&menu, window, cx))
            })
            // Layer 3: modal shells
            .when(self.overlays.settings_open, |d| {
                d.child(self.render_settings_modal(window, cx))
            })
            .when(self.overlays.palette_open, |d| {
                d.child(self.render_command_palette(window, cx))
            })
            .children(self.render_builtin_dialog(window, cx))
            .children(self.render_plugin_modal(window, cx))
            .children(self.render_host_consent(window, cx))
            // Layer 4: feedback (Toast / OSD / loading pill) always on top
            .children(self.render_top_pill(fullscreen, t, cx))
    }
}

#[cfg(test)]
mod contract_tests;
