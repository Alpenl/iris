use super::super::{
    AppModel,
    plugin_ui::overlay::{render_overlay_nodes, resolve_overlay_color},
};
use crate::theme::ThemeTokens;
use gpui::{
    AnyElement, App, ContentMask, Context, Entity, FontWeight, IntoElement, ParentElement, Rgba,
    ShapedLine, SharedString, Styled, TextRun, Window, canvas, div, point, px, relative,
};
use iris_api::v2::{
    Capability, HostEvent,
    enhance::{CueContent, CueLayout, OverlayWindow},
};
use iris_plugin_host::{
    ledger::{Ledger, Reservation, Resource},
    overlay::OverlayFrame,
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::Arc,
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
    _lease: Reservation,
}

#[derive(Clone, Default)]
pub(super) struct GlyphCache(Rc<RefCell<BTreeMap<Key, Glyph>>>);

impl GlyphCache {
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }
}

impl AppModel {
    pub(in crate::ui) fn reset_timed_overlays(&mut self) {
        self.plugin_view.enhance.bank = None;
        self.plugin_view.enhance.announced.clear();
        self.plugin_view.enhance.frame = None;
        self.plugin_view.enhance.glyphs.clear();
        self.plugin_view.enhance.wake = None;
    }

    pub(in crate::ui) fn sync_overlay_clock(&mut self) {
        let state = self.active().session.state();
        let values = (
            state.time_pos,
            state.speed,
            state.paused || state.ended || !state.loaded,
            state.buffering,
            state.duration,
        );
        self.plugin_view.enhance.clock.update(
            values.0,
            values.1,
            values.2,
            values.3,
            values.4,
            Instant::now(),
        );
        self.plugin_view.enhance.wake = None;
    }

    pub(in crate::ui) fn reconcile_enhancements(&mut self, cx: &mut Context<Self>) {
        let dead: Vec<_> = self
            .plugin_view
            .enhance
            .bindings
            .iter()
            .filter(|(_, binding)| !binding.is_live())
            .map(|(id, _)| id.clone())
            .collect();
        let mut revoked_any = false;
        for owner in dead {
            self.plugin_view
                .enhance
                .annotations
                .selections
                .remove(&owner);
            self.active_mut().session.revoke_plugin_tracks(&owner);
            if self
                .plugin_view
                .enhance
                .timeline
                .as_ref()
                .is_some_and(|(id, _, _)| id == &owner)
            {
                self.plugin_view.enhance.timeline = None;
                self.active_mut().session.flush_deferred();
            }
            if self.plugin_view.enhance.lut_owner.as_deref() == Some(&owner) {
                let _ = self.active_mut().session.set_monitor_lut(None);
                self.plugin_view.enhance.lut_owner = None;
            }
            self.plugin_view
                .enhance
                .pending
                .retain(|(id, _), _| *id != owner);
            if let Some(bank) = &self.plugin_view.enhance.bank {
                let Ok(mut bank) = bank.try_lock() else {
                    continue;
                };
                bank.revoke(&owner);
            }
            self.plugin_view.enhance.bindings.remove(&owner);
            self.plugin_view
                .enhance
                .glyphs
                .0
                .borrow_mut()
                .retain(|(id, _, _), _| *id != owner);
            revoked_any = true;
        }
        let overlays: Vec<_> = self
            .plugins
            .list()
            .iter()
            .filter_map(|p| {
                p.binding
                    .as_ref()
                    .filter(|b| {
                        b.is_live()
                            && b.installed.grants.contains(&Capability::UiOverlay)
                            && self.plugins.client().is_ready(&p.id)
                    })
                    .cloned()
            })
            .collect();
        for binding in overlays {
            let owner = binding.installed.installation_id.clone();
            let Ok(access) = self.plugins.client().access(&binding.installed.plugin_id) else {
                continue;
            };
            self.ensure_overlay_bank(&access);
            self.plugin_view
                .enhance
                .bindings
                .insert(owner, binding.clone());
        }
        self.update_overlay_window(cx);
        if revoked_any {
            self.step_timed_overlay_frame(cx);
            self.notify_viewport_and_dock(cx);
        }
    }

    fn notify_overlay_window_changed(
        &mut self,
        changed: bool,
        window: OverlayWindow,
        cx: &mut Context<Self>,
    ) {
        let generation = self.plugin_view.enhance.overlay_generation;
        let recipients: Vec<_> = self
            .plugin_view
            .enhance
            .bindings
            .values()
            .filter(|b| {
                b.is_live()
                    && b.installed.grants.contains(&Capability::UiOverlay)
                    && (changed
                        || self
                            .plugin_view
                            .enhance
                            .announced
                            .get(&b.installed.installation_id)
                            != Some(&generation))
            })
            .map(|b| {
                (
                    b.installed.plugin_id.clone(),
                    b.installed.installation_id.clone(),
                )
            })
            .collect();
        for (plugin, owner) in recipients {
            let report = self.plugins.send_to(
                &plugin,
                HostEvent::OverlayWindowChanged {
                    media: self.plugin_media_id(),
                    generation,
                    window: window.clone(),
                },
            );
            if report.notices.is_empty() {
                self.plugin_view.enhance.announced.insert(owner, generation);
            }
            let regions = self.apply_poll_report(report, None, cx);
            if regions.any() {
                self.notify_regions(regions, cx);
            }
        }
    }

    pub(in crate::ui) fn update_overlay_window(&mut self, cx: &mut Context<Self>) {
        if !self.active().session.state().loaded {
            return;
        }
        let Some(bank) = self.plugin_view.enhance.bank.as_ref() else {
            return;
        };
        let position = self.plugin_view.enhance.clock.position_ms(Instant::now());
        let (changed, window) = {
            let Ok(mut bank) = bank.try_lock() else {
                return;
            };
            let changed = bank
                .advance(
                    self.plugin_media_id(),
                    self.plugin_view.enhance.overlay_generation,
                    position,
                )
                .unwrap_or(false);
            (changed, bank.window().clone())
        };
        self.notify_overlay_window_changed(changed, window, cx);
    }

    /// Advances timed overlay cues outside `render()`, scheduling wake timers and notifying
    /// `ViewportView` only when the active cue frame changes.
    pub(in crate::ui) fn step_timed_overlay_frame(&mut self, cx: &mut Context<Self>) {
        if !self.active().session.state().loaded {
            if self.plugin_view.enhance.frame.take().is_some() {
                self.notify_viewport(cx);
            }
            return;
        }
        let Some(bank) = self.plugin_view.enhance.bank.clone() else {
            if self.plugin_view.enhance.frame.take().is_some() {
                self.notify_viewport(cx);
            }
            return;
        };
        let now = Instant::now();
        let clock = &self.plugin_view.enhance.clock;
        let (position, paused, speed) = (
            clock.position_ms(now),
            clock.effectively_paused(now),
            clock.speed(),
        );
        let generation = self.plugin_view.enhance.overlay_generation;
        let has_unannounced = self.plugin_view.enhance.bindings.values().any(|b| {
            b.is_live()
                && b.installed.grants.contains(&Capability::UiOverlay)
                && self
                    .plugin_view
                    .enhance
                    .announced
                    .get(&b.installed.installation_id)
                    != Some(&generation)
        });
        let (frame, window_changed) = {
            let Ok(mut bank) = bank.try_lock() else {
                if !paused && self.plugin_view.enhance.wake.is_none() {
                    let timer = cx.background_executor().timer(Duration::from_millis(16));
                    self.plugin_view.enhance.wake = Some(cx.spawn(async move |this, cx| {
                        timer.await;
                        let _ = this.update(cx, |this, cx| {
                            this.plugin_view.enhance.wake = None;
                            this.step_timed_overlay_frame(cx);
                        });
                    }));
                }
                return;
            };
            let changed = bank
                .advance(self.plugin_media_id(), generation, position)
                .unwrap_or(false);
            let window_changed =
                (changed || has_unannounced).then(|| (changed, bank.window().clone()));
            let frame = (!bank.is_empty()).then(|| bank.frame(position, paused, speed));
            (frame, window_changed)
        };
        if let Some((changed, next_window)) = window_changed {
            self.notify_overlay_window_changed(changed, next_window, cx);
        }
        let Some(Ok(mut frame)) = frame else {
            if self.plugin_view.enhance.frame.take().is_some() {
                self.notify_viewport(cx);
            }
            return;
        };
        frame.cues.retain(|cue| {
            self.plugin_view
                .enhance
                .bindings
                .get(&cue.owner)
                .is_some_and(|b| b.is_live())
        });
        if !frame.animate
            && self.plugin_view.enhance.wake.is_none()
            && let Some(delay) = frame.next_change
        {
            let timer = cx
                .background_executor()
                .timer(delay.max(Duration::from_millis(1)));
            self.plugin_view.enhance.wake = Some(cx.spawn(async move |this, cx| {
                timer.await;
                let _ = this.update(cx, |this, cx| {
                    this.plugin_view.enhance.wake = None;
                    this.step_timed_overlay_frame(cx);
                });
            }));
        }
        let had_visible = self
            .plugin_view
            .enhance
            .frame
            .as_ref()
            .is_some_and(|prev| !prev.cues.is_empty());
        let has_visible = !frame.cues.is_empty();
        self.plugin_view.enhance.frame = Some(Arc::new(frame));
        if had_visible || has_visible {
            self.notify_viewport(cx);
        }
    }
}

pub(in crate::ui) fn render_timed_overlay(
    model: &Entity<AppModel>,
    window: &Window,
    cx: &App,
) -> Option<AnyElement> {
    let m = model.read(cx);
    let frame = m.plugin_view.enhance.frame.as_ref()?.clone();
    if frame.cues.is_empty() {
        return None;
    }
    if frame.animate {
        let weak = model.downgrade();
        window.on_next_frame(move |_, cx| {
            let _ = weak.update(cx, |this, cx| this.step_timed_overlay_frame(cx));
        });
    }
    let ledger = m.plugin_view.enhance.ledger.clone()?;
    let cache = m.plugin_view.enhance.glyphs.clone();
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
                    move |_, window, _| shape_frame(&frame, &cache, &ledger, t, window),
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
    ledger: &Ledger,
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
            let Ok(lease) = ledger.reserve(&cue.owner, &[(Resource::HostBytes, 32768)]) else {
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
