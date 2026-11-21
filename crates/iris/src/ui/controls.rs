//! The control bar: under the picture when windowed, floating over it in
//! fullscreen. A video's timeline runs along the bar's top edge.
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, ScrollWheelEvent, StatefulInteractiveElement, Styled, canvas, div,
    prelude::FluentBuilder, px, relative,
};

use super::widgets::{ghost, icon, icon_button, icon_on_ghost, separator, text_button, tip};
use super::*;
use crate::assets::Icon;

/// Height of the timeline's hit area, which straddles the bar's top edge.
const SEAM_HIT: f32 = 14.;
/// How much of that hit area lies above the bar, over the picture.
const SEAM_ABOVE: f32 = 6.;

impl IrisWindow {
    /// One row of controls; the directory toggle sits at the end next to the
    /// directory. `floating` is the fullscreen variant, sized by its frame.
    pub(super) fn render_dock(
        &self,
        floating: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let kind = self.viewports[0].session.kind();
        let left = self.settings.directory_side == Side::Left;
        let dir_open = if self.fullscreen {
            self.dir_popup
        } else {
            self.show_directory
        };
        let mut dir = Some(
            icon_button(
                ("dir", usize::from(dir_open)),
                if left {
                    Icon::PanelLeft
                } else {
                    Icon::PanelRight
                },
                dir_open,
            )
            .tooltip(tip(
                if dir_open {
                    "收起目录"
                } else {
                    "展开目录"
                },
                Some("Tab"),
            ))
            .on_click(cx.listener(|this, _, w, cx| this.toggle_directory(&ToggleDirectory, w, cx))),
        );
        let (start, end): (Option<AnyElement>, Option<AnyElement>) = match kind {
            Some(Kind::Video) => (
                Some(self.video_transport(cx).into_any_element()),
                Some(self.video_fields(cx).into_any_element()),
            ),
            Some(Kind::Photo) => (
                Some(self.photo_transport(cx).into_any_element()),
                Some(self.photo_fields(window, cx).into_any_element()),
            ),
            None => (None, None),
        };
        // GPUI keeps a tooltip, built from the label of that moment, in the
        // element's state, and still shows it when the click lands before the
        // tooltip delay is over. Keying the row on where its buttons sit, and
        // each toggle on what it shows, drops that tooltip when either changes.
        let layout = kind.map_or(0, |k| k as usize + 1) * 4
            + usize::from(left) * 2
            + usize::from(self.show_directory && !floating);
        let mut row = div()
            .id(("dock-row", layout))
            .size_full()
            .flex()
            .items_center()
            .px(px(8.));
        if left {
            row = row
                .children(dir.take())
                .when(start.is_some(), |d| d.child(separator()));
        }
        row = row
            .children(start)
            .child(div().flex_1())
            .children(end)
            .child(self.window_buttons(cx));
        if let Some(dir) = dir {
            row = row.child(separator()).child(dir);
        }

        let timeline = (kind == Some(Kind::Video)).then(|| self.render_timeline(cx));
        let seam = timeline.is_some();
        div()
            .relative()
            .when(floating, |d| d.size_full())
            .when(!floating, |d| {
                d.h(px(DOCK_H))
                    .w_full()
                    .flex_none()
                    .bg(window_bg())
                    .when(!seam, |d| d.border_t_1().border_color(line()))
            })
            .child(row)
            // Last, so the part above the bar is on top of the picture.
            .children(timeline)
    }

    fn step_button(
        &self,
        id: &'static str,
        delta: isize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let back = delta < 0;
        icon_button(id, if back { Icon::Prev } else { Icon::Next }, false)
            .tooltip(tip(
                if back { "上一个" } else { "下一个" },
                Some(if back { "PgUp" } else { "PgDn" }),
            ))
            .on_click(cx.listener(move |this, _, _, cx| this.step(delta, cx)))
    }

    fn video_transport(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = &self.viewports[0].session.state;
        let paused = state.paused;
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(self.step_button("prev", -1, cx))
            .child(
                ghost(("play", usize::from(paused)), false)
                    .child(icon_on_ghost(
                        if paused { Icon::Play } else { Icon::Pause },
                        text(),
                    ))
                    .tooltip(tip(if paused { "播放" } else { "暂停" }, Some("Space")))
                    .on_click(cx.listener(|this, _, w, cx| this.toggle_pause(&TogglePause, w, cx))),
            )
            .child(self.step_button("next", 1, cx))
            .child(
                div()
                    .pl(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .font_family(FONT_MONO)
                    .text_size(px(12.))
                    .child(div().text_color(text()).child(fmt_time(state.time_pos)))
                    .child(div().text_color(text_faint()).child("/"))
                    .child(div().text_color(text_dim()).child(if state.duration > 0. {
                        fmt_time(state.duration)
                    } else {
                        "--:--".into()
                    })),
            )
    }

    fn video_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = &self.viewports[0].session.state;
        let subs = state.subs_visible;
        let audio = match state.audio.as_str() {
            "" | "no" => "—".to_string(),
            track => track.to_string(),
        };
        let speed = state.speed;
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(
                icon_button(("subs", usize::from(subs)), Icon::Subtitles, subs)
                    .tooltip(tip(if subs { "隐藏字幕" } else { "显示字幕" }, Some("V")))
                    .on_click(
                        cx.listener(|this, _, _, cx| this.command(Command::ToggleSubtitles, cx)),
                    ),
            )
            .child(
                ghost("audio", false)
                    .px(px(6.))
                    .gap(px(6.))
                    .child(icon_on_ghost(Icon::Audio, text_dim()))
                    .child(div().font_family(FONT_MONO).text_size(px(11.)).child(audio))
                    .tooltip(tip("切换音轨", Some("A")))
                    .on_click(cx.listener(|this, _, _, cx| this.command(Command::NextAudio, cx))),
            )
            .child(
                text_button("speed", format!("{speed:.2}×"))
                    .when((speed - 1.).abs() > 0.001, |d| d.text_color(text()))
                    .tooltip(tip("速度 · 点按恢复 1.0×", Some("[ ]")))
                    .on_click(
                        cx.listener(|this, _, _, cx| this.command(Command::SetSpeed(1.), cx)),
                    ),
            )
            .child(separator())
            .child(self.render_volume(cx))
            .child(separator())
    }

    /// Speaker, ten segments (click to set, wheel to nudge) and the value.
    /// Grey rather than the accent: the accent is kept for where playback is.
    fn render_volume(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let volume = self.viewports[0].session.state.volume;
        let filled = (volume / 10.).round().clamp(0., 10.) as usize;
        div()
            .id("volume")
            .group("volume")
            .h(px(BUTTON))
            .flex()
            .items_center()
            .gap(px(6.))
            .pl(px(4.))
            .tooltip(tip("音量 · 滚轮调节", Some("↑ ↓")))
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                let dy = f32::from(ev.delta.pixel_delta(px(20.)).y) as f64;
                if dy != 0. {
                    this.command(Command::VolumeBy(dy / 10.), cx);
                }
                cx.stop_propagation();
            }))
            .child(icon(Icon::Volume, 16., text_faint()))
            .child(div().flex().items_center().children((0..10).map(|i| {
                let on = i < filled;
                div()
                    .id(("vol", i))
                    .group("vol")
                    .w(px(6.))
                    .h(px(BUTTON))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .child(
                        div()
                            .w(px(4.))
                            .h(px(10.))
                            .when(on, |d| {
                                d.bg(text_dim()).group_hover("volume", |s| s.bg(text()))
                            })
                            .when(!on, |d| {
                                d.bg(line_strong())
                                    .group_hover("vol", |s| s.bg(text_faint()))
                            }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.command(Command::SetVolume(((i + 1) * 10) as f64), cx)
                    }))
            })))
            .child(
                div()
                    .w(px(24.))
                    .font_family(FONT_MONO)
                    .text_size(px(11.))
                    .text_color(text_dim())
                    .child(format!("{volume:.0}")),
            )
    }

    fn photo_transport(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.display_entries();
        let index = self.viewports[0]
            .session
            .current
            .as_ref()
            .and_then(|p| entries.iter().position(|e| &e.path == p));
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(self.step_button("prev", -1, cx))
            .child(self.step_button("next", 1, cx))
            .child(
                div()
                    .pl(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .font_family(FONT_MONO)
                    .text_size(px(12.))
                    .child(
                        div()
                            .text_color(text())
                            .child(index.map_or("—".to_string(), |i| (i + 1).to_string())),
                    )
                    .child(div().text_color(text_faint()).child("/"))
                    .child(
                        div()
                            .text_color(text_dim())
                            .child(entries.len().to_string()),
                    ),
            )
    }

    fn photo_fields(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = &self.viewports[0].image;
        // Relative to the image's own pixels, so 1:1 reads 100%.
        let (viewport, media) = self.image_geometry();
        let known = viewport[0] > 0. && self.viewports[0].session.state.video_size.0 > 0;
        let percent =
            known.then(|| view.pixel_scale(viewport, media, window.scale_factor() as f64) * 100.);
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(
                icon_button("zoom-out", Icon::Minus, false)
                    .tooltip(tip("缩小", Some("-")))
                    .on_click(cx.listener(|this, _, w, cx| this.zoom_out(&ZoomOut, w, cx))),
            )
            .child(
                div()
                    .w(px(48.))
                    .flex()
                    .justify_center()
                    .font_family(FONT_MONO)
                    .text_size(px(12.))
                    .text_color(text_dim())
                    .child(percent.map_or("—".to_string(), |p| format!("{p:.0}%"))),
            )
            .child(
                icon_button("zoom-in", Icon::Plus, false)
                    .tooltip(tip("放大", Some("=")))
                    .on_click(cx.listener(|this, _, w, cx| this.zoom_in(&ZoomIn, w, cx))),
            )
            .child(
                icon_button("fit", Icon::Fit, !view.zoomed())
                    .tooltip(tip("适应窗口", Some("0")))
                    .on_click(cx.listener(|this, _, w, cx| this.fit_image(&FitImage, w, cx))),
            )
            .child(
                text_button("actual", "1:1")
                    .tooltip(tip("原始大小", Some("Space")))
                    .on_click(cx.listener(|this, _, w, cx| {
                        this.toggle_image_size(&ToggleImageSize, w, cx)
                    })),
            )
            .child(separator())
    }

    /// Favorite and fullscreen: about the file and the window, not playback.
    fn window_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.viewports[0].session.current.as_deref();
        let favorite = current.is_some_and(|p| self.store.favorite(p));
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .when(current.is_some(), |d| {
                d.child(
                    ghost(("favorite", usize::from(favorite)), false)
                        .child(if favorite {
                            icon(Icon::StarFill, 16., accent())
                        } else {
                            icon_on_ghost(Icon::Star, text_dim())
                        })
                        .tooltip(tip(if favorite { "取消收藏" } else { "收藏" }, Some("S")))
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.toggle_favorite(&ToggleFavorite, w, cx)
                        })),
                )
            })
            .child(
                icon_button(
                    ("fullscreen", usize::from(self.fullscreen)),
                    if self.fullscreen {
                        Icon::FullscreenExit
                    } else {
                        Icon::Fullscreen
                    },
                    false,
                )
                .tooltip(if self.fullscreen {
                    tip("退出全屏", Some("Esc"))
                } else {
                    tip("全屏", Some("F"))
                })
                .on_click(
                    cx.listener(|this, _, w, cx| this.toggle_fullscreen(&ToggleFullscreen, w, cx)),
                ),
            )
    }

    /// A 2px line on the bar's top edge with a tick for the playhead. Hovering
    /// or dragging thickens it and shows the time under the pointer.
    fn render_timeline(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = &self.viewports[0].session.state;
        let duration = state.duration;
        let pct = if duration > 0. {
            (state.time_pos / duration).clamp(0., 1.) as f32
        } else {
            0.
        };
        let active = self.timeline_hover || self.seeking;
        let track = if active { 4. } else { 2. };
        let slot = self.progress_bounds.clone();
        let label = active
            .then(|| {
                let b = self.progress_bounds.get()?;
                let p = self.mouse_pos?;
                let width = f32::from(b.size.width);
                if width <= 0. || duration <= 0. {
                    return None;
                }
                let x = f32::from(p.x - b.origin.x).clamp(0., width);
                const W: f32 = 72.;
                let left = (x - W / 2.).clamp(0., (width - W).max(0.));
                Some(
                    div()
                        .absolute()
                        .left(px(left))
                        .bottom(px(SEAM_HIT + 4.))
                        .w(px(W))
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .h(px(20.))
                                .px(px(6.))
                                .flex()
                                .items_center()
                                .bg(panel())
                                .border_1()
                                .border_color(line_strong())
                                .font_family(FONT_MONO)
                                .text_size(px(11.))
                                .text_color(text())
                                .child(fmt_time(x as f64 / width as f64 * duration)),
                        ),
                )
            })
            .flatten();
        div()
            .id("timeline")
            .absolute()
            .top(px(-SEAM_ABOVE))
            .left_0()
            .right_0()
            .h(px(SEAM_HIT))
            .cursor_pointer()
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.timeline_hover = *hovered;
                cx.notify();
            }))
            // Dragging continues in the root's mouse-move handler, so it keeps
            // working when the pointer drifts off this thin strip.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.seeking = true;
                    this.seek_to_x(ev.position.x);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                canvas(move |b, _, _| slot.set(Some(b)), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(
                div()
                    .absolute()
                    .top(px(SEAM_ABOVE))
                    .left_0()
                    .right_0()
                    .h(px(track))
                    .bg(if active { line_strong() } else { line() }),
            )
            .child(
                div()
                    .absolute()
                    .top(px(SEAM_ABOVE))
                    .left_0()
                    .w(relative(pct))
                    .h(px(track))
                    .bg(accent()),
            )
            .child(
                div()
                    .absolute()
                    .top(px(SEAM_ABOVE - 3.))
                    .left(relative(pct))
                    .ml(px(-1.))
                    .w(px(2.))
                    .h(px(track + 6.))
                    .bg(accent()),
            )
            .children(label)
    }
}

pub(super) fn fmt_time(t: f64) -> String {
    let s = t.max(0.) as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}
