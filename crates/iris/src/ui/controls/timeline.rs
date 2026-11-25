//! Scrubbable timeline: live stream indicator, buffered segment,
//! chapter tick marks, and hover timestamp/chapter preview bubble.

use std::time::Instant;

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, canvas, div, prelude::FluentBuilder, px,
    relative,
};
use iris_core::player::format_time;

use super::super::IrisWindow;
use crate::theme::{FONT_MONO, FONT_UI, ThemeTokens};

/// Height of the timeline's hit area, which straddles the bar's top edge.
const SEAM_HIT: f32 = 14.;
/// How much of that hit area lies above the bar, over the picture.
const SEAM_ABOVE: f32 = 6.;

impl IrisWindow {
    pub(super) fn render_timeline(
        &self,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let session = &self.active().session;
        let state = session.state();
        let slot = self.chrome.probes.progress_bounds.clone();

        if state.is_live {
            let mut live_tint = t.accent;
            live_tint.a = 0.45;
            return div()
                .id("timeline-live")
                .absolute()
                .top(px(-SEAM_ABOVE))
                .left_0()
                .right_0()
                .h(px(SEAM_HIT))
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
                        .h(px(2.))
                        .bg(t.line),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(SEAM_ABOVE))
                        .left_0()
                        .right_0()
                        .h(px(2.))
                        .bg(live_tint),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(SEAM_ABOVE))
                        .right_0()
                        .w(px(8.))
                        .h(px(2.))
                        .bg(t.accent),
                )
                .into_any_element();
        }

        let duration = state.duration;
        let pct = if let Some(r) = self.chrome.scrub.seek_preview_ratio {
            r.clamp(0., 1.)
        } else if duration > 0. {
            (state.time_pos / duration).clamp(0., 1.) as f32
        } else {
            0.
        };
        let buf_pct = if duration > 0. && state.cache_duration > 0. {
            ((state.time_pos + state.cache_duration) / duration).clamp(0., 1.) as f32
        } else {
            0.
        };
        let active = self.chrome.scrub.timeline_hover || self.chrome.scrub.seeking;
        let track = if active { 4. } else { 2. };

        let ab_marks: Vec<AnyElement> = [state.ab_start, state.ab_end]
            .into_iter()
            .flatten()
            .filter(|time| duration > 0. && *time <= duration)
            .map(|time| {
                div()
                    .absolute()
                    .top(px(SEAM_ABOVE - 3.))
                    .left(relative((time / duration) as f32))
                    .w(px(3.))
                    .h(px(track + 6.))
                    .bg(t.text)
                    .into_any_element()
            })
            .collect();
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
                        .bg(t.window_bg)
                        .into_any_element()
                })
                .collect()
        } else {
            Vec::new()
        };

        let annotation_marks: Vec<AnyElement> = session
            .current_media_uri()
            .filter(|_| session.may_share_current_identity() && duration > 0.)
            .and_then(|uri| self.store.annotations(&uri))
            .into_iter()
            .flat_map(|a| a.markers.values())
            .filter(|m| m.position_ms as f64 / 1000. <= duration)
            .map(|marker| {
                div()
                    .absolute()
                    .top(px(SEAM_ABOVE - 2.))
                    .left(relative(
                        (marker.position_ms as f64 / 1000. / duration) as f32,
                    ))
                    .w(px(2.))
                    .h(px(track + 4.))
                    .bg(t.accent)
                    .into_any_element()
            })
            .collect();
        let label = active
            .then(|| {
                let b = self.chrome.probes.progress_bounds.get()?;
                let p = self.chrome.mouse_pos?;
                let width = f32::from(b.size.width);
                if width <= 0. || duration <= 0. {
                    return None;
                }
                let x = f32::from(p.x - b.origin.x).clamp(0., width);
                let hover_t = x as f64 / width as f64 * duration;
                let chapter_title = session
                    .chapter_index_at(hover_t)
                    .map(|(ix, c)| super::super::strings::chrome::chapter_title(ix, &c.title));
                let max_box: f32 = if chapter_title.is_some() { 220. } else { 84. };
                let box_w = max_box.min((width - 8.).max(64.));
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
                                .min_h(t.control_h_sm)
                                .max_w(px(box_w))
                                .px(px(6.))
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .rounded(t.radius_md)
                                .bg(t.panel)
                                .border_1()
                                .border_color(t.line_strong)
                                .font_family(FONT_MONO)
                                .text_size(t.text_sm)
                                .text_color(t.text)
                                .child(div().flex_none().child(format_time(hover_t)))
                                .when_some(chapter_title, |d, ch| {
                                    d.child(
                                        div()
                                            .min_w(px(0.))
                                            .font_family(FONT_UI)
                                            .text_size(t.text_xs)
                                            .text_color(t.text_dim)
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
                this.chrome.scrub.timeline_hover = *hovered;
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    if this.active().session.state().duration <= 0. {
                        return;
                    }
                    this.chrome.scrub.seek_preview_timeout = None;
                    this.chrome.scrub.seeking = true;
                    this.chrome.scrub.seek_dragged = false;
                    this.chrome.scrub.last_seek_drag = Instant::now();
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
                    .bg(t.line),
            )
            .when(buf_pct > 0., |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(SEAM_ABOVE))
                        .left_0()
                        .w(relative(buf_pct))
                        .h(px(track))
                        .bg(t.line_strong),
                )
            })
            .child(
                div()
                    .absolute()
                    .top(px(SEAM_ABOVE))
                    .left_0()
                    .w(relative(pct))
                    .h(px(track))
                    .bg(t.accent),
            )
            .children(chapter_marks)
            .children(self.plugin_timeline_ranges(duration, t, cx))
            .children(annotation_marks)
            .children(ab_marks)
            .child(
                div()
                    .absolute()
                    .top(px(SEAM_ABOVE - 3.))
                    .left(relative(pct))
                    .ml(px(-1.))
                    .w(px(2.))
                    .h(px(track + 6.))
                    .bg(t.accent),
            )
            .children(label)
            .into_any_element()
    }
}
