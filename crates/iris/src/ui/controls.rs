//! The control bar: under the picture when windowed, floating over it in
//! fullscreen. Adapts cleanly to narrow mini-windows, anchors popovers directly
//! above their buttons, and supports 60fps timeline + continuous volume dragging.
use gpui::{
    AnyElement, Bounds, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Pixels, ScrollWheelEvent, StatefulInteractiveElement, Styled, canvas, div,
    prelude::FluentBuilder, px, relative,
};

use super::widgets::{ghost, icon, icon_button, icon_on_ghost, separator, text_button, tip};
use super::*;
use crate::assets::Icon;
use crate::player::TrackKind;

/// Height of the timeline's hit area, which straddles the bar's top edge.
const SEAM_HIT: f32 = 14.;
/// How much of that hit area lies above the bar, over the picture.
const SEAM_ABOVE: f32 = 6.;

/// Wraps a control button with a zero-cost layout probe so popovers anchor
/// directly above it.
fn anchor_wrap(
    slot: Rc<Cell<Option<Bounds<Pixels>>>>,
    child: impl IntoElement,
) -> impl IntoElement {
    div()
        .relative()
        .flex_none()
        .child(child)
        .child(
            canvas(move |b, _, _| slot.set(Some(b)), |_, _, _, _| {})
                .absolute()
                .size_full(),
        )
}

impl IrisWindow {
    /// Width available to the dock row (excluding docked directory when windowed).
    fn dock_available_width(&self, floating: bool, window: &Window) -> f32 {
        let total = f32::from(window.viewport_size().width);
        if floating {
            (total - FLOAT_GAP * 2.).max(0.)
        } else if self.show_directory {
            (total - self.effective_directory_width(window)).max(0.)
        } else {
            total
        }
    }

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
        let avail_w = self.dock_available_width(floating, window);
        let narrow = avail_w < 640.;
        let tiny = avail_w < 500.;
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
                Some(self.video_transport(tiny, cx).into_any_element()),
                Some(self.video_fields(narrow, tiny, cx).into_any_element()),
            ),
            Some(Kind::Audio) => (
                Some(self.video_transport(tiny, cx).into_any_element()),
                Some(self.audio_fields(narrow, cx).into_any_element()),
            ),
            Some(Kind::Photo) => (
                Some(self.photo_transport(cx).into_any_element()),
                Some(self.photo_fields(narrow, window, cx).into_any_element()),
            ),
            None => (None, None),
        };
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
            .child(self.window_buttons(tiny, cx));
        if let Some(dir) = dir {
            row = row.child(separator()).child(dir);
        }

        let timeline =
            matches!(kind, Some(Kind::Video | Kind::Audio)).then(|| self.render_timeline(cx));
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
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
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

    fn video_transport(&self, tiny: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let state = &self.viewports[0].session.state;
        let paused = state.paused;
        let shown_pos = self
            .seek_preview_ratio
            .filter(|_| self.seeking && state.duration > 0.)
            .map_or(state.time_pos, |r| r as f64 * state.duration);
        let second_time = if state.duration > 0. {
            if self.show_remaining {
                format!("-{}", fmt_time((state.duration - shown_pos).max(0.)))
            } else {
                fmt_time(state.duration)
            }
        } else {
            "--:--".into()
        };
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .when(!tiny, |d| d.child(self.step_button("prev", -1, cx)))
            .child(
                ghost(("play", usize::from(paused)), false)
                    .child(icon_on_ghost(
                        if paused { Icon::Play } else { Icon::Pause },
                        text(),
                    ))
                    .tooltip(tip(if paused { "播放" } else { "暂停" }, Some("Space")))
                    .on_click(cx.listener(|this, _, w, cx| this.toggle_pause(&TogglePause, w, cx))),
            )
            .when(!tiny, |d| d.child(self.step_button("next", 1, cx)))
            .child(
                div()
                    .id(("time-readout", usize::from(self.show_remaining)))
                    .h(px(BUTTON))
                    .ml(px(4.))
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .cursor_pointer()
                    .hover(|s| s.bg(hover()))
                    .font_family(FONT_MONO)
                    .text_size(px(12.))
                    .tooltip(tip(
                        if self.show_remaining {
                            "点击显示总时长"
                        } else {
                            "点击显示剩余时间"
                        },
                        None,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_remaining = !this.show_remaining;
                        cx.notify();
                    }))
                    .child(div().text_color(text()).child(fmt_time(shown_pos)))
                    .when(!tiny, |d| {
                        d.child(div().text_color(text_faint()).child("/"))
                            .child(div().text_color(text_dim()).child(second_time))
                    }),
            )
    }

    fn video_fields(&self, narrow: bool, tiny: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let session = &self.viewports[0].session;
        let state = &session.state;
        let subs = state.subs_visible;
        let looping = state.loop_file;
        let audio = session
            .selected_track(TrackKind::Audio)
            .map(|t| {
                if !t.lang.is_empty() {
                    t.lang.to_ascii_uppercase()
                } else {
                    t.id.to_string()
                }
            })
            .unwrap_or_else(|| match state.audio.as_str() {
                "" | "no" => "—".to_string(),
                track => track.to_string(),
            });
        let speed = state.speed;
        let sub_pop = self.popover == Some(PopoverKind::Subtitles);
        let audio_pop = self.popover == Some(PopoverKind::Audio);
        let speed_pop = self.popover == Some(PopoverKind::Speed);
        let more_pop = self.popover == Some(PopoverKind::More);

        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .when(!narrow, |d| {
                d.child(
                    icon_button(("loop", usize::from(looping)), Icon::Repeat, looping)
                        .tooltip(tip(
                            if looping {
                                "关闭单曲循环"
                            } else {
                                "单曲循环"
                            },
                            Some("L"),
                        ))
                        .on_click(cx.listener(|this, _, w, cx| this.toggle_loop(&ToggleLoop, w, cx))),
                )
            })
            .child(anchor_wrap(
                self.anchor_sub.clone(),
                icon_button(
                    ("subs", usize::from(subs) * 2 + usize::from(sub_pop)),
                    Icon::Subtitles,
                    subs || sub_pop,
                )
                .tooltip(tip("字幕 · 点击选轨与偏移", Some("J / V")))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.toggle_popover(PopoverKind::Subtitles, cx);
                })),
            ))
            .when(!tiny, |d| {
                d.child(anchor_wrap(
                    self.anchor_audio.clone(),
                    ghost(("audio", usize::from(audio_pop)), audio_pop)
                        .px(px(6.))
                        .gap(px(5.))
                        .child(icon_on_ghost(
                            Icon::Audio,
                            if audio_pop { text() } else { text_dim() },
                        ))
                        .when(!narrow, |d| {
                            d.child(div().font_family(FONT_MONO).text_size(px(11.)).child(audio))
                        })
                        .tooltip(tip("音轨 · 点击选轨", Some("A")))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_popover(PopoverKind::Audio, cx);
                        })),
                ))
            })
            .child(anchor_wrap(
                self.anchor_speed.clone(),
                text_button(("speed", usize::from(speed_pop)), format!("{speed:.2}×"))
                    .when(speed_pop, |d| d.bg(selected()).text_color(text()))
                    .when((speed - 1.).abs() > 0.001, |d| d.text_color(text()))
                    .tooltip(tip("播放速度 · 点击选择，右键还原 1.0×", Some("[ ]")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_popover(PopoverKind::Speed, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, _, _, cx| {
                            this.popover = None;
                            this.command(Command::SetSpeed(1.), cx);
                            this.show_osd("倍速 · 1.00×", cx);
                            cx.stop_propagation();
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                        let step = this.damped_wheel_steps(ev, 36.);
                        if step != 0 {
                            let next = player::step_speed(
                                this.viewports[0].session.state.speed,
                                step.signum(),
                            );
                            this.command(Command::SetSpeed(next), cx);
                            this.show_osd(format!("倍速 · {next:.2}×"), cx);
                        }
                        cx.stop_propagation();
                    })),
            ))
            .child(anchor_wrap(
                self.anchor_more.clone(),
                icon_button(("more", usize::from(more_pop)), Icon::More, more_pop)
                    .tooltip(tip("画面比例 / 旋转 / 截图 / 章节 / 详情", None))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_popover(PopoverKind::More, cx);
                    })),
            ))
            .child(separator())
            .child(self.render_volume(narrow, cx))
            .child(separator())
    }

    fn audio_fields(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let state = &self.viewports[0].session.state;
        let looping = state.loop_file;
        let speed = state.speed;
        let speed_pop = self.popover == Some(PopoverKind::Speed);
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(
                icon_button(("loop", usize::from(looping)), Icon::Repeat, looping)
                    .tooltip(tip(
                        if looping {
                            "关闭单曲循环"
                        } else {
                            "单曲循环"
                        },
                        Some("L"),
                    ))
                    .on_click(cx.listener(|this, _, w, cx| this.toggle_loop(&ToggleLoop, w, cx))),
            )
            .child(anchor_wrap(
                self.anchor_speed.clone(),
                text_button(("speed", usize::from(speed_pop)), format!("{speed:.2}×"))
                    .when(speed_pop, |d| d.bg(selected()).text_color(text()))
                    .when((speed - 1.).abs() > 0.001, |d| d.text_color(text()))
                    .tooltip(tip("播放速度 · 点击选择", Some("[ ]")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_popover(PopoverKind::Speed, cx);
                    })),
            ))
            .child(separator())
            .child(self.render_volume(narrow, cx))
            .child(separator())
    }

    /// Clickable speaker (mute toggle), continuously draggable 0–100% volume
    /// bar (plus wheel damping), and numeric readout.
    fn render_volume(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let state = &self.viewports[0].session.state;
        let volume = state.volume;
        let muted = state.muted;
        let filled = if muted {
            0
        } else {
            (volume / 10.).round().clamp(0., 10.) as usize
        };
        let vol_slot = self.vol_bounds.clone();
        div()
            .id("volume")
            .group("volume")
            .h(px(BUTTON))
            .flex()
            .items_center()
            .gap(px(4.))
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                let delta = this.damped_volume_delta(ev);
                if delta.abs() >= 0.5 {
                    this.command(Command::VolumeBy(delta), cx);
                    let next = (this.viewports[0].session.state.volume + delta).clamp(0., 100.);
                    this.show_osd(format!("音量 · {next:.0}%"), cx);
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
                )
                .tooltip(tip(if muted { "取消静音" } else { "静音" }, Some("M")))
                .on_click(cx.listener(|this, _, w, cx| this.toggle_mute(&ToggleMute, w, cx))),
            )
            .when(!narrow, |d| {
                d.child(
                    div()
                        .id("vol-bars")
                        .relative()
                        .flex()
                        .items_center()
                        .h(px(BUTTON))
                        .cursor_pointer()
                        .tooltip(tip("音量 · 点击或按住拖动调节", Some("↑ ↓")))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                                this.dragging_volume = true;
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
                                .w(px(6.))
                                .h(px(BUTTON))
                                .flex()
                                .items_center()
                                .child(
                                    div()
                                        .w(px(4.))
                                        .h(px(10.))
                                        .when(on, |d| {
                                            d.bg(text_dim())
                                                .group_hover("volume", |s| s.bg(text()))
                                        })
                                        .when(!on, |d| d.bg(line_strong())),
                                )
                        })),
                )
                .child(
                    div()
                        .w(px(24.))
                        .pl(px(2.))
                        .font_family(FONT_MONO)
                        .text_size(px(11.))
                        .text_color(if muted { text_faint() } else { text_dim() })
                        .child(if muted {
                            "0".to_string()
                        } else {
                            format!("{volume:.0}")
                        }),
                )
            })
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

    fn photo_fields(
        &self,
        narrow: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = &self.viewports[0].image;
        let hflip = self.viewports[0].session.state.hflip;
        let strip_on = self.settings.photo_filmstrip;
        let (viewport, media) = self.image_geometry();
        let known = viewport[0] > 0. && self.viewports[0].session.state.video_size.0 > 0;
        let percent =
            known.then(|| view.pixel_scale(viewport, media, window.scale_factor() as f64) * 100.);
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .when(!narrow, |d| {
                d.child(
                    icon_button(
                        ("filmstrip", usize::from(strip_on)),
                        Icon::Filmstrip,
                        strip_on,
                    )
                    .tooltip(tip(
                        if strip_on {
                            "隐藏缩略图条"
                        } else {
                            "显示缩略图条"
                        },
                        Some("G"),
                    ))
                    .on_click(
                        cx.listener(|this, _, w, cx| this.toggle_filmstrip(&ToggleFilmstrip, w, cx)),
                    ),
                )
                .child(
                    icon_button(("hflip", usize::from(hflip)), Icon::FlipH, hflip)
                        .tooltip(tip("水平镜像翻转", Some("H")))
                        .on_click(
                            cx.listener(|this, _, w, cx| this.toggle_hflip(&ToggleHFlip, w, cx)),
                        ),
                )
            })
            .child(
                icon_button("rotate", Icon::Rotate, false)
                    .tooltip(tip("顺时针旋转 90°", Some("R")))
                    .on_click(cx.listener(|this, _, w, cx| this.rotate_cw(&RotateCw, w, cx))),
            )
            .child(separator())
            .child(
                icon_button("zoom-out", Icon::Minus, false)
                    .tooltip(tip("缩小", Some("-")))
                    .on_click(cx.listener(|this, _, w, cx| this.zoom_out(&ZoomOut, w, cx))),
            )
            .child(
                div()
                    .w(px(46.))
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
            .when(!narrow, |d| {
                d.child(
                    text_button("actual", "1:1")
                        .tooltip(tip("原始大小", Some("Space")))
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.toggle_image_size(&ToggleImageSize, w, cx)
                        })),
                )
            })
            .child(separator())
    }

    /// Info, favorite, settings and fullscreen: about the file and the window.
    fn window_buttons(&self, tiny: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.viewports[0].session.current.as_deref();
        let favorite = current.is_some_and(|p| self.store.favorite(p));
        let set_key = if cfg!(target_os = "macos") {
            "⌘,"
        } else {
            "Ctrl+,"
        };
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .when(current.is_some() && !tiny, |d| {
                d.child(
                    icon_button(
                        ("info", usize::from(self.info_open)),
                        Icon::Info,
                        self.info_open,
                    )
                    .tooltip(tip("媒体详情", Some("I")))
                    .on_click(cx.listener(|this, _, w, cx| this.toggle_info(&ToggleInfo, w, cx))),
                )
            })
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
                    ("settings", usize::from(self.settings_open)),
                    Icon::Settings,
                    self.settings_open,
                )
                .tooltip(tip("设置", Some(set_key)))
                .on_click(cx.listener(|this, _, w, cx| this.open_settings(&OpenSettings, w, cx))),
            )
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

    /// A 2px line on the bar's top edge with chapter ticks and a playhead.
    /// While dragging, the visual bar locks 1:1 to the pointer at display refresh rate.
    fn render_timeline(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = &self.viewports[0].session;
        let state = &session.state;
        let duration = state.duration;
        let pct = if let Some(r) = self.seek_preview_ratio
            && self.seeking
        {
            r.clamp(0., 1.)
        } else if duration > 0. {
            (state.time_pos / duration).clamp(0., 1.) as f32
        } else {
            0.
        };
        let active = self.timeline_hover || self.seeking;
        let track = if active { 4. } else { 2. };
        let slot = self.progress_bounds.clone();

        // Subtle chapter markers along the timeline track.
        let chapter_marks: Vec<AnyElement> = if duration > 0. {
            state
                .chapters
                .iter()
                .filter(|c| c.time > 0.5 && c.time < duration - 0.5)
                .map(|c| {
                    let ratio = (c.time / duration).clamp(0., 1.) as f32;
                    div()
                        .absolute()
                        .top(px(SEAM_ABOVE - 1.))
                        .left(relative(ratio))
                        .w(px(1.))
                        .h(px(track + 2.))
                        .bg(window_bg())
                        .into_any_element()
                })
                .collect()
        } else {
            Vec::new()
        };

        let label = active
            .then(|| {
                let b = self.progress_bounds.get()?;
                let p = self.mouse_pos?;
                let width = f32::from(b.size.width);
                if width <= 0. || duration <= 0. {
                    return None;
                }
                let x = f32::from(p.x - b.origin.x).clamp(0., width);
                let hover_t = x as f64 / width as f64 * duration;
                let chapter_title = session.chapter_at(hover_t).map(|c| c.title.clone());
                let box_w: f32 = if chapter_title.is_some() { 180. } else { 76. };
                let left = (x - box_w / 2.).clamp(0., (width - box_w).max(0.));
                Some(
                    div()
                        .absolute()
                        .left(px(left))
                        .bottom(px(SEAM_HIT + 4.))
                        .w(px(box_w))
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .h(px(22.))
                                .max_w(px(box_w))
                                .px(px(6.))
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .bg(panel())
                                .border_1()
                                .border_color(line_strong())
                                .font_family(FONT_MONO)
                                .text_size(px(11.))
                                .text_color(text())
                                .child(fmt_time(hover_t))
                                .when_some(chapter_title, |d, ch| {
                                    d.child(
                                        div()
                                            .font_family(FONT_UI)
                                            .text_size(px(10.))
                                            .text_color(text_dim())
                                            .truncate()
                                            .child(ch),
                                    )
                                }),
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.seeking = true;
                    this.last_seek_drag = Instant::now();
                    this.update_seek_preview(ev.position.x);
                    this.seek_to_x(ev.position.x, true);
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
            .children(chapter_marks)
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
