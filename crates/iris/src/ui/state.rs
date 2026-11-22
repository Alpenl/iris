//! Cohesive UI state structs extracted from `IrisWindow`:
//! overlays, scrub/pointer tracking, LRU bounded thumbnail cache,
//! cached sorted directory listing, and single-line IME-capable text input.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use gpui::RenderImage;

use super::context_menu::ContextMenu;
use super::directory::DirRow;
use crate::library::Entry;

/// Cap in-memory decoded thumbnails to keep RAM usage tiny.
pub(super) const MAX_THUMB_CACHE: usize = 80;
/// Maximum simultaneous background thumbnail decode tasks.
pub(super) const MAX_THUMB_CONCURRENT: usize = 3;

/// Which entries the directory lists. Favorites span folders.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum DirectoryFilter {
    All,
    Videos,
    Photos,
    Favorites,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PopoverKind {
    Subtitles,
    Audio,
    Speed,
    More,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SettingsTab {
    Appearance,
    Playback,
    Tracks,
    View,
    Plugins,
    Shortcuts,
}

/// Manages all mutually exclusive modals, popovers, and transient surfaces.
pub(super) struct OverlayState {
    pub dir_popup: bool,
    pub settings_open: bool,
    pub settings_tab: SettingsTab,
    pub info_open: bool,
    pub popover: Option<PopoverKind>,
    pub context_menu: Option<ContextMenu>,
    pub palette_open: bool,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            dir_popup: false,
            settings_open: false,
            settings_tab: SettingsTab::Appearance,
            info_open: false,
            popover: None,
            context_menu: None,
            palette_open: false,
        }
    }
}

impl OverlayState {
    pub fn has_blocking(&self) -> bool {
        self.dir_popup
            || self.popover.is_some()
            || self.settings_open
            || self.palette_open
            || self.context_menu.is_some()
    }

    pub fn close_menus(&mut self) {
        self.popover = None;
        self.context_menu = None;
    }

    pub fn close_all(&mut self) {
        self.dir_popup = false;
        self.popover = None;
        self.settings_open = false;
        self.palette_open = false;
        self.context_menu = None;
    }

    pub fn open_settings(&mut self, tab: Option<SettingsTab>) {
        self.popover = None;
        self.context_menu = None;
        self.palette_open = false;
        self.dir_popup = false;
        if let Some(t) = tab {
            self.settings_tab = t;
        }
        self.settings_open = true;
    }

    pub fn toggle_settings(&mut self) -> bool {
        if self.settings_open {
            self.settings_open = false;
        } else {
            self.open_settings(None);
        }
        self.settings_open
    }

    pub fn toggle_palette(&mut self) -> bool {
        if self.palette_open {
            self.palette_open = false;
        } else {
            self.popover = None;
            self.context_menu = None;
            self.settings_open = false;
            self.dir_popup = false;
            self.palette_open = true;
        }
        self.palette_open
    }

    pub fn toggle_popover(&mut self, kind: PopoverKind) {
        self.context_menu = None;
        self.palette_open = false;
        if self.popover == Some(kind) {
            self.popover = None;
        } else {
            self.popover = Some(kind);
            self.settings_open = false;
        }
    }

    pub fn open_context_menu(&mut self, menu: ContextMenu) {
        self.popover = None;
        self.palette_open = false;
        self.context_menu = Some(menu);
    }
}

/// Timeline scrubbing, volume dragging, wheel damping, and double-click debounce.
pub(super) struct ScrubState {
    pub timeline_hover: bool,
    pub seeking: bool,
    pub seek_preview_ratio: Option<f32>,
    pub last_seek_drag: Instant,
    pub dragging_volume: bool,
    pub wheel_accum: f64,
    pub click_seq: u64,
}

impl Default for ScrubState {
    fn default() -> Self {
        Self {
            timeline_hover: false,
            seeking: false,
            seek_preview_ratio: None,
            last_seek_drag: Instant::now(),
            dragging_volume: false,
            wheel_accum: 0.,
            click_seq: 0,
        }
    }
}

/// Cached directory rows, sorted entries, photo slice, and tab badge counts,
/// rebuilt only when entries, filter, sort, search, or favorites change.
#[derive(Default)]
pub(super) struct DirectoryCache {
    pub rows: Vec<DirRow>,
    pub sorted_entries: Vec<Entry>,
    pub photo_entries: Vec<Entry>,
    pub video_audio_count: usize,
    pub photo_count: usize,
    pub favorite_count: usize,
}

/// Bounded LRU thumbnail cache with priority decode queue and concurrency cap.
#[derive(Default)]
pub(super) struct ThumbCache {
    entries: HashMap<PathBuf, (u64, Arc<RenderImage>)>,
    pending: VecDeque<PathBuf>,
    inflight: HashSet<PathBuf>,
    active_tasks: usize,
    tick: u64,
    generation: u64,
}

impl ThumbCache {
    pub fn get(&self, path: &Path) -> Option<Arc<RenderImage>> {
        self.entries.get(path).map(|(_, img)| img.clone())
    }

    /// Touches visible paths so LRU eviction keeps the currently displayed window hot.
    pub fn touch(&mut self, path: &Path) {
        if let Some((stamp, _)) = self.entries.get_mut(path) {
            self.tick += 1;
            *stamp = self.tick;
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Replaces the pending queue with paths ordered by priority (closest to
    /// current photo first), bumping generation so stale queued items are dropped.
    pub fn enqueue_window(&mut self, prioritized: Vec<PathBuf>) {
        self.generation = self.generation.wrapping_add(1);
        self.pending.clear();
        for p in prioritized {
            if self.entries.contains_key(&p) {
                self.touch(&p);
                continue;
            }
            if !self.inflight.contains(&p) {
                self.pending.push_back(p);
            }
        }
    }

    /// Pops up to available concurrency slots from the pending queue.
    pub fn pop_next_batch(&mut self) -> Vec<(u64, PathBuf)> {
        let mut out = Vec::new();
        while self.active_tasks < MAX_THUMB_CONCURRENT {
            let Some(path) = self.pending.pop_front() else {
                break;
            };
            if self.entries.contains_key(&path) || self.inflight.contains(&path) {
                continue;
            }
            self.inflight.insert(path.clone());
            self.active_tasks += 1;
            out.push((self.generation, path));
        }
        out
    }

    pub fn finish(&mut self, path: PathBuf, decoded: Option<Arc<RenderImage>>) {
        self.inflight.remove(&path);
        self.active_tasks = self.active_tasks.saturating_sub(1);
        if let Some(img) = decoded {
            self.tick += 1;
            let stamp = self.tick;
            if self.entries.len() >= MAX_THUMB_CACHE && !self.entries.contains_key(&path) {
                // Evict least-recently-touched entry instead of clearing the whole cache.
                if let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, (t, _))| *t)
                    .map(|(k, _)| k.clone())
                {
                    self.entries.remove(&oldest);
                }
            }
            self.entries.insert(path, (stamp, img));
        }
    }
}

/// Single-line text buffer supporting cursor movement, selection, clipboard
/// operations, and full macOS/Windows IME composition (`marked_range`).
#[derive(Default, Clone, Debug)]
pub(super) struct TextInputState {
    pub content: String,
    pub selected_range: Range<usize>,
    pub marked_range: Option<Range<usize>>,
}

impl TextInputState {
    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    pub fn clear(&mut self) {
        self.content.clear();
        self.selected_range = 0..0;
        self.marked_range = None;
    }

    pub fn select_all(&mut self) {
        self.marked_range = None;
        self.selected_range = 0..self.content.len();
    }

    pub fn selected_text(&self) -> &str {
        let range = self.clamped_range(self.selected_range.clone());
        &self.content[range]
    }

    pub fn move_left(&mut self) {
        self.marked_range = None;
        let pos = if !self.selected_range.is_empty() {
            self.selected_range.start
        } else {
            self.prev_char_boundary(self.selected_range.start)
        };
        self.selected_range = pos..pos;
    }

    pub fn move_right(&mut self) {
        self.marked_range = None;
        let pos = if !self.selected_range.is_empty() {
            self.selected_range.end
        } else {
            self.next_char_boundary(self.selected_range.end)
        };
        self.selected_range = pos..pos;
    }

    pub fn move_home(&mut self) {
        self.marked_range = None;
        self.selected_range = 0..0;
    }

    pub fn move_end(&mut self) {
        self.marked_range = None;
        let end = self.content.len();
        self.selected_range = end..end;
    }

    pub fn backspace(&mut self) {
        if self.selected_range.is_empty() {
            let prev = self.prev_char_boundary(self.selected_range.start);
            self.selected_range = prev..self.selected_range.end;
        }
        self.replace_bytes(None, "");
    }

    pub fn delete_forward(&mut self) {
        if self.selected_range.is_empty() {
            let next = self.next_char_boundary(self.selected_range.end);
            self.selected_range = self.selected_range.start..next;
        }
        self.replace_bytes(None, "");
    }

    pub fn insert_paste(&mut self, raw: &str) {
        let clean: String = raw
            .chars()
            .map(|c| {
                if matches!(c, '\n' | '\r' | '\t') {
                    ' '
                } else {
                    c
                }
            })
            .filter(|c| !c.is_control())
            .collect();
        if !clean.is_empty() {
            self.replace_bytes(None, &clean);
        }
    }

    pub fn display_text(&self, focused: bool) -> String {
        if !focused {
            return self.content.clone();
        }
        let cursor = self.clamped_boundary(self.selected_range.end);
        let mut out = String::with_capacity(self.content.len() + 4);
        out.push_str(&self.content[..cursor]);
        out.push('▏');
        out.push_str(&self.content[cursor..]);
        out
    }

    // ---- UTF-16 helpers for GPUI EntityInputHandler ----

    pub fn offset_from_utf16(&self, utf16_offset: usize) -> usize {
        let mut utf8 = 0;
        let mut u16_acc = 0;
        for ch in self.content.chars() {
            if u16_acc >= utf16_offset {
                break;
            }
            u16_acc += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    pub fn offset_to_utf16(&self, utf8_offset: usize) -> usize {
        let mut u16_acc = 0;
        let mut utf8 = 0;
        for ch in self.content.chars() {
            if utf8 >= utf8_offset {
                break;
            }
            utf8 += ch.len_utf8();
            u16_acc += ch.len_utf16();
        }
        u16_acc
    }

    pub fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    pub fn replace_utf16(&mut self, range_utf16: Option<Range<usize>>, new_text: &str) {
        let range = range_utf16.map(|r| self.range_from_utf16(&r));
        self.replace_bytes(range, new_text);
    }

    pub fn replace_and_mark_utf16(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_utf16: Option<Range<usize>>,
    ) {
        let range = range_utf16
            .map(|r| self.range_from_utf16(&r))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let range = self.clamped_range(range);
        self.content.replace_range(range.clone(), new_text);
        let end = range.start + new_text.len();
        self.marked_range = (!new_text.is_empty()).then_some(range.start..end);
        self.selected_range = if let Some(sel_u16) = new_selected_utf16 {
            let local_start = utf16_to_utf8_in(new_text, sel_u16.start);
            let local_end = utf16_to_utf8_in(new_text, sel_u16.end);
            (range.start + local_start)..(range.start + local_end)
        } else {
            end..end
        };
    }

    fn replace_bytes(&mut self, range: Option<Range<usize>>, new_text: &str) {
        let range = range
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let range = self.clamped_range(range);
        self.content.replace_range(range.clone(), new_text);
        let pos = range.start + new_text.len();
        self.selected_range = pos..pos;
        self.marked_range = None;
    }

    fn clamped_boundary(&self, pos: usize) -> usize {
        let mut p = pos.min(self.content.len());
        while p > 0 && !self.content.is_char_boundary(p) {
            p -= 1;
        }
        p
    }

    fn clamped_range(&self, r: Range<usize>) -> Range<usize> {
        let s = self.clamped_boundary(r.start);
        let e = self.clamped_boundary(r.end).max(s);
        s..e
    }

    fn prev_char_boundary(&self, pos: usize) -> usize {
        let pos = self.clamped_boundary(pos);
        self.content[..pos]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_char_boundary(&self, pos: usize) -> usize {
        let pos = self.clamped_boundary(pos);
        self.content[pos..]
            .chars()
            .next()
            .map_or(self.content.len(), |c| pos + c.len_utf8())
    }
}

fn utf16_to_utf8_in(s: &str, u16_off: usize) -> usize {
    let mut utf8 = 0;
    let mut u16_acc = 0;
    for ch in s.chars() {
        if u16_acc >= u16_off {
            break;
        }
        u16_acc += ch.len_utf16();
        utf8 += ch.len_utf8();
    }
    utf8
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Frame, RgbaImage};

    fn dummy_render_image() -> Arc<RenderImage> {
        Arc::new(RenderImage::new([Frame::new(RgbaImage::new(1, 1))]))
    }

    #[test]
    fn text_input_handles_cjk_emoji_utf16_and_ime_marking() {
        let mut input = TextInputState::default();
        // Compose CJK via IME marked range, then commit final text.
        input.replace_and_mark_utf16(None, "ni", None);
        assert_eq!(input.text(), "ni");
        assert_eq!(input.marked_range, Some(0..2));

        input.replace_utf16(None, "你好");
        assert_eq!(input.text(), "你好");
        assert_eq!(input.marked_range, None);
        assert_eq!(input.selected_range, 6..6);

        // Emoji takes 2 UTF-16 code units and 4 UTF-8 bytes.
        input.replace_utf16(None, "🎬a");
        assert_eq!(input.text(), "你好🎬a");
        assert_eq!(input.offset_to_utf16(input.content.len()), 5);
        assert_eq!(input.offset_from_utf16(2), 6); // after "你好"
        assert_eq!(input.offset_from_utf16(4), 10); // after "你好🎬"

        input.move_left();
        input.move_left();
        input.delete_forward(); // deletes emoji
        assert_eq!(input.text(), "你好a");

        input.select_all();
        assert_eq!(input.selected_text(), "你好a");
        input.insert_paste("foo\nbar\tbaz\x07");
        assert_eq!(input.text(), "foo bar baz");
    }

    #[test]
    fn thumb_cache_enforces_concurrency_and_lru_eviction() {
        let mut cache = ThumbCache::default();
        let paths: Vec<PathBuf> = (0..(MAX_THUMB_CACHE + 5))
            .map(|i| PathBuf::from(format!("/p/{i}.jpg")))
            .collect();

        cache.enqueue_window(paths[..10].to_vec());
        let first_batch = cache.pop_next_batch();
        assert_eq!(first_batch.len(), MAX_THUMB_CONCURRENT);
        assert!(cache.pop_next_batch().is_empty());

        for (_, p) in first_batch {
            cache.finish(p, Some(dummy_render_image()));
        }

        // Fill up to capacity, touch entry 0 so it becomes MRU, then insert 5 more.
        for p in &paths[3..MAX_THUMB_CACHE] {
            cache.enqueue_window(vec![p.clone()]);
            for (_, next) in cache.pop_next_batch() {
                cache.finish(next, Some(dummy_render_image()));
            }
        }
        cache.touch(&paths[0]);
        for p in &paths[MAX_THUMB_CACHE..] {
            cache.enqueue_window(vec![p.clone()]);
            for (_, next) in cache.pop_next_batch() {
                cache.finish(next, Some(dummy_render_image()));
            }
        }
        assert!(cache.get(&paths[0]).is_some(), "touched entry must survive");
        assert!(
            cache.get(&paths[1]).is_none(),
            "oldest untouched entry must evict"
        );
    }
}
