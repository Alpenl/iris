//! Main `IrisWindow` shell and `WindowShell` GPUI adapter around [`iris_app::AppModel`]:
//! regional GPUI entity wiring, window/focus lifecycle observers, two-phase native shutdown,
//! and side-effect-free `Render`.
//!
//! ## Five-Layer Input Priority Stack
//! 1. **`ShortcutRecorder`** (`KeyContext: ShortcutRecorder + TextInput` when Settings Shortcuts
//!    recorder is open & focused): captures raw chords before workspace bindings (`Esc` cancels
//!    recording; reserved `⌘⌥⇧R` / `Ctrl+Alt+Shift+R` `ResetAppearance` still passes through).
//! 2. **Active `TextInput`** (`KeyContext: Workspace + TextInput` on search, palette, modal, or
//!    sidebar form field): `!TextInput` firewall blocks single-key and text-editing workspace
//!    shortcuts while IME/typing owns the keystroke (`Esc` peels input/modal via `TextFieldHost`).
//! 3. **Modal / Context-Menu / Palette / Overlay `Esc` & `!Modal` Stack** (`KeyContext: Workspace + Modal`
//!    when consent, plugin modal, built-in dialog, settings, palette, context menu, or popover is open):
//!    blocks background workspace chords while `handle_escape` peels exactly one top-most layer in
//!    strict reverse Z-order (`HostConsent > ShortcutRecorder > PluginModal > BuiltinDialog > Palette
//!    > Settings > ContextMenu > Popover > MediaInfo > FullscreenDirPopup > SidebarForm > Search`).
//! 4. **`Workspace` / `Viewport` Keymap** (`KeyContext: Workspace` + `Viewport[media=...]`):
//!    dispatches active scheme (`Modern` / `MpvClassic` / `PotPlayer`) + user overrides + preset/plugin
//!    bindings when unobstructed by layers 1–3.
//! 5. **Fullscreen `Esc` Exit**: reached only after layers 1–3 have zero active overlays or inputs.

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
use iris_app::{
    AppModel, CatalogController, MediaKeyInput, MediaKeyOutcome, PlaybackController,
    PluginController, StartupWorkbench, WindowShutdownCoordinator,
};
use iris_core::input::{self, PointerCommand, PointerInput};
use iris_core::keymap::Keymap;
use iris_core::library::Kind;
use iris_core::player::{Command, Session};
use iris_core::preset::{PresetManager, SidebarMode};
use iris_core::settings::{Settings, Side};

pub(crate) use actions::{
    CycleAspect, CyclePreset, NextChapter, OpenCommandPalette, OpenFile, OpenNextMedia,
    OpenPreviousMedia, OpenSettings, OpenStreamUrl, PrevChapter, Quit, RefreshDirectory,
    ResetAppearance, TakeScreenshot, ToggleAlwaysOnTop, ToggleDirectory, ToggleFullscreen,
    ToggleInfo, ToggleLoop, ToggleMute, TogglePause, ToggleSearch, rebind_keys,
};

use chrome::WindowChromeState;
use kit::text_field::TextInputHub;
use state::{CatalogUiState, OverlayState, PopoverKind};
use viewport::Viewport;
use views::{
    DirtyRegions, DockView, PluginPanel, SettingsPanel, Sidebar, UiVisibility, ViewHandles,
    ViewportView, model_listener,
};

use crate::platform::{self, MediaKey, MediaKeyBridge, PinchMonitor, SleepPreventer};
use crate::theme::{FONT_UI, ThemeTokens};

/// Window-owned GPUI shell pairing headless [`iris_app::AppModel`] (whose internal fields are all
/// non-`pub`) with window-local focus handles, `VideoSurface`, overlays, and async executor tasks.
pub(in crate::ui) struct WindowShell {
    pub(in crate::ui) app: AppModel,
    focus: FocusHandle,
    viewport: Viewport,
    catalog_ui: CatalogUiState,
    overlays: OverlayState,
    text_inputs: TextInputHub,
    chrome: WindowChromeState,
    plugin_ui: plugin_runtime::PluginUiState,
    shutdown: WindowShutdownCoordinator,
    sleep_preventer: SleepPreventer,
    _pinch_monitor: Option<PinchMonitor>,
    media_keys: Option<MediaKeyBridge>,
    views: ViewHandles,
    _subscriptions: Vec<Subscription>,
    _events: Task<()>,
    _plugin_events: Task<()>,
}

impl WindowShell {
    fn new(
        session: Session,
        plugin_host: iris_plugin_host::PluginHost,
        startup: StartupWorkbench,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let now = Instant::now();
        let (mut app, mut rx, mut plugin_wake_rx) =
            AppModel::open_window(session, plugin_host, startup, now)
                .expect("open window app model");
        let attach_slot = app
            .take_video_attach_slot()
            .expect("initial window video attach slot");
        let (video, video_reset_rx) = match crate::video::attach(window, attach_slot) {
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
                        (
                            this.session().kind() == Some(Kind::Photo),
                            this.playback().load_generation(),
                        )
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
                            let (zoom, pan) = (this.active().image.zoom, this.active().image.pan);
                            if this.app.recover_after_gpu_reset(
                                load_gen,
                                reset.restored_vid,
                                zoom,
                                pan,
                            ) {
                                this.notify_viewport_and_dock(cx);
                            }
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
        if app.settings().always_on_top {
            platform::set_always_on_top(window, true);
        }
        let focus = cx.focus_handle();
        let text_inputs = TextInputHub::new(cx);
        focus.focus(window);

        let weak_for_close = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            let _ = weak_for_close.update(cx, |this, _| this.shutdown());
            true
        });

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
                        let input = match key {
                            MediaKey::Play => MediaKeyInput::Play,
                            MediaKey::Pause => MediaKeyInput::Pause,
                            MediaKey::Toggle => MediaKeyInput::Toggle,
                            MediaKey::Next => MediaKeyInput::Next,
                            MediaKey::Prev => MediaKeyInput::Prev,
                            MediaKey::Seek(seconds) => MediaKeyInput::Seek(seconds),
                        };
                        match this.app.handle_media_key(input) {
                            MediaKeyOutcome::Ignored | MediaKeyOutcome::Executed => {}
                            MediaKeyOutcome::StepQueue { delta } => this.step(delta, true, cx),
                            MediaKeyOutcome::Failed(e) => {
                                log::warn!("media key command failed: {e:#}");
                                this.show_error_toast(strings::errors::player_action_error(&e), cx);
                            }
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

        let initial_scale = window.scale_factor() as f64;

        Self {
            app,
            focus,
            viewport: Viewport::new(video),
            catalog_ui: CatalogUiState::default(),
            overlays: OverlayState::default(),
            text_inputs,
            chrome: WindowChromeState::new(now, initial_scale),
            plugin_ui: plugin_runtime::PluginUiState::default(),
            shutdown: WindowShutdownCoordinator::default(),
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
        if self.plugins().active_modal().is_some() {
            self.app.mark_plugin_modal_focus_pending();
        }
        self.sync_plugin_modal_focus(window);
        self.sync_theme_tokens(cx);
        self.rebuild_directory_rows();
        self.open_paths(initial, cx);
        self.sync_view_timer(cx);
        self.sync_window_and_viewport(window, cx);
    }

    #[inline]
    pub(in crate::ui) fn session(&self) -> &Session {
        self.app.session()
    }

    #[inline]
    pub(in crate::ui) fn playback(&self) -> &PlaybackController {
        self.app.playback()
    }

    #[inline]
    pub(in crate::ui) fn catalog(&self) -> &CatalogController {
        self.app.catalog()
    }

    #[inline]
    pub(in crate::ui) fn plugins(&self) -> &PluginController {
        self.app.plugins()
    }

    #[inline]
    pub(in crate::ui) fn settings(&self) -> &Settings {
        self.app.settings()
    }

    #[inline]
    pub(in crate::ui) fn presets(&self) -> &PresetManager {
        self.app.presets()
    }

    #[inline]
    pub(in crate::ui) fn keymap(&self) -> &Keymap {
        self.app.keymap()
    }

    #[inline]
    pub(in crate::ui) fn is_preset_modified(&self) -> bool {
        self.app.is_preset_modified()
    }

    #[inline]
    pub(in crate::ui) fn active(&self) -> &Viewport {
        &self.viewport
    }

    #[inline]
    pub(in crate::ui) fn active_mut(&mut self) -> &mut Viewport {
        &mut self.viewport
    }

    pub(in crate::ui) fn ui_visibility(&self) -> UiVisibility {
        UiVisibility {
            dock_visible: self.chrome_shown(),
            sidebar_plugins_visible: self.sidebar_is_visible()
                && self.catalog().sidebar_mode() == SidebarMode::Plugins,
            settings_open: self.overlays.settings_open,
            palette_open: self.overlays.palette_open,
            popover_open: self.overlays.popover.is_some(),
        }
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
        if self.session().kind() == Some(Kind::Photo)
            && self.image_geometry_ready()
            && self.app.take_pending_photo_actual()
        {
            let (viewport, media) = self.image_geometry();
            let scale = self.chrome.last_scale;
            self.active_mut()
                .image
                .toggle_actual_size(viewport, media, scale);
            let (zoom, pan) = (self.active().image.zoom, self.active().image.pan);
            let _ = self.app.transform_view(zoom, pan);
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
        self.app.set_subtitle_inset(inset, height);
    }

    pub(in crate::ui) fn sync_window_and_viewport(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_fullscreen(window, cx);
        if self.plugins().modal().focus_pending() {
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
        if let Some(e) = self.app.execute_player_command(command).error {
            log::warn!("player command failed: {e:#}");
            self.show_error_toast(strings::errors::player_action_error(&e), cx);
        }
    }

    /// Single explicit window shutdown coordinator used by window close, frameless close buttons,
    /// `on_app_quit`, and `Drop` fallback. Destroys `VideoSurface` strictly BEFORE `Session::shutdown()`.
    pub(super) fn shutdown(&mut self) {
        if self.shutdown.is_done() {
            return;
        }
        self.catalog_ui.store_flush.take();
        let window_size_to_save = self
            .chrome
            .pending_window_size
            .is_none()
            .then_some(self.chrome.last_window_size)
            .flatten();
        let sleep_preventer = &mut self.sleep_preventer;
        let media_keys = &mut self.media_keys;
        let viewport = &self.viewport;
        let _ = self.app.shutdown_with_coordinator(
            &mut self.shutdown,
            window_size_to_save,
            Instant::now(),
            || sleep_preventer.set_active(false),
            || {
                media_keys.take();
            },
            || drop(viewport.take_video_surface()),
        );
    }
}

impl Drop for WindowShell {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) struct IrisWindow {
    model: Entity<WindowShell>,
    viewport_view: Entity<ViewportView>,
    dock_view: Entity<DockView>,
    sidebar_view: Entity<Sidebar>,
    settings_view: Entity<SettingsPanel>,
}

impl IrisWindow {
    pub(crate) fn new(
        session: Session,
        plugin_host: iris_plugin_host::PluginHost,
        startup: StartupWorkbench,
        initial: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let model = cx.new(|cx| WindowShell::new(session, plugin_host, startup, window, cx));
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

pub(in crate::ui) fn build_root_key_context(
    recording: bool,
    has_blocking_overlay: bool,
    text_input_focused: bool,
) -> KeyContext {
    let mut root_ctx = KeyContext::default();
    if recording {
        root_ctx.add("ShortcutRecorder");
        root_ctx.add("TextInput");
    } else {
        root_ctx.add("Workspace");
        if has_blocking_overlay {
            root_ctx.add("Modal");
        }
        if text_input_focused {
            root_ctx.add("TextInput");
        }
    }
    root_ctx
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

        let root_ctx = build_root_key_context(
            recording,
            m.has_blocking_overlay(),
            m.text_inputs.is_any_focused(window),
        );

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
                        } else if this.plugins().active_modal().is_some() {
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
        let dir_open = m.settings().directory_open;
        let dir_side = m.settings().directory_side;
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
