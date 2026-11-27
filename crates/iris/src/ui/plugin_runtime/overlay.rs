use super::super::{
    AppModel,
    plugin_ui::overlay::{render_overlay_nodes, resolve_overlay_color},
};
use crate::theme::ThemeTokens;
use gpui::{
    AnyElement, App, ContentMask, Context, Entity, FontWeight, IntoElement, ParentElement, Rgba,
    ShapedLine, SharedString, Styled, TextRun, Window, canvas, div, point, px, relative,
};
use iris_api::v2::enhance::{CueContent, CueLayout};
use iris_plugin_host::{
    overlay::{OverlayFrame, OverlayGlyphLease},
    ui::OverlayStep,
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    time::{Duration, Instant},
};

type Key = (String, String, String);

struct Glyph {
    text: String,
    size: u16,
    bold: bool,
    color: Rgba,
    scale: f32,
    shaped: ShapedLine,
    _lease: OverlayGlyphLease,
}

#[derive(Clone, Default)]
pub(in crate::ui) struct GlyphCache(Rc<RefCell<BTreeMap<Key, Glyph>>>);

impl GlyphCache {
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }
}

impl AppModel {
    pub(in crate::ui) fn reset_timed_overlays(&mut self) {
        self.plugins.reset_timed_overlays();
        self.plugin_view.overlay_frame = None;
        self.plugin_view.glyphs.clear();
        self.plugin_view.overlay_wake = None;
    }

    pub(in crate::ui) fn sync_overlay_clock(&mut self) {
        let (time_pos, speed, paused, buffering, duration) = {
            let state = self.active().session.state();
            (
                state.time_pos,
                state.speed,
                state.paused || state.ended || !state.loaded,
                state.buffering,
                state.duration,
            )
        };
        self.plugins.update_overlay_clock(
            time_pos,
            speed,
            paused,
            buffering,
            duration,
            Instant::now(),
        );
        self.plugin_view.overlay_wake = None;
    }

    pub(in crate::ui) fn reconcile_enhancements(&mut self, cx: &mut Context<Self>) {
        let (media_loaded, position_ms) = {
            let state = self.active().session.state();
            (state.loaded, (state.time_pos.max(0.) * 1000.) as u64)
        };
        let (revoked, reports) =
            self.plugins
                .reconcile_enhancements(media_loaded, position_ms, Instant::now());
        let revoked_any = !revoked.revoked_owners.is_empty();
        for owner in &revoked.revoked_owners {
            self.active_mut().session.revoke_plugin_tracks(owner);
            self.plugin_view
                .glyphs
                .0
                .borrow_mut()
                .retain(|(id, _, _), _| id != owner);
        }
        if revoked.clear_timeline {
            self.active_mut().session.flush_deferred();
        }
        if revoked.clear_lut {
            let _ = self.active_mut().session.set_monitor_lut(None);
        }
        for report in reports {
            let regions = self.apply_poll_report(report, None, cx);
            if regions.any() {
                self.notify_regions(regions, cx);
            }
        }
        if revoked_any {
            self.step_timed_overlay_frame(cx);
            self.notify_viewport_and_dock(cx);
        }
    }

    pub(in crate::ui) fn update_overlay_window(&mut self, cx: &mut Context<Self>) {
        let media_loaded = self.active().session.state().loaded;
        for report in self
            .plugins
            .update_overlay_window(media_loaded, Instant::now())
        {
            let regions = self.apply_poll_report(report, None, cx);
            if regions.any() {
                self.notify_regions(regions, cx);
            }
        }
    }

    /// Advances timed overlay cues outside `render()`, scheduling wake timers and notifying
    /// `ViewportView` only when the active cue frame changes.
    pub(in crate::ui) fn step_timed_overlay_frame(&mut self, cx: &mut Context<Self>) {
        let media_loaded = self.active().session.state().loaded;
        match self
            .plugins
            .step_overlay_frame(media_loaded, Instant::now())
        {
            OverlayStep::Empty => {
                if self.plugin_view.overlay_frame.take().is_some() {
                    self.notify_viewport(cx);
                }
            }
            OverlayStep::Busy { retry_after } => {
                if let Some(delay) = retry_after
                    && self.plugin_view.overlay_wake.is_none()
                {
                    let timer = cx.background_executor().timer(delay);
                    self.plugin_view.overlay_wake = Some(cx.spawn(async move |this, cx| {
                        timer.await;
                        let _ = this.update(cx, |this, cx| {
                            this.plugin_view.overlay_wake = None;
                            this.step_timed_overlay_frame(cx);
                        });
                    }));
                }
            }
            OverlayStep::Updated { frame, reports } => {
                for report in reports {
                    let regions = self.apply_poll_report(report, None, cx);
                    if regions.any() {
                        self.notify_regions(regions, cx);
                    }
                }
                let Some(frame) = frame else {
                    if self.plugin_view.overlay_frame.take().is_some() {
                        self.notify_viewport(cx);
                    }
                    return;
                };
                if !frame.animate
                    && self.plugin_view.overlay_wake.is_none()
                    && let Some(delay) = frame.next_change
                {
                    let timer = cx
                        .background_executor()
                        .timer(delay.max(Duration::from_millis(1)));
                    self.plugin_view.overlay_wake = Some(cx.spawn(async move |this, cx| {
                        timer.await;
                        let _ = this.update(cx, |this, cx| {
                            this.plugin_view.overlay_wake = None;
                            this.step_timed_overlay_frame(cx);
                        });
                    }));
                }
                let had_visible = self
                    .plugin_view
                    .overlay_frame
                    .as_ref()
                    .is_some_and(|prev| !prev.cues.is_empty());
                let has_visible = !frame.cues.is_empty();
                self.plugin_view.overlay_frame = Some(frame);
                if had_visible || has_visible {
                    self.notify_viewport(cx);
                }
            }
        }
    }
}

pub(in crate::ui) fn render_timed_overlay(
    model: &Entity<AppModel>,
    window: &Window,
    cx: &App,
) -> Option<AnyElement> {
    let m = model.read(cx);
    let frame = m.plugin_view.overlay_frame.as_ref()?.clone();
    if frame.cues.is_empty() {
        return None;
    }
    if frame.animate {
        let weak = model.downgrade();
        window.on_next_frame(move |_, cx| {
            let _ = weak.update(cx, |this, cx| this.step_timed_overlay_frame(cx));
        });
    }
    let cache = m.plugin_view.glyphs.clone();
    let t = ThemeTokens::of(cx);
    let mut vectors = Vec::new();
    for cue in &frame.cues {
        if let CueContent::Vector { nodes } = &cue.cue.content {
            vectors.push(
                div()
                    .absolute()
                    .left(relative(cue.x))
                    .top(relative(cue.y))
                    .size_full()
                    .children(render_overlay_nodes(
                        model,
                        nodes.iter().map(|n| ("", n)),
                        cx,
                    ))
                    .into_any_element(),
            );
        }
    }
    let painting = frame.clone();
    Some(
        div()
            .absolute()
            .inset_0()
            .overflow_hidden()
            .children(vectors)
            .child(
                canvas(
                    move |_, window, _| shape_frame(&frame, &cache, t, window),
                    move |bounds, shaped, window, cx| {
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            for (index, line, line_height) in shaped {
                                let cue = &painting.cues[index];
                                let left = if matches!(cue.cue.layout, CueLayout::Scroll) {
                                    bounds.size.width * (1. - cue.progress)
                                        - line.width * cue.progress
                                } else {
                                    bounds.size.width * cue.x - line.width * 0.5
                                };
                                let point = point(
                                    bounds.origin.x + left,
                                    bounds.origin.y + bounds.size.height * cue.y,
                                );
                                let _ = line.paint(point, line_height, window, cx);
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
            .into_any_element(),
    )
}

fn shape_frame(
    frame: &OverlayFrame,
    cache: &GlyphCache,
    t: ThemeTokens,
    window: &mut Window,
) -> Vec<(usize, ShapedLine, gpui::Pixels)> {
    let mut values = cache.0.borrow_mut();
    let alive: std::collections::BTreeSet<_> = frame
        .cues
        .iter()
        .map(|c| (c.owner.clone(), c.track.clone(), c.cue.id.clone()))
        .collect();
    // Layouts outside the current bounded frame are promptly released; paused
    // or scrolling cues reuse their glyph layout until content/style changes.
    values.retain(|key, _| alive.contains(key));
    let mut shaped = Vec::new();
    for (index, cue) in frame.cues.iter().enumerate() {
        let CueContent::Text {
            text,
            size,
            bold,
            color,
        } = &cue.cue.content
        else {
            continue;
        };
        let text = text.replace(['\r', '\n'], " ");
        let key = (cue.owner.clone(), cue.track.clone(), cue.cue.id.clone());
        let color = resolve_overlay_color(color, 1., t);
        let scale = window.scale_factor();
        if values.get(&key).is_none_or(|g| {
            g.text != text
                || g.size != *size
                || g.bold != *bold
                || g.color != color
                || g.scale != scale
        }) {
            let Ok(lease) = frame.reserve_glyph(&cue.owner) else {
                continue;
            };
            let mut font = window.text_style().font();
            if *bold {
                font.weight = FontWeight::BOLD;
            }
            let line = window.text_system().shape_line(
                SharedString::from(text.clone()),
                px(f32::from(*size)),
                &[TextRun {
                    len: text.len(),
                    font,
                    color: color.into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            );
            values.insert(
                key.clone(),
                Glyph {
                    text,
                    size: *size,
                    bold: *bold,
                    color,
                    scale,
                    shaped: line,
                    _lease: lease,
                },
            );
        }
        if let Some(glyph) = values.get(&key) {
            shaped.push((index, glyph.shaped.clone(), px(f32::from(*size) * 1.35)));
        }
    }
    shaped
}
