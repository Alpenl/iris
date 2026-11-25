//! Video, audio, and photo transport/field buttons, stream quality variant
//! trigger, and plugin toolbar button slot.

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement, ScrollWheelEvent,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use iris_core::player::{self, AspectMode, Command, TrackKind, format_time};

use super::super::IrisWindow;
use super::super::actions::{
    CycleAspect, FitImage, RotateCw, TakeScreenshot, ToggleFilmstrip, ToggleHFlip, ToggleImageSize,
    ToggleLoop, TogglePause, ZoomIn, ZoomOut,
};
use super::super::kit::{
    badge_pill, ghost, hstack, icon, icon_button, icon_button_disabled, icon_on_ghost, text_button,
    tip,
};
use super::super::state::PopoverKind;
use super::{anchor_wrap, dock_sep};
use crate::assets::Icon;
use crate::theme::{FONT_MONO, ThemeTokens};

impl IrisWindow {
    pub(super) fn render_plugin_toolbar_slot(
        &self,
        compact: bool,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let buttons: Vec<(String, iris_api::ToolbarButton)> = self
            .plugins
            .toolbar_buttons()
            .into_iter()
            .map(|(pid, b)| (pid.to_string(), b.clone()))
            .collect();
        if buttons.is_empty() {
            return None;
        }
        let items = buttons.into_iter().enumerate().map(|(i, (pid, btn))| {
            let hi = btn.active || btn.accent;
            let tip_lbl = btn
                .tooltip
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(&btn.label)
                .to_string();
            let act = btn.action_id;
            let mapped = btn
                .icon
                .as_deref()
                .and_then(Icon::from_name)
                .or_else(|| (btn.icon.is_some() && compact).then_some(Icon::Puzzle));
            let base = ghost(("plugin-tb", i), hi, t)
                .rounded(t.radius_md)
                .when(btn.accent, |d| d.border_1().border_color(t.accent))
                .tooltip(tip(tip_lbl, None))
                .on_click(cx.listener(move |this, _, w, cx| {
                    this.invoke_plugin_action(&pid, &act, w, cx);
                }));
            if let Some(glyph) = mapped {
                return base.child(if hi {
                    icon(glyph, 15., t.accent)
                } else {
                    icon_on_ghost(glyph, t.text_dim, t)
                });
            }
            let col = match (btn.accent, btn.active) {
                (true, _) => t.accent,
                (_, true) => t.text,
                _ => t.text_muted,
            };
            let lbl = div()
                .max_w(px(88.))
                .truncate()
                .text_size(t.text_sm)
                .text_color(col)
                .child(btn.label);
            base.px(px(if compact { 5. } else { 7. }))
                .gap(px(4.))
                .child(icon(
                    Icon::Puzzle,
                    12.,
                    if hi { t.accent } else { t.text_faint },
                ))
                .child(lbl)
        });
        Some(
            hstack(if compact { 2. } else { 3. })
                .pr(px(if compact { 2. } else { 4. }))
                .children(items),
        )
    }

    fn render_variant_selector(
        &self,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let meta = self.catalog.active_stream_meta.as_ref()?;
        let label = meta
            .active_variant
            .as_deref()
            .and_then(|aid| meta.variants.iter().find(|v| v.id == aid))
            .or_else(|| meta.variants.first())
            .map(|v| v.label.clone())?;
        let var_pop = self.overlays.popover == Some(PopoverKind::Variants);
        let btn = text_button(("variants", usize::from(var_pop)), label, t)
            .rounded(t.radius_md)
            .border_1()
            .border_color(if var_pop { t.accent } else { t.line_strong })
            .when(var_pop, |d| d.bg(t.selected).text_color(t.accent))
            .when(!var_pop, |d| d.text_color(t.text))
            .tooltip(tip("切换清晰度 / 播放线路", None))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_popover(PopoverKind::Variants, cx)));
        Some(anchor_wrap(self.chrome.probes.anchor_variants.clone(), btn))
    }

    fn can_step_media(&self) -> bool {
        if !self.queue.is_empty() {
            return self.queue.len() > 1;
        }
        self.catalog.playable_count > 1 || self.plugin_view.playlist.len() > 1
    }

    fn step_button(
        &self,
        id: &'static str,
        delta: isize,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (glyph, lbl, act) = if delta < 0 {
            (Icon::Prev, "上一个", "open_prev")
        } else {
            (Icon::Next, "下一个", "open_next")
        };
        if !self.can_step_media() {
            return icon_button_disabled(id, glyph, false, t)
                .tooltip(tip("当前列表仅有一项", None));
        }
        icon_button(id, glyph, false, t)
            .tooltip(self.act_tip(lbl, act))
            .on_click(cx.listener(move |this, _, _, cx| this.step(delta, true, cx)))
    }

    fn loop_button(
        &self,
        looping: bool,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let lbl = if looping {
            "关闭单曲循环"
        } else {
            "单曲循环"
        };
        icon_button(("loop", usize::from(looping)), Icon::Repeat, looping, t)
            .tooltip(self.act_tip(lbl, "toggle_loop"))
            .on_click(cx.listener(|this, _, w, cx| this.toggle_loop(&ToggleLoop, w, cx)))
    }

    fn speed_button(
        &self,
        speed: f64,
        with_wheel_and_reset: bool,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let speed_pop = self.overlays.popover == Some(PopoverKind::Speed);
        let tip_txt = if with_wheel_and_reset {
            "播放速度 · 点击选择，右键还原 1.0×"
        } else {
            "播放速度 · 点击选择"
        };
        let mut btn = text_button(("speed", usize::from(speed_pop)), format!("{speed:.2}×"), t)
            .when(speed_pop, |d| d.bg(t.selected).text_color(t.text))
            .when((speed - 1.).abs() > 0.001, |d| d.text_color(t.text))
            .tooltip(self.act_tip(tip_txt, "speed_up"))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_popover(PopoverKind::Speed, cx)));
        if with_wheel_and_reset {
            btn = btn
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, _, cx| {
                        this.overlays.popover = None;
                        this.command(Command::SetSpeed(1.), cx);
                        this.show_osd("倍速 · 1.00×", cx);
                        cx.stop_propagation();
                    }),
                )
                .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                    let step = this.damped_wheel_steps(ev, 36.);
                    if step != 0 {
                        let cur = this.active().session.state().speed;
                        let next = player::step_speed(cur, step.signum());
                        this.command(Command::SetSpeed(next), cx);
                        this.show_osd(format!("倍速 · {next:.2}×"), cx);
                    }
                    cx.stop_propagation();
                }));
        }
        anchor_wrap(self.chrome.probes.anchor_speed.clone(), btn)
    }

    pub(super) fn video_transport(
        &self,
        tiny: bool,
        compact: bool,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.active().session.state();
        let (paused, rem) = (state.paused, self.chrome.show_remaining);
        let scrub = &self.chrome.scrub;
        let shown_pos = scrub
            .seek_preview_ratio
            .filter(|_| scrub.seeking && state.duration > 0.)
            .map_or(state.time_pos, |r| r as f64 * state.duration);
        let second_time = if state.duration <= 0. {
            "--:--".into()
        } else if rem {
            format!("-{}", format_time((state.duration - shown_pos).max(0.)))
        } else {
            format_time(state.duration)
        };

        let readout = if state.is_live {
            div()
                .id("live-badge")
                .ml(px(if compact { 2. } else { 4. }))
                .tooltip(tip("实时直播流", None))
                .child(badge_pill("● LIVE", true, t))
                .into_any_element()
        } else {
            let tip_lbl = if rem {
                "点击显示总时长"
            } else {
                "点击显示剩余时间"
            };
            hstack(if compact { 4. } else { 5. })
                .id(("time-readout", usize::from(rem)))
                .h(t.control_h_md)
                .ml(px(if compact { 2. } else { 4. }))
                .px(px(if compact { 4. } else { 6. }))
                .rounded(t.radius_sm)
                .cursor_pointer()
                .hover(|s| s.bg(t.hover))
                .font_family(FONT_MONO)
                .text_size(t.text_md)
                .tooltip(tip(tip_lbl, None))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.chrome.show_remaining = !this.chrome.show_remaining;
                    cx.notify();
                }))
                .child(div().text_color(t.text).child(format_time(shown_pos)))
                .when(!tiny, |d| {
                    d.child(div().text_color(t.text_faint).child("/"))
                        .child(div().text_color(t.text_dim).child(second_time))
                })
                .into_any_element()
        };

        let (play_glyph, play_lbl) = if paused {
            (Icon::Play, "播放")
        } else {
            (Icon::Pause, "暂停")
        };
        hstack(if compact { 1. } else { 2. })
            .when(!tiny, |d| d.child(self.step_button("prev", -1, t, cx)))
            .child(
                ghost(("play", usize::from(paused)), false, t)
                    .child(icon_on_ghost(play_glyph, t.text, t))
                    .tooltip(self.act_tip(play_lbl, "toggle_pause"))
                    .on_click(cx.listener(|this, _, w, cx| this.toggle_pause(&TogglePause, w, cx))),
            )
            .when(!tiny, |d| d.child(self.step_button("next", 1, t, cx)))
            .child(readout)
            .when(state.ab_start.is_some(), |d| {
                let a = format_time(state.ab_start.unwrap_or(0.));
                let label = state.ab_end.map_or_else(
                    || format!("A {a}"),
                    |b| format!("A-B {a}–{}", format_time(b)),
                );
                d.child(
                    text_button("native-ab", label, t)
                        .text_color(t.accent)
                        .tooltip(tip("点击清除 A-B 循环", None))
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.clear_ab_loop(&super::super::actions::ClearAbLoop, w, cx)
                        })),
                )
            })
    }

    pub(super) fn video_fields(
        &self,
        narrow: bool,
        tiny: bool,
        compact: bool,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let session = &self.active().session;
        let state = session.state();
        let subs = state.subs_visible;
        let aspect_on = state.aspect != AspectMode::Original;
        let aspect_tip = format!(
            "画面比例 · {}",
            super::super::strings::chrome::aspect_label(state.aspect)
        );
        let audio = session.selected_track(TrackKind::Audio).map_or_else(
            || match state.audio.as_str() {
                "" | "no" => "—".to_string(),
                track => track.to_string(),
            },
            |tr| {
                if tr.lang.is_empty() {
                    tr.id.to_string()
                } else {
                    tr.lang.to_ascii_uppercase()
                }
            },
        );
        let sub_pop = self.overlays.popover == Some(PopoverKind::Subtitles);
        let audio_pop = self.overlays.popover == Some(PopoverKind::Audio);
        let more_pop = self.overlays.popover == Some(PopoverKind::More);
        let probes = &self.chrome.probes;
        let show = |k: &str| !self.is_dock_button_hidden(k);

        let audio_col = if audio_pop { t.text } else { t.text_dim };
        let audio_btn = ghost(("audio", usize::from(audio_pop)), audio_pop, t)
            .px(px(if compact { 5. } else { 6. }))
            .gap(px(5.))
            .child(icon_on_ghost(Icon::Audio, audio_col, t))
            .when(!narrow && !compact, |d| {
                d.child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(t.text_sm)
                        .child(audio),
                )
            })
            .tooltip(self.act_tip("音轨与输出设备 · 点击选择", "cycle_audio"))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_popover(PopoverKind::Audio, cx)));
        let sub_id = ("subs", usize::from(subs) * 2 + usize::from(sub_pop));
        let sub_btn = icon_button(sub_id, Icon::Subtitles, subs || sub_pop, t)
            .tooltip(self.act_tip("字幕 · 点击选轨与偏移", "cycle_sub"))
            .on_click(
                cx.listener(|this, _, _, cx| this.toggle_popover(PopoverKind::Subtitles, cx)),
            );
        let more_btn = icon_button(("more", usize::from(more_pop)), Icon::More, more_pop, t)
            .tooltip(tip("画面比例 / 均衡器 / 旋转 / 截图 / 章节", None))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_popover(PopoverKind::More, cx)));
        let shot_btn = icon_button("screenshot", Icon::Camera, false, t)
            .tooltip(self.act_tip("保存当前帧截图", "take_screenshot"))
            .on_click(cx.listener(|this, _, w, cx| this.take_screenshot(&TakeScreenshot, w, cx)));
        let aspect_id = ("aspect", usize::from(aspect_on));
        let aspect_btn = icon_button(aspect_id, Icon::Aspect, aspect_on, t)
            .tooltip(self.act_tip(aspect_tip, "cycle_aspect"))
            .on_click(cx.listener(|this, _, w, cx| this.cycle_aspect(&CycleAspect, w, cx)));

        hstack(if compact { 1. } else { 2. })
            .children(self.render_variant_selector(t, cx))
            .when(!narrow && show("screenshot"), |d| d.child(shot_btn))
            .when(!narrow && show("loop"), |d| {
                d.child(self.loop_button(state.loop_file, t, cx))
            })
            .when(!narrow && show("aspect"), |d| d.child(aspect_btn))
            .when(show("subtitles"), |d| {
                d.child(anchor_wrap(probes.anchor_sub.clone(), sub_btn))
            })
            .when(!tiny && show("audio"), |d| {
                d.child(anchor_wrap(probes.anchor_audio.clone(), audio_btn))
            })
            .when(show("speed"), |d| {
                d.child(self.speed_button(state.speed, true, t, cx))
            })
            .child(anchor_wrap(probes.anchor_more.clone(), more_btn))
            .child(dock_sep(compact, t))
            .child(self.render_volume(narrow, compact, t, cx))
            .child(dock_sep(compact, t))
    }

    pub(super) fn audio_fields(
        &self,
        narrow: bool,
        compact: bool,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.active().session.state();
        let audio_pop = self.overlays.popover == Some(PopoverKind::Audio);
        let show = |k: &str| !self.is_dock_button_hidden(k);
        let audio_btn = icon_button(("audio", usize::from(audio_pop)), Icon::Audio, audio_pop, t)
            .tooltip(self.act_tip("音轨与输出设备", "cycle_audio"))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_popover(PopoverKind::Audio, cx)));

        let audio_slot = self.chrome.probes.anchor_audio.clone();
        hstack(if compact { 1. } else { 2. })
            .children(self.render_variant_selector(t, cx))
            .when(show("loop"), |d| {
                d.child(self.loop_button(state.loop_file, t, cx))
            })
            .when(show("audio"), |d| {
                d.child(anchor_wrap(audio_slot, audio_btn))
            })
            .when(show("speed"), |d| {
                d.child(self.speed_button(state.speed, false, t, cx))
            })
            .child(dock_sep(compact, t))
            .child(self.render_volume(narrow, compact, t, cx))
            .child(dock_sep(compact, t))
    }

    pub(super) fn photo_transport(
        &self,
        compact: bool,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let session = &self.active().session;
        let standalone_remote = self.catalog.current_playable_number.is_none()
            && session.current().is_none()
            && self.catalog.active_source.is_local()
            && self.catalog.filter != iris_core::catalog::MediaFilter::FavoritesOnly;
        let (pos_str, total_str) = if standalone_remote {
            if !self.queue.is_empty() {
                let pos = session
                    .current_media_uri()
                    .and_then(|u| {
                        (0..self.queue.len())
                            .find(|&i| self.queue.get(i).is_some_and(|q| q.uri == u))
                    })
                    .map_or_else(|| "—".to_string(), |i| (i + 1).to_string());
                (pos, self.queue.len().to_string())
            } else if session.current_uri().is_some() {
                ("1".to_string(), "1".to_string())
            } else {
                ("—".to_string(), "0".to_string())
            }
        } else {
            let pos = self
                .catalog
                .current_playable_number
                .map_or_else(|| "—".to_string(), |number| number.to_string());
            (pos, self.catalog.playable_count.to_string())
        };
        let counter = hstack(if compact { 4. } else { 6. })
            .pl(px(if compact { 6. } else { 10. }))
            .font_family(FONT_MONO)
            .text_size(t.text_md)
            .child(div().text_color(t.text).child(pos_str))
            .child(div().text_color(t.text_faint).child("/"))
            .child(div().text_color(t.text_dim).child(total_str));
        hstack(if compact { 1. } else { 2. })
            .child(self.step_button("prev", -1, t, cx))
            .child(self.step_button("next", 1, t, cx))
            .child(counter)
    }

    pub(super) fn photo_fields(
        &self,
        narrow: bool,
        compact: bool,
        t: ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = &self.active().image;
        let state = self.active().session.state();
        let strip_on = self.settings.photo_filmstrip;
        let more_pop = self.overlays.popover == Some(PopoverKind::More);
        let (viewport, media) = self.image_geometry();
        let pct_str = (viewport[0] > 0. && state.video_size.0 > 0)
            .then(|| view.pixel_scale(viewport, media, window.scale_factor() as f64) * 100.)
            .map_or_else(|| "—".to_string(), |p| format!("{p:.0}%"));
        let strip_tip = if strip_on {
            "隐藏缩略图条"
        } else {
            "显示缩略图条"
        };
        let more_btn = icon_button(("more", usize::from(more_pop)), Icon::More, more_pop, t)
            .tooltip(tip("画面均衡器 / 镜像 / 详情", None))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_popover(PopoverKind::More, cx)));
        let strip_id = ("filmstrip", usize::from(strip_on));
        let strip_btn = icon_button(strip_id, Icon::Filmstrip, strip_on, t)
            .tooltip(self.act_tip(strip_tip, "toggle_filmstrip"))
            .on_click(cx.listener(|this, _, w, cx| this.toggle_filmstrip(&ToggleFilmstrip, w, cx)));
        let (hflip, _) = self.active().session.display_flips();
        let flip_btn = icon_button(("hflip", usize::from(hflip)), Icon::FlipH, hflip, t)
            .tooltip(self.act_tip("水平镜像翻转", "toggle_hflip"))
            .on_click(cx.listener(|this, _, w, cx| this.toggle_hflip(&ToggleHFlip, w, cx)));
        let zoom_readout = div()
            .w(px(48.))
            .flex()
            .justify_center()
            .font_family(FONT_MONO)
            .text_size(t.text_md)
            .text_color(t.text_dim)
            .child(pct_str);

        let fit_on = !view.zoomed();
        let at_min_zoom = view.zoom <= -3.99;
        let at_max_zoom = view.zoom >= 5.99;
        let more_slot = self.chrome.probes.anchor_more.clone();
        let zoom_out_btn = if at_min_zoom {
            icon_button_disabled("zoom-out", Icon::Minus, false, t)
                .tooltip(tip("已达最小缩放比例", None))
        } else {
            icon_button("zoom-out", Icon::Minus, false, t)
                .tooltip(self.act_tip("缩小", "zoom_out"))
                .on_click(cx.listener(|this, _, w, cx| this.zoom_out(&ZoomOut, w, cx)))
        };
        let zoom_in_btn = if at_max_zoom {
            icon_button_disabled("zoom-in", Icon::Plus, false, t)
                .tooltip(tip("已达最大缩放比例", None))
        } else {
            icon_button("zoom-in", Icon::Plus, false, t)
                .tooltip(self.act_tip("放大", "zoom_in"))
                .on_click(cx.listener(|this, _, w, cx| this.zoom_in(&ZoomIn, w, cx)))
        };
        let fit_btn = if fit_on {
            icon_button_disabled(("fit", 1usize), Icon::Fit, true, t)
                .tooltip(tip("当前已适应窗口", None))
        } else {
            icon_button(("fit", 0usize), Icon::Fit, false, t)
                .tooltip(self.act_tip("适应窗口", "fit_image"))
                .on_click(cx.listener(|this, _, w, cx| this.fit_image(&FitImage, w, cx)))
        };
        hstack(if compact { 1. } else { 2. })
            .when(!narrow, |d| d.child(strip_btn).child(flip_btn))
            .child(
                icon_button("rotate", Icon::Rotate, false, t)
                    .tooltip(self.act_tip("顺时针旋转 90°", "rotate_cw"))
                    .on_click(cx.listener(|this, _, w, cx| this.rotate_cw(&RotateCw, w, cx))),
            )
            .child(anchor_wrap(more_slot, more_btn))
            .child(dock_sep(compact, t))
            .child(zoom_out_btn)
            .when(!compact, |d| d.child(zoom_readout))
            .child(zoom_in_btn)
            .child(fit_btn)
            .when(!narrow && !compact, |d| {
                d.child(
                    text_button("actual", "1:1", t)
                        .tooltip(self.act_tip("原始大小", "toggle_image_size"))
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.toggle_image_size(&ToggleImageSize, w, cx)
                        })),
                )
            })
            .child(dock_sep(compact, t))
    }
}
