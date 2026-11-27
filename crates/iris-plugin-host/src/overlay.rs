//! Windowed cue residency and clock-driven rendering. No per-frame guest calls.
mod clock;
use super::{
    instance::Blob,
    ledger::{Ledger, Reservation, Resource},
    validation,
};
pub use clock::OverlayClock;
use iris_api::v2::{
    ErrorCode, Generation, Id, WireU64,
    enhance::{CueContent, CueLayout, OverlayWindow, TimedCue},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

const MAX_DURATION: u64 = 30_000;
struct Cue {
    value: TimedCue,
    bytes: u64,
}
struct Track {
    cues: BTreeMap<(u64, String), Cue>,
    ids: BTreeSet<String>,
    bytes: u64,
    lease: Reservation,
}
pub struct PaintCue {
    pub owner: String,
    pub track: String,
    pub cue: TimedCue,
    /// Normalized anchor; scroll text starts just outside the right edge.
    pub x: f32,
    pub y: f32,
    pub progress: f32,
}
/// Keep the frame alive while its cloned text/layout is retained by the UI.
pub struct OverlayFrame {
    pub cues: Vec<PaintCue>,
    pub animate: bool,
    pub next_change: Option<Duration>,
    pub omitted: usize,
    _memory: Reservation,
}
pub struct OverlayBank {
    ledger: Ledger,
    media: Id,
    generation: Generation,
    window: OverlayWindow,
    anchor: u64,
    tracks: BTreeMap<(String, String), Track>,
}
impl OverlayBank {
    pub fn new(ledger: Ledger, media: Id, generation: Generation, position_ms: u64) -> Self {
        Self {
            ledger,
            media,
            generation,
            window: window(Id(1), position_ms),
            anchor: position_ms,
            tracks: BTreeMap::new(),
        }
    }
    pub fn window(&self) -> &OverlayWindow {
        &self.window
    }
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }
    /// A changed media/seek clears old submissions. Clock motion moves the
    /// window every 15 seconds; retained overlapping cues keep their identity.
    pub fn advance(
        &mut self,
        media: Id,
        generation: Generation,
        position_ms: u64,
    ) -> Result<bool, ErrorCode> {
        if generation.media < self.generation.media
            || (generation.media == self.generation.media
                && (generation.seek < self.generation.seek
                    || generation.context < self.generation.context))
        {
            return Err(ErrorCode::StaleHandle);
        }
        let reset = media != self.media
            || generation.media != self.generation.media
            || generation.seek != self.generation.seek;
        if !reset && generation == self.generation && self.anchor.abs_diff(position_ms) < 15_000 {
            return Ok(false);
        }
        let revision = Id(self
            .window
            .revision
            .0
            .checked_add(1)
            .ok_or(ErrorCode::QuotaExceeded)?);
        let next = window(revision, position_ms);
        if reset {
            self.tracks.clear();
        } else {
            for track in self.tracks.values_mut() {
                track.cues.retain(|_, cue| {
                    cue.value.start_ms.0 + cue.value.duration_ms.0 > next.start_ms.0
                        && cue.value.start_ms.0 < next.end_ms.0
                });
                track.ids = track
                    .cues
                    .values()
                    .map(|cue| cue.value.id.clone())
                    .collect();
                track.bytes = track.cues.values().map(|cue| cue.bytes).sum();
                track
                    .lease
                    .replace(&charges(track.cues.len(), track.bytes))?;
            }
            self.tracks.retain(|_, track| !track.cues.is_empty());
        }
        self.media = media;
        self.generation = generation;
        self.window = next;
        self.anchor = position_ms;
        Ok(true)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        &mut self,
        owner: &str,
        media: Id,
        generation: Generation,
        name: &str,
        revision: Id,
        blob: &Arc<Blob>,
        append: bool,
    ) -> Result<(), ErrorCode> {
        self.check(media, generation, revision)?;
        validation::identifier(name)?;
        if blob.owner != owner {
            return Err(ErrorCode::Denied);
        }
        let scratch_bytes = ((blob.bytes.len() as u64).saturating_mul(16))
            .clamp(64 << 10, (iris_api::v2::CHUNK_LIMIT * 8) as u64);
        let _scratch = self
            .ledger
            .reserve(owner, &[(Resource::HostBytes, scratch_bytes)])?;
        let values: Vec<TimedCue> =
            iris_core::bounded_json::decode(&blob.bytes, iris_api::v2::CHUNK_LIMIT)
                .map_err(super::json_error)?;
        if values.is_empty() {
            if !append {
                self.tracks.remove(&(owner.into(), name.into()));
            }
            return Ok(());
        }
        let mut added = BTreeMap::new();
        let mut ids = BTreeSet::new();
        let mut bytes = 0;
        for value in values {
            validate(&value)?;
            let end = value
                .start_ms
                .0
                .checked_add(value.duration_ms.0)
                .ok_or(ErrorCode::RangeInvalid)?;
            if value.start_ms < self.window.start_ms || end > self.window.end_ms.0 {
                return Err(ErrorCode::OutOfWindow);
            }
            if !ids.insert(value.id.clone()) {
                return Err(ErrorCode::InvalidArgument);
            }
            let size = estimate_cue_bytes(&value)?;
            bytes += size;
            added.insert(
                (value.start_ms.0, value.id.clone()),
                Cue { value, bytes: size },
            );
        }
        let key = (owner.to_string(), name.to_string());
        let old = self.tracks.get(&key);
        if append && old.is_some_and(|old| ids.iter().any(|id| old.ids.contains(id))) {
            return Err(ErrorCode::InvalidArgument);
        }
        let (count, total) = if append {
            (
                added.len() + old.map_or(0, |t| t.cues.len()),
                bytes + old.map_or(0, |t| t.bytes),
            )
        } else {
            (added.len(), bytes)
        };
        if let Some(old) = self.tracks.get_mut(&key) {
            old.lease.replace(&charges(count, total))?;
            if !append {
                old.cues.clear();
                old.ids.clear();
            }
            old.cues.extend(added);
            old.ids.extend(ids);
            old.bytes = total;
        } else {
            if self.tracks.len() >= 64
                || self.tracks.keys().filter(|(id, _)| id == owner).count() >= 8
            {
                return Err(ErrorCode::QuotaExceeded);
            }
            let lease = self.ledger.reserve(owner, &charges(count, total))?;
            self.tracks.insert(
                key,
                Track {
                    cues: added,
                    ids,
                    bytes: total,
                    lease,
                },
            );
        }
        Ok(())
    }
    fn check(&self, media: Id, generation: Generation, revision: Id) -> Result<(), ErrorCode> {
        if media != self.media || generation != self.generation || revision != self.window.revision
        {
            return Err(ErrorCode::StaleHandle);
        }
        Ok(())
    }
    pub fn clear(
        &mut self,
        owner: &str,
        media: Id,
        generation: Generation,
        name: &str,
    ) -> Result<(), ErrorCode> {
        self.check(media, generation, self.window.revision)?;
        self.tracks.remove(&(owner.into(), name.into()));
        Ok(())
    }
    pub fn revoke(&mut self, owner: &str) {
        self.tracks.retain(|(id, _), _| id != owner);
    }
    pub fn frame(
        &self,
        position_ms: u64,
        paused: bool,
        speed: f64,
    ) -> Result<OverlayFrame, ErrorCode> {
        if !speed.is_finite() || speed <= 0. {
            return Err(ErrorCode::InvalidArgument);
        }
        let visible = self
            .tracks
            .values()
            .flat_map(|track| track.cues.values())
            .filter(|cue| {
                cue.value.start_ms.0 <= position_ms
                    && position_ms < cue.value.start_ms.0 + cue.value.duration_ms.0
            })
            .take(512)
            .count();
        let memory = self.ledger.reserve(
            "host-overlay-frames",
            &[(Resource::HostBytes, visible as u64 * 32768)],
        )?;
        let mut frame = OverlayFrame {
            cues: Vec::new(),
            animate: false,
            next_change: None,
            omitted: 0,
            _memory: memory,
        };
        let mut next = None::<u64>;
        let mut owners = BTreeMap::<&str, usize>::new();
        let mut rows = BTreeMap::<u8, usize>::new();
        for ((owner, name), track) in &self.tracks {
            for cue in track.cues.values() {
                let start = cue.value.start_ms.0;
                let end = start + cue.value.duration_ms.0;
                if start > position_ms {
                    next = Some(next.map_or(start, |n| n.min(start)));
                    break;
                }
                if end <= position_ms {
                    continue;
                }
                next = Some(next.map_or(end, |n| n.min(end)));
                let count = owners.entry(owner).or_default();
                if *count >= 256 || frame.cues.len() >= 512 {
                    frame.omitted += 1;
                    continue;
                }
                *count += 1;
                let progress = (position_ms - start) as f32 / cue.value.duration_ms.0 as f32;
                let group = match cue.value.layout {
                    CueLayout::Scroll => 0,
                    CueLayout::Top => 1,
                    CueLayout::Bottom => 2,
                    CueLayout::Positioned { .. } => 3,
                };
                let row = rows.entry(group).or_default();
                let y = 0.05 + (*row % 12) as f32 * 0.07;
                *row += 1;
                let (x, y) = match cue.value.layout {
                    CueLayout::Scroll => {
                        frame.animate |= !paused;
                        (1.05 - progress * 2.1, y)
                    }
                    CueLayout::Top => (0.5, y),
                    CueLayout::Bottom => (0.5, 1. - y),
                    CueLayout::Positioned { x, y } => (x, y),
                };
                frame.cues.push(PaintCue {
                    owner: owner.clone(),
                    track: name.clone(),
                    cue: cue.value.clone(),
                    x,
                    y,
                    progress,
                });
            }
        }
        if !paused {
            frame.next_change = next.map(|ms| {
                Duration::from_secs_f64(
                    (ms.saturating_sub(position_ms) as f64 / 1000. / speed).min(86400.),
                )
            });
        }
        Ok(frame)
    }
}
fn window(revision: Id, position: u64) -> OverlayWindow {
    OverlayWindow {
        revision,
        start_ms: WireU64(position.saturating_sub(30_000)),
        end_ms: WireU64(position.saturating_add(120_000)),
    }
}
fn estimate_overlay_node_bytes(node: &iris_api::OverlayNode) -> usize {
    use iris_api::OverlayNode;
    let color = |c: &iris_api::Color| match c {
        iris_api::Color::Hex(s) => s.len() + 2,
        _ => 10,
    };
    let opt_color = |c: &Option<iris_api::Color>| c.as_ref().map_or(4, color);
    match node {
        OverlayNode::Box { fill, border, .. } => 128 + opt_color(fill) + opt_color(border),
        OverlayNode::Line { color: c, .. } => 112 + color(c),
        OverlayNode::Grid { color: c, .. } => 72 + color(c),
        OverlayNode::Crosshair { color: c, .. } => 96 + color(c),
        OverlayNode::Text {
            text, color: c, bg, ..
        } => 128 + text.len() + color(c) + opt_color(bg),
        OverlayNode::Bars {
            values, color: c, ..
        } => 96 + color(c) + values.len().saturating_mul(12),
        OverlayNode::Button {
            id,
            label,
            icon,
            action_id,
            ..
        } => 112 + id.len() + label.len() + icon.as_ref().map_or(4, String::len) + action_id.len(),
    }
}
fn estimate_cue_bytes(cue: &TimedCue) -> Result<u64, ErrorCode> {
    let base: usize = 96 + cue.id.len();
    let layout_bytes: usize = match cue.layout {
        CueLayout::Scroll | CueLayout::Top | CueLayout::Bottom => 24,
        CueLayout::Positioned { .. } => 56,
    };
    let content_bytes: usize = match &cue.content {
        CueContent::Text { text, color, .. } => {
            let color_len = match color {
                iris_api::Color::Hex(s) => s.len() + 2,
                _ => 10,
            };
            72 + text.len() + color_len
        }
        CueContent::Vector { nodes } => {
            let mut sum = 32usize;
            for node in nodes {
                sum = sum.saturating_add(estimate_overlay_node_bytes(node));
            }
            sum
        }
    };
    let total = base
        .saturating_add(layout_bytes)
        .saturating_add(content_bytes);
    if total > 16_384 {
        return Err(ErrorCode::QuotaExceeded);
    }
    Ok(total as u64)
}
fn charges(count: usize, bytes: u64) -> [(Resource, u64); 4] {
    [
        (Resource::Handle, 1),
        (Resource::OverlayCues, count as u64),
        (Resource::OverlayBytes, bytes),
        (Resource::HostBytes, bytes * 8 + count as u64 * 512 + 4096),
    ]
}
fn validate(value: &TimedCue) -> Result<(), ErrorCode> {
    validation::identifier(&value.id)?;
    if value.duration_ms.0 == 0 || value.duration_ms.0 > MAX_DURATION {
        return Err(ErrorCode::InvalidArgument);
    }
    if let CueLayout::Positioned { x, y } = value.layout
        && (!x.is_finite()
            || !y.is_finite()
            || !(0. ..=1.).contains(&x)
            || !(0. ..=1.).contains(&y))
    {
        return Err(ErrorCode::InvalidArgument);
    }
    match &value.content {
        CueContent::Text { text, size, .. } => {
            validation::text(text, 1024)?;
            if !(9..=64).contains(size) {
                return Err(ErrorCode::InvalidArgument);
            }
        }
        CueContent::Vector { nodes } => {
            if nodes.len() > 16 {
                return Err(ErrorCode::QuotaExceeded);
            }
            validation::overlay_with_limit(nodes, false, 32)?;
            let text: usize = nodes
                .iter()
                .filter_map(|n| {
                    if let iris_api::OverlayNode::Text { text, .. } = n {
                        Some(text.len())
                    } else {
                        None
                    }
                })
                .sum();
            if text > 1024 {
                return Err(ErrorCode::QuotaExceeded);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
