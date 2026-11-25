//! Responsive control bar (`render_dock`): docked at `t.dock_h` in windowed mode
//! or overlaid on the picture, breakpoint calculation, volume bar, and window actions.

mod fields;
mod timeline;

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, Bounds, Context, Div, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Pixels, ScrollWheelEvent, StatefulInteractiveElement, Styled, Window, canvas,
    div, prelude::FluentBuilder, px,
};
use iris_core::library::Kind;
use iris_core::settings::Side;

use super::IrisWindow;
use super::actions::{
    OpenCommandPalette, OpenSettings, ToggleDirectory, ToggleFavorite, ToggleFullscreen,
    ToggleInfo, ToggleMute,
};
use super::kit::{ghost, icon, icon_button, icon_on_ghost, separator, tip};
use crate::assets::Icon;
use crate::theme::{FONT_MONO, ThemeTokens};

fn anchor_wrap(
    slot: Rc<Cell<Option<Bounds<Pixels>>>>,
    child: impl IntoElement,
) -> impl IntoElement {
    div().relative().flex_none().child(child).child(
        canvas(move |b, _, _| slot.set(Some(b)), |_, _, _, _| {})
            .absolute()
            .size_full(),
    )
}

fn dock_sep(compact: bool, t: ThemeTokens) -> Div {
    if compact {
        div().flex_none().w(px(1.)).h(px(14.)).mx(px(3.)).bg(t.line)
    } else {
        separator(t)
    }
}

impl IrisWindow {
    pub(super) fn act_tip(
        &self,
        label: impl Into<gpui::SharedString>,
        action_id: &str,
    ) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView + 'static {
        tip(label, self.key_hint(action_id))
    }

    fn dock_available_width(&self, window: &Window) -> f32 {
        let total = f32::from(window.viewport_size().width);
        if !self.chrome.fullscreen && self.settings.directory_open {
            (total - self.settings.directory_width).max(0.)
        } else {
            total
        }
    }

    pub(super) fn render_dock(
        &self,
        floating: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = ThemeTokens::of(cx);
        let kind = self.active().session.kind();
        let left = self.settings.directory_side == Side::Left;
        let compact = self.effective_compact_controls();
        let avail_w = self.dock_available_width(window);
        let narrow = avail_w < if compact { 560. } else { 640. };
        let tiny = avail_w < if compact { 440. } else { 500. };
        let dir_open = self.sidebar_is_visible();
        let mut dir = Some(
            icon_button(
                ("dir", usize::from(dir_open)),
                if left {
                    Icon::PanelLeft
                } else {
                    Icon::PanelRight
                },
                dir_open,
                t,
            )
            .tooltip(tip(
                if dir_open {
                    "收起侧栏"
                } else {
                    "展开侧栏"
                },
                self.key_hint("toggle_directory"),
            ))
            .on_click(cx.listener(|this, _, w, cx| this.toggle_directory(&ToggleDirectory, w, cx))),
        );
        let (start, end): (Option<AnyElement>, Option<AnyElement>) = match kind {
            Some(Kind::Video) => (
                Some(
                    self.video_transport(tiny, compact, t, cx)
                        .into_any_element(),
                ),
                Some(
                    self.video_fields(narrow, tiny, compact, t, cx)
                        .into_any_element(),
                ),
            ),
            Some(Kind::Audio) => (
                Some(
                    self.video_transport(tiny, compact, t, cx)
                        .into_any_element(),
                ),
                Some(self.audio_fields(narrow, compact, t, cx).into_any_element()),
            ),
            Some(Kind::Photo) => (
                Some(self.photo_transport(compact, t, cx).into_any_element()),
                Some(
                    self.photo_fields(narrow, compact, t, window, cx)
                        .into_any_element(),
                ),
            ),
            None => (None, None),
        };
        let layout = kind.map_or(0, |k| k as usize + 1) * 8
            + usize::from(left) * 4
            + usize::from(self.settings.directory_open && !floating) * 2
            + usize::from(compact);
        let mut row = div()
            .id(("dock-row", layout))
            .size_full()
            .flex()
            .items_center()
            .px(px(if compact { 5. } else { 8. }));
        if left {
            row = row
                .children(dir.take())
                .when(start.is_some(), |d| d.child(dock_sep(compact, t)));
        }
        let plugin_status = (!tiny)
            .then(|| {
                self.render_plugin_status_slot(cx)
                    .map(IntoElement::into_any_element)
            })
            .flatten();
        let plugin_toolbar = (!tiny)
            .then(|| {
                self.render_plugin_toolbar_slot(compact, t, cx)
                    .map(IntoElement::into_any_element)
            })
            .flatten();
        let has_plugin_tb = plugin_toolbar.is_some();
        let has_end = end.is_some();
        row = row
            .children(start)
            .child(div().flex_1())
            .children(plugin_status)
            .children(plugin_toolbar)
            .when(has_plugin_tb && !has_end, |d| d.child(dock_sep(compact, t)))
            .children(end)
            .child(self.window_buttons(tiny, compact, t, cx));
        if let Some(dir) = dir {
            row = row.child(dock_sep(compact, t)).child(dir);
        }

        let timeline =
            matches!(kind, Some(Kind::Video | Kind::Audio)).then(|| self.render_timeline(t, cx));
        let seam = timeline.is_some();
        div()
            .relative()
            .when(floating, |d| d.size_full())
            .when(!floating, |d| {
                d.h(t.dock_h)
                    .w_full()
                    .flex_none()
                    .bg(t.window_bg)
                    .when(!seam, |d| d.border_t_1().border_color(t.line))
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(row)
            .children(timeline)
    }

    fn render_volume(
        &self,
        narrow: bool,
        compact: bool,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.active().session.state();
        let volume = state.volume;
        let muted = state.muted;
        let filled = if muted {
            0
        } else {
            (volume / 10.).round().clamp(0., 10.) as usize
        };
        let vol_slot = self.chrome.probes.vol_bounds.clone();
        div()
            .id("volume")
            .group("volume")
            .h(t.control_h_md)
            .flex()
            .items_center()
            .gap(px(if compact { 2. } else { 4. }))
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                let delta = this.damped_volume_delta(ev);
                if delta.abs() >= 0.5 {
                    this.adjust_volume_by(delta, cx);
                }
                cx.stop_propagation();
            }))
            .child(
                icon_button(
                    ("mute", usize::from(muted)),
                    if muted || volume <= 0.5 {
                        Icon::VolumeMute
                    } else {
                        Icon::Volume
                    },
                    muted,
                    t,
                )
                .tooltip(tip(
                    if muted { "取消静音" } else { "静音" },
                    self.key_hint("toggle_mute"),
                ))
                .on_click(cx.listener(|this, _, w, cx| this.toggle_mute(&ToggleMute, w, cx))),
            )
            .when(!narrow, |d| {
                d.child(
                    div()
                        .id("vol-bars")
                        .relative()
                        .flex()
                        .items_center()
                        .h(t.control_h_md)
                        .cursor_pointer()
                        .tooltip(tip("音量 · 点击或按住拖动调节", self.key_hint("volume_up")))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                                this.chrome.scrub.dragging_volume = true;
                                this.set_volume_from_x(ev.position.x, cx);
                                cx.stop_propagation();
                            }),
                        )
                        .child(
                            canvas(move |b, _, _| vol_slot.set(Some(b)), |_, _, _, _| {})
                                .absolute()
                                .size_full(),
                        )
                        .children((0..10).map(|i| {
                            let on = i < filled;
                            div()
                                .w(px(if compact { 5. } else { 6. }))
                                .h(t.control_h_md)
                                .flex()
                                .items_center()
                                .child(
                                    div()
                                        .w(px(if compact { 3. } else { 4. }))
                                        .h(px(10.))
                                        .when(on, |d| {
                                            d.bg(t.text_dim)
                                                .group_hover("volume", move |s| s.bg(t.text))
                                        })
                                        .when(!on, |d| d.bg(t.line_strong)),
                                )
                        })),
                )
                .when(!compact, |d| {
                    d.child(
                        div()
                            .w(px(26.))
                            .pl(px(2.))
                            .font_family(FONT_MONO)
                            .text_size(t.text_sm)
                            .text_color(if muted { t.text_faint } else { t.text_dim })
                            .child(if muted {
                                "0".to_string()
                            } else {
                                format!("{volume:.0}")
                            }),
                    )
                })
            })
    }

    fn window_buttons(
        &self,
        tiny: bool,
        compact: bool,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let session = &self.active().session;
        let cur_uri = session.current_media_uri();
        let has_media = cur_uri.is_some();
        let favorite = cur_uri.as_ref().is_some_and(|u| self.store.is_favorite(u));

        div()
            .flex()
            .items_center()
            .gap(px(if compact { 1. } else { 2. }))
            .when(!tiny, |d| {
                d.child(
                    icon_button(
                        ("palette", usize::from(self.overlays.palette_open)),
                        Icon::Command,
                        self.overlays.palette_open,
                        t,
                    )
                    .tooltip(tip(
                        "命令面板（内核与插件命令）",
                        self.key_hint("open_palette"),
                    ))
                    .on_click(cx.listener(|this, _, w, cx| {
                        this.open_command_palette(&OpenCommandPalette, w, cx)
                    })),
                )
            })
            .when(
                has_media && !tiny && !self.is_dock_button_hidden("info"),
                |d| {
                    d.child(
                        icon_button(
                            ("info", usize::from(self.overlays.info_open)),
                            Icon::Info,
                            self.overlays.info_open,
                            t,
                        )
                        .tooltip(tip("媒体详情", self.key_hint("toggle_info")))
                        .on_click(
                            cx.listener(|this, _, w, cx| this.toggle_info(&ToggleInfo, w, cx)),
                        ),
                    )
                },
            )
            .when(has_media && !self.is_dock_button_hidden("favorite"), |d| {
                d.child(
                    ghost(("favorite", usize::from(favorite)), false, t)
                        .child(if favorite {
                            icon(Icon::StarFill, 16., t.accent)
                        } else {
                            icon_on_ghost(Icon::Star, t.text_dim, t)
                        })
                        .tooltip(tip(
                            if favorite { "取消收藏" } else { "收藏" },
                            self.key_hint("toggle_favorite"),
                        ))
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.toggle_favorite(&ToggleFavorite, w, cx)
                        })),
                )
            })
            .child(
                icon_button(
                    ("settings", usize::from(self.overlays.settings_open)),
                    Icon::Settings,
                    self.overlays.settings_open,
                    t,
                )
                .tooltip(tip("设置", self.key_hint("open_settings")))
                .on_click(cx.listener(|this, _, w, cx| this.open_settings(&OpenSettings, w, cx))),
            )
            .child(
                icon_button(
                    ("fullscreen", usize::from(self.chrome.fullscreen)),
                    if self.chrome.fullscreen {
                        Icon::FullscreenExit
                    } else {
                        Icon::Fullscreen
                    },
                    false,
                    t,
                )
                .tooltip(if self.chrome.fullscreen {
                    tip("退出全屏", Some("Esc".into()))
                } else {
                    tip("全屏", self.key_hint("toggle_fullscreen"))
                })
                .on_click(
                    cx.listener(|this, _, w, cx| this.toggle_fullscreen(&ToggleFullscreen, w, cx)),
                ),
            )
    }
}
