//! Main `IrisWindow` shell and `AppModel` domain coordinator: regional GPUI
//! entity wiring, window/focus lifecycle observers, and side-effect-free `Render`.

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
mod input_host;
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
pub(crate) mod strings;
mod titlebar;
mod vfs_ctrl;
mod viewport;
mod viewport_cards;
mod views;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{
    App, AppContext as _, Context, CursorStyle, Entity, ExternalPaths, FocusHandle,
    InteractiveElement, IntoElement, KeyContext, ParentElement, Point, Render, Styled,
    Subscription, Task, Window, div, prelude::FluentBuilder, px,
};
use iris_core::catalog::PlayQueue;
use iris_core::input::{self, PointerCommand, PointerInput};
use iris_core::keymap::Keymap;
use iris_core::library::Kind;
use iris_core::player::{Command, MAX_SPEED, MIN_SPEED, Session};
use iris_core::preset::{PresetManager, PresetRuntime};
use iris_core::settings::{Settings, Side};
use iris_core::store::Store;
use iris_core::vfs::LocationStore;
use iris_plugin_host::ui::PluginManager;

pub(crate) use actions::{
    CycleAspect, CyclePreset, NextChapter, OpenCommandPalette, OpenFile, OpenNextMedia,
    OpenPreviousMedia, OpenSettings, OpenStreamUrl, PrevChapter, Quit, RefreshDirectory,
    ResetAppearance, TakeScreenshot, ToggleAlwaysOnTop, ToggleDirectory, ToggleFullscreen,
    ToggleInfo, ToggleLoop, ToggleMute, TogglePause, ToggleSearch, rebind_keys,
};

use chrome::WindowChromeState;
use kit::text_field::TextInputHub;
use state::{CatalogState, OverlayState, PopoverKind};
use viewport::{Viewport, should_recover_after_gpu_reset};
use views::{
    DirtyRegions, DockView, PluginPanel, SettingsPanel, Sidebar, ViewHandles, ViewportView,
    model_listener,
};

use crate::platform::{self, MediaKey, MediaKeyBridge, PinchMonitor, SleepPreventer};
use crate::theme::{FONT_UI, ThemeTokens};

pub(in crate::ui) struct AppModel {
    focus: FocusHandle,
    viewport: Viewport,
    catalog: CatalogState,
    queue: PlayQueue,
    overlays: OverlayState,
    text_inputs: TextInputHub,
    chrome: WindowChromeState,
    settings: Settings,
    presets: PresetManager,
    preset_rt: PresetRuntime,
    keymap: Keymap,
    plugin_host: iris_plugin_host::PluginHost,
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
    views: ViewHandles,
    _subscriptions: Vec<Subscription>,
    _events: Task<()>,
    _plugin_events: Task<()>,
}

impl AppModel {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session: Session,
        plugin_host: iris_plugin_host::PluginHost,
        settings: Settings,
        presets: PresetManager,
        preset_rt: PresetRuntime,
        init_plugin_cmds: Vec<(String, String)>,
        keymap: Keymap,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (video, video_reset_rx) = match crate::video::attach(window, &session) {
            Ok((v, rx)) => (Some(v), Some(rx)),
            Err(e) => {
                log::error!("video surface failed: {e:#}");
                cx.quit();
                (None, None)
            }
        };
        if let Some(mut reset_rx) = video_reset_rx {
            cx.spawn(async move |this, cx| {
                while let Some(reset) = reset_rx.next().await {
                    let Ok((is_photo, load_gen)) = this.update(cx, |this, _| {
                        let vp = this.active();
                        (vp.session.kind() == Some(Kind::Photo), vp.load_generation)
                    }) else {
                        break;
                    };
                    if !is_photo {
                        cx.background_executor()
                            .timer(Duration::from_millis(120))
                            .await;
                    }
                    if this
                        .update(cx, |this, cx| {
                            let vp = this.active();
                            if !should_recover_after_gpu_reset(
                                load_gen,
                                vp.load_generation,
                                vp.session.kind(),
                                vp.session.state().loaded,
                                reset.restored_vid,
                            ) {
                                return;
                            }
                            let (zoom, pan) = (vp.image.zoom, vp.image.pan);
                            let settings = this.settings.clone();
                            this.active_mut()
                                .session
                                .recover_unloaded_after_gpu_reset(&settings, zoom, pan);
                            this.notify_viewport_and_dock(cx);
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

        let subscriptions = vec![
            cx.on_app_quit(|this, _| {
                this.shutdown();
                async {}
            }),
            cx.observe_window_bounds(window, |this, window, cx| {
                this.sync_window_and_viewport(window, cx);
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                this.sync_window_and_viewport(window, cx);
            }),
            cx.on_focus_out(&text_inputs.form_focus, window, |this, _, window, cx| {
                this.reconcile_input_focus(window, cx);
            }),
            cx.on_focus_out(
                &text_inputs.recorder_focus,
                window,
                |this, _, window, cx| {
                    this.reconcile_input_focus(window, cx);
                },
            ),
            cx.on_focus_out(&text_inputs.search_focus, window, |this, _, window, cx| {
                this.reconcile_input_focus(window, cx);
            }),
            cx.on_focus_out(&text_inputs.palette_focus, window, |this, _, window, cx| {
                this.reconcile_input_focus(window, cx);
            }),
        ];

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
            plugin_host.open_manager(&settings.disabled_plugins, &settings.theme_id);
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

        Self {
            focus,
            viewport: Viewport::new(session, video, now),
            catalog,
            queue: PlayQueue::new(),
            overlays: OverlayState::default(),
            text_inputs,
            chrome: WindowChromeState::new(now, initial_scale),
            settings,
            presets,
            preset_rt,
            keymap,
            plugin_host,
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
            views: ViewHandles::default(),
            _subscriptions: subscriptions,
            _events: events,
            _plugin_events: plugin_events,
        }
    }

    fn finish_startup(
        &mut self,
        initial: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.plugin_view.modal.focus_pending = self.plugins.active_modal().is_some();
        self.sync_plugin_modal_focus(window);
        self.sync_theme_tokens(cx);
        self.rebuild_directory_rows();
        self.viewport.session.apply_settings(&self.settings);
        if self.settings.audio_device != "auto" && !self.settings.audio_device.is_empty() {
            self.command(
                Command::SetAudioDevice(self.settings.audio_device.clone()),
                cx,
            );
        }
        if let Some(v) = self.settings.volume {
            self.command(Command::SetVolume(v), cx);
        }
        if (self.settings.remember_speed || self.settings.active_preset != "default")
            && let Some(v) = self.settings.speed
        {
            self.command(Command::SetSpeed(v), cx);
        }
        self.open_paths(initial, cx);
        self.sync_view_timer(cx);
        self.sync_window_and_viewport(window, cx);
    }

    #[inline]
    pub(in crate::ui) fn active(&self) -> &Viewport {
        &self.viewport
    }

    #[inline]
    pub(in crate::ui) fn active_mut(&mut self) -> &mut Viewport {
        &mut self.viewport
    }

    #[inline]
    pub(in crate::ui) fn notify_regions(&self, regions: DirtyRegions, cx: &mut App) {
        self.views.notify(regions, cx);
    }

    #[inline]
    pub(in crate::ui) fn notify_all(&self, cx: &mut App) {
        self.notify_regions(DirtyRegions::ALL, cx);
    }

    #[inline]
    pub(in crate::ui) fn notify_root(&self, cx: &mut App) {
        self.notify_regions(DirtyRegions::root_only(), cx);
    }

    #[inline]
    pub(in crate::ui) fn notify_sidebar(&self, cx: &mut App) {
        self.notify_regions(DirtyRegions::sidebar_only(), cx);
    }

    #[inline]
    pub(in crate::ui) fn notify_plugin_panel(&self, cx: &mut App) {
        self.notify_regions(DirtyRegions::plugin_panel_only(), cx);
    }

    #[inline]
    pub(in crate::ui) fn notify_settings(&self, cx: &mut App) {
        self.notify_regions(DirtyRegions::settings_only(), cx);
    }

    #[inline]
    pub(in crate::ui) fn notify_viewport(&self, cx: &mut App) {
        self.notify_regions(DirtyRegions::viewport_only(), cx);
    }

    #[inline]
    pub(in crate::ui) fn notify_dock(&self, cx: &mut App) {
        self.notify_regions(DirtyRegions::dock_only(), cx);
    }

    #[inline]
    pub(in crate::ui) fn notify_viewport_and_dock(&self, cx: &mut App) {
        self.notify_regions(
            DirtyRegions {
                viewport: true,
                dock: true,
                ..DirtyRegions::NONE
            },
            cx,
        );
    }

    pub(in crate::ui) fn sync_viewport_effects(&mut self, cx: &mut Context<Self>) {
        if self.active().pending_photo_actual
            && self.active().session.kind() == Some(Kind::Photo)
            && self.image_geometry_ready()
        {
            self.active_mut().pending_photo_actual = false;
            let (viewport, media) = self.image_geometry();
            let scale = self.chrome.last_scale;
            self.active_mut()
                .image
                .toggle_actual_size(viewport, media, scale);
            let (zoom, pan) = (self.active().image.zoom, self.active().image.pan);
            let _ = self.active_mut().session.transform(zoom, pan);
            self.notify_viewport_and_dock(cx);
        }
        let t = ThemeTokens::of(cx);
        let floating_dock = self.effective_floating_dock();
        let inset = if (self.chrome.fullscreen || floating_dock) && self.chrome.chrome_visible {
            (f32::from(t.dock_h) + 8.) as f64
        } else {
            0.
        };
        let rect_h = self.active().rect.get().map(|r| r.0[3]).unwrap_or(0.);
        let height = if self.chrome.fullscreen {
            self.chrome.last_viewport_height.max(rect_h)
        } else {
            rect_h
        };
        self.active_mut().session.subtitle_inset(inset, height);
    }

    pub(in crate::ui) fn sync_window_and_viewport(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_fullscreen(window, cx);
        if self.plugin_view.modal.focus_pending {
            self.sync_plugin_modal_focus(window);
        }
        let sz = window.viewport_size();
        let (w, h) = (f32::from(sz.width), f32::from(sz.height));
        self.chrome.last_viewport_height = h as f64;
        if !self.chrome.fullscreen && !window.is_maximized() && w >= 480. && h >= 300. {
            self.chrome.last_window_size = Some((w, h));
        }
        let scale = window.scale_factor() as f64;
        self.chrome.last_scale = scale;
        if let Some(v) = self.active().video.borrow_mut().as_mut() {
            v.set_scale(scale);
        }
        self.reconcile_input_focus(window, cx);
        self.sync_viewport_effects(cx);
    }

    pub(super) fn command(&mut self, command: Command, cx: &mut Context<Self>) {
        if let Err(e) = self.active_mut().session.execute(command) {
            log::warn!("player command failed: {e:#}");
            self.show_error_toast(strings::errors::player_action_error(&e), cx);
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
        self.plugin_host.shutdown();
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

impl Drop for AppModel {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) struct IrisWindow {
    model: Entity<AppModel>,
    viewport_view: Entity<ViewportView>,
    dock_view: Entity<DockView>,
    sidebar_view: Entity<Sidebar>,
    settings_view: Entity<SettingsPanel>,
}

impl IrisWindow {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session: Session,
        plugin_host: iris_plugin_host::PluginHost,
        settings: Settings,
        presets: PresetManager,
        preset_rt: PresetRuntime,
        init_plugin_cmds: Vec<(String, String)>,
        keymap: Keymap,
        initial: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let model = cx.new(|cx| {
            AppModel::new(
                session,
                plugin_host,
                settings,
                presets,
                preset_rt,
                init_plugin_cmds,
                keymap,
                window,
                cx,
            )
        });
        let plugin_panel = cx.new(|_| PluginPanel {
            model: model.clone(),
        });
        let sidebar_view = cx.new(|_| Sidebar {
            model: model.clone(),
            plugin_panel: plugin_panel.clone(),
            floating: false,
        });
        let floating_sidebar = cx.new(|_| Sidebar {
            model: model.clone(),
            plugin_panel: plugin_panel.clone(),
            floating: true,
        });
        let dock_view = cx.new(|_| DockView {
            model: model.clone(),
            floating: false,
        });
        let floating_dock = cx.new(|_| DockView {
            model: model.clone(),
            floating: true,
        });
        let viewport_view = cx.new(|_| ViewportView {
            model: model.clone(),
            floating_sidebar: floating_sidebar.clone(),
            floating_dock: floating_dock.clone(),
        });
        let settings_view = cx.new(|_| SettingsPanel {
            model: model.clone(),
        });
        let handles = ViewHandles {
            root: Some(cx.weak_entity()),
            docked_sidebar: Some(sidebar_view.downgrade()),
            floating_sidebar: Some(floating_sidebar.downgrade()),
            plugin_panel: Some(plugin_panel.downgrade()),
            settings: Some(settings_view.downgrade()),
            viewport: Some(viewport_view.downgrade()),
            docked_dock: Some(dock_view.downgrade()),
            floating_dock: Some(floating_dock.downgrade()),
        };
        model.update(cx, |m, cx| {
            m.views = handles;
            m.finish_startup(initial, window, cx);
        });
        Self {
            model,
            viewport_view,
            dock_view,
            sidebar_view,
            settings_view,
        }
    }

    pub(crate) fn open_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.open_paths(paths, cx));
    }
}

impl Render for IrisWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let m = self.model.read(cx);
        m.chrome
            .render_count
            .set(m.chrome.render_count.get().saturating_add(1));

        let t = ThemeTokens::of(cx);
        let fullscreen = m.chrome.fullscreen;
        let hide_tb = m.effective_hide_titlebar();
        let floating_dock = m.effective_floating_dock();
        let recorder_focused = m.text_inputs.recorder_focus.is_focused(window);
        let recording = m.overlays.is_recording(recorder_focused);

        let mut root_ctx = KeyContext::default();
        if recording {
            root_ctx.add("ShortcutRecorder");
            root_ctx.add("TextInput");
        } else {
            root_ctx.add("Workspace");
            if m.has_blocking_overlay() {
                root_ctx.add("Modal");
            }
            if m.text_inputs.is_any_focused(window) {
                root_ctx.add("TextInput");
            }
        }

        let show_overlay_tb = !fullscreen
            && hide_tb
            && !m.has_active_modal()
            && m.overlays.context_menu.is_none()
            && (m.chrome.titlebar_hover || m.overlays.popover == Some(PopoverKind::Presets));

        let root = div()
            .id("root")
            .relative()
            .key_context(root_ctx)
            .on_key_down(model_listener(
                &self.model,
                |this, ev: &gpui::KeyDownEvent, window, cx| {
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
                },
            ))
            .on_any_mouse_down(model_listener(
                &self.model,
                |this, ev: &gpui::MouseDownEvent, window, cx| {
                    if this.overlays.recording_shortcut.is_some()
                        || ev.button != gpui::MouseButton::Left
                        || this.has_active_modal()
                    {
                        return;
                    }
                    if let Some(prev) = this.text_inputs.active_form_key.take() {
                        if prev.starts_with("sb:") {
                            this.notify_plugin_panel(cx);
                        } else {
                            this.notify_root(cx);
                        }
                    }
                    this.focus.focus(window);
                },
            ))
            .size_full()
            .flex()
            .flex_col()
            .font_family(FONT_UI)
            .text_color(t.text);

        let resizing = m.chrome.resizing_dir.is_some();
        let hide_cursor = !m.chrome_shown() && !m.has_host_consent();
        let dir_open = m.settings.directory_open;
        let dir_side = m.settings.directory_side;
        let popover = m.overlays.popover;
        let context_menu = m.overlays.context_menu.clone();
        let settings_open = m.overlays.settings_open;
        let palette_open = m.overlays.palette_open;

        actions::bind_window_actions(root, &self.model)
            .on_drop(model_listener(
                &self.model,
                |this, paths: &ExternalPaths, _, cx| {
                    this.open_paths(paths.paths().to_vec(), cx);
                },
            ))
            .when(resizing, |d| d.cursor(CursorStyle::ResizeLeftRight))
            .when(hide_cursor, |d| d.cursor(CursorStyle::None))
            .when(!fullscreen && !hide_tb, |d| {
                d.child(titlebar::render_titlebar(&self.model, false, cx))
            })
            .child({
                let sidebar =
                    (!fullscreen && dir_open).then(|| self.sidebar_view.clone().into_any_element());
                let player = div()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.viewport_view.clone())
                    .when(!fullscreen && !floating_dock, |d| {
                        d.child(self.dock_view.clone())
                    })
                    .into_any_element();
                let row = div().flex_1().min_h(px(0.)).flex();
                if dir_side == Side::Left {
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
                        .child(titlebar::render_titlebar(&self.model, true, cx)),
                )
            })
            // Layer 2: anchored overlays (Popover & ContextMenu) on #root
            .when_some(popover, |d, pop| {
                d.child(popover::render_popover(
                    &self.model,
                    pop,
                    fullscreen,
                    window,
                    cx,
                ))
            })
            .when_some(context_menu, |d, menu| {
                d.child(context_menu::render_context_menu(
                    &self.model,
                    &menu,
                    window,
                    cx,
                ))
            })
            // Layer 3: modal shells (`SettingsPanel` is an isolated GPUI entity)
            .when(settings_open, |d| d.child(self.settings_view.clone()))
            .when(palette_open, |d| {
                d.child(palette::render_command_palette(&self.model, window, cx))
            })
            .children(dialogs::render_builtin_dialog(&self.model, window, cx))
            .children(plugin_ui::render_plugin_modal(&self.model, window, cx))
            .children(plugin_runtime::render_host_consent(&self.model, window, cx))
            // Layer 4: feedback (Toast / OSD / loading pill) always on top
            .children(chrome::render_top_pill(&self.model, fullscreen, t, cx))
    }
}

#[cfg(test)]
mod contract_tests;
