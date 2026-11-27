//! Scrubbable timeline: live stream indicator, buffered segment,
//! chapter tick marks, and hover timestamp/chapter preview bubble.

use std::time::Instant;

use gpui::{
    AnyElement, App, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, canvas, div, prelude::FluentBuilder, px,
    relative,
};
use iris_core::player::format_time;

use super::super::AppModel;
use super::super::views::model_listener;
use crate::theme::{FONT_MONO, FONT_UI, ThemeTokens};

/// Height of the timeline's hit area, which straddles the bar's top edge.
const SEAM_HIT: f32 = 14.;
/// How much of that hit area lies above the bar, over the picture.
const SEAM_ABOVE: f32 = 6.;

fn timeline_ratios(
    preview_ratio: Option<f32>,
    time_pos: f64,
    cache_duration: f64,
    duration: f64,
) -> (f32, f32) {
    let valid_dur = duration.is_finite() && duration > 0.;
    let pct = if let Some(r) = preview_ratio.filter(|r| r.is_finite()) {
        r.clamp(0., 1.)
    } else if valid_dur && time_pos.is_finite() {
        (time_pos / duration).clamp(0., 1.) as f32
    } else {
        0.
    };
    let buf_pct =
        if valid_dur && time_pos.is_finite() && cache_duration.is_finite() && cache_duration > 0. {
            ((time_pos + cache_duration) / duration).clamp(0., 1.) as f32
        } else {
            0.
        };
    (pct, buf_pct)
}

pub(super) fn render_timeline(
    model: &Entity<AppModel>,
    t: ThemeTokens,
    cx: &App,
) -> impl IntoElement {
    let m = model.read(cx);
    let session = &m.active().session;
    let state = session.state();
    let slot = m.chrome.probes.progress_bounds.clone();

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
    let valid_dur = duration.is_finite() && duration > 0.;
    let (pct, buf_pct) = timeline_ratios(
        m.chrome.scrub.seek_preview_ratio,
        state.time_pos,
        state.cache_duration,
        duration,
    );
    let active = m.chrome.scrub.timeline_hover || m.chrome.scrub.seeking;
    let track = if active { 4. } else { 2. };

    let ab_marks: Vec<AnyElement> = [state.ab_start, state.ab_end]
        .into_iter()
        .flatten()
        .filter(|time| valid_dur && time.is_finite() && *time >= 0. && *time <= duration)
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
    let chapter_marks: Vec<AnyElement> = if valid_dur {
        state
            .chapters
            .iter()
            .filter(|c| c.time.is_finite() && c.time > 0.5 && c.time < duration - 0.5)
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
        .filter(|_| session.may_share_current_identity() && valid_dur)
        .and_then(|uri| m.store.annotations(&uri))
        .into_iter()
        .flat_map(|a| a.markers.values())
        .filter(|marker| marker.position_ms as f64 / 1000. <= duration)
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
            let b = m.chrome.probes.progress_bounds.get()?;
            let p = m.chrome.mouse_pos?;
            let width = f32::from(b.size.width);
            if !width.is_finite() || width <= 0. || !valid_dur {
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

    let plugin_ranges =
        super::super::plugin_runtime::timeline::plugin_timeline_ranges(model, duration, t, cx);

    div()
        .id("timeline")
        .absolute()
        .top(px(-SEAM_ABOVE))
        .left_0()
        .right_0()
        .h(px(SEAM_HIT))
        .cursor_pointer()
        .on_hover(model_listener(model, |this, hovered: &bool, _, cx| {
            this.chrome.scrub.timeline_hover = *hovered;
            this.notify_dock(cx);
        }))
        .on_mouse_down(
            MouseButton::Left,
            model_listener(model, |this, ev: &MouseDownEvent, _, cx| {
                let duration = this.active().session.state().duration;
                if !duration.is_finite() || duration <= 0. {
                    return;
                }
                this.chrome.scrub.seek_preview_timeout = None;
                this.chrome.scrub.seeking = true;
                this.chrome.scrub.seek_dragged = false;
                this.chrome.scrub.last_seek_drag = Instant::now();
                this.update_seek_preview(ev.position.x);
                this.seek_to_x(ev.position.x, true);
                cx.stop_propagation();
                this.notify_dock(cx);
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
        .children(plugin_ranges)
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

#[cfg(test)]
mod tests {
    use super::timeline_ratios;

    #[test]
    fn timeline_ratios_guard_non_finite_inputs_before_clamping() {
        assert_eq!(timeline_ratios(Some(f32::NAN), 10., 5., 100.), (0.1, 0.15));
        assert_eq!(timeline_ratios(None, f64::NAN, 5., 100.), (0., 0.));
        assert_eq!(timeline_ratios(None, 10., f64::INFINITY, 100.), (0.1, 0.));
        assert_eq!(timeline_ratios(None, 10., 5., f64::INFINITY), (0., 0.));
        assert_eq!(timeline_ratios(None, 10., 5., f64::NAN), (0., 0.));
        assert_eq!(timeline_ratios(Some(1.5), 10., 5., 100.), (1., 0.15));
    }
}
