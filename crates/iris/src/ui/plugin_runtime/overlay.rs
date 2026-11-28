use super::super::{
    WindowShell,
    plugin_ui::overlay::{render_overlay_nodes, resolve_overlay_color},
};
use crate::theme::ThemeTokens;
use gpui::{
    AnyElement, App, ContentMask, Context, Entity, FontWeight, IntoElement, ParentElement, Rgba,
    ShapedLine, SharedString, Styled, TextRun, Window, canvas, div, point, px, relative,
};
use iris_api::v2::enhance::{CueContent, CueLayout};
use iris_plugin_host::{OverlayFrame, OverlayGlyphLease};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Instant};

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

    pub fn clear_owner(&self, owner: &str) {
        self.0.borrow_mut().retain(|(id, _, _), _| id != owner);
    }
}

impl WindowShell {
    pub(in crate::ui) fn sync_overlay_clock(&mut self) {
        let _ = self.app.sync_overlay_clock(Instant::now());
        self.plugin_ui.overlay_wake = None;
    }

    pub(in crate::ui) fn reconcile_enhancements(&mut self, cx: &mut Context<Self>) {
        let out = self.app.reconcile_plugin_enhancements(Instant::now());
        let regions = self.apply_plugin_outcome(out, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }

    pub(in crate::ui) fn update_overlay_window(&mut self, cx: &mut Context<Self>) {
        let out = self.app.update_overlay_window(Instant::now());
        let regions = self.apply_plugin_outcome(out, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }

    /// Advances timed overlay cues outside `render()`, scheduling wake timers and notifying
    /// `ViewportView` only when the active cue frame changes.
    pub(in crate::ui) fn step_timed_overlay_frame(&mut self, cx: &mut Context<Self>) {
        let out = self.app.step_timed_overlay_frame(Instant::now());
        let regions = self.apply_plugin_outcome(out, None, cx);
        if regions.any() {
            self.notify_regions(regions, cx);
        }
    }
}

pub(in crate::ui) fn render_timed_overlay(
    model: &Entity<WindowShell>,
    window: &Window,
    cx: &App,
) -> Option<AnyElement> {
    let m = model.read(cx);
    let frame = m.plugins().overlay_frame()?.clone();
    if frame.cues.is_empty() {
        return None;
    }
    if frame.animate {
        let weak = model.downgrade();
        window.on_next_frame(move |_, cx| {
            let _ = weak.update(cx, |this, cx| this.step_timed_overlay_frame(cx));
        });
    }
    let cache = m.plugin_ui.glyphs.clone();
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
