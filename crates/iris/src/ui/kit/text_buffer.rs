//! Pure UTF-8 / UTF-16 single-line text buffer (`TextInputState`) and
//! window-independent form/modal field store (`TextInputBuffers`).

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use iris_api::{PluginModal, Widget};

#[derive(Clone, Default)]
pub(in crate::ui) struct TextInputState {
    content: String,
    pub(in crate::ui) selected_range: Range<usize>,
    pub(in crate::ui) marked_range: Option<Range<usize>>,
    // A plugin can refresh a field spec while its edit buffer is still alive.
    // Once classified as secret, retained text must never become readable on refresh.
    secret: Cell<bool>,
}

impl TextInputState {
    pub(in crate::ui) fn with_text(s: impl Into<String>) -> Self {
        let content = s.into();
        let len = content.len();
        Self {
            content,
            selected_range: len..len,
            marked_range: None,
            secret: Cell::new(false),
        }
    }

    pub(super) fn mark_secret(&self) {
        self.secret.set(true);
    }

    pub(in crate::ui) fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    pub(in crate::ui) fn text(&self) -> &str {
        &self.content
    }

    pub(in crate::ui) fn clear(&mut self) {
        self.content.clear();
        self.selected_range = 0..0;
        self.marked_range = None;
    }

    pub(in crate::ui) fn select_all(&mut self) {
        self.selected_range = 0..self.content.len();
    }

    /// Copy without a selection copies the whole field. Cut requires a selection.
    pub(super) fn clipboard_text(&self, whole_if_unselected: bool) -> Option<&str> {
        if self.secret.get() {
            return None;
        }
        let selection = self.clamped_range(self.selected_range.clone());
        let text = if selection.is_empty() && whole_if_unselected {
            self.text()
        } else {
            &self.content[selection]
        };
        (!text.is_empty()).then_some(text)
    }

    pub(super) fn native_text_for_range(
        &self,
        requested: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
    ) -> Option<String> {
        if self.secret.get() {
            *adjusted = None;
            return None;
        }
        let range = self.range_from_utf16(&requested);
        *adjusted = Some(self.range_to_utf16(&range));
        Some(self.content[range].to_owned())
    }

    pub(in crate::ui) fn move_left(&mut self) {
        self.marked_range = None;
        let p = if !self.selected_range.is_empty() {
            self.selected_range.start
        } else {
            self.prev_char_boundary(self.selected_range.start)
        };
        self.selected_range = p..p;
    }

    pub(in crate::ui) fn move_right(&mut self) {
        self.marked_range = None;
        let p = if !self.selected_range.is_empty() {
            self.selected_range.end
        } else {
            self.next_char_boundary(self.selected_range.end)
        };
        self.selected_range = p..p;
    }

    pub(in crate::ui) fn move_home(&mut self) {
        self.marked_range = None;
        self.selected_range = 0..0;
    }

    pub(in crate::ui) fn move_end(&mut self) {
        self.marked_range = None;
        let len = self.content.len();
        self.selected_range = len..len;
    }

    pub(in crate::ui) fn backspace(&mut self) {
        let r = if !self.selected_range.is_empty() {
            self.clamped_range(self.selected_range.clone())
        } else {
            self.prev_char_boundary(self.selected_range.start)..self.selected_range.start
        };
        self.replace_utf8(r, "");
    }

    pub(in crate::ui) fn delete_forward(&mut self) {
        let r = if !self.selected_range.is_empty() {
            self.clamped_range(self.selected_range.clone())
        } else {
            self.selected_range.end..self.next_char_boundary(self.selected_range.end)
        };
        self.replace_utf8(r, "");
    }

    pub(in crate::ui) fn insert_paste(&mut self, raw: &str) {
        let clean: String = raw
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect();
        self.replace_utf8(self.clamped_range(self.selected_range.clone()), &clean);
    }

    fn replace_utf8(&mut self, range: Range<usize>, new_text: &str) {
        let range = self.clamped_range(range);
        self.content.replace_range(range.clone(), new_text);
        let pos = range.start + new_text.len();
        self.selected_range = pos..pos;
        self.marked_range = None;
    }

    pub(super) fn display_text(&self, focused: bool) -> String {
        let cursor = focused.then(|| self.clamped_boundary(self.selected_range.end));
        if self.secret.get() {
            let mask_bytes = self.content.chars().count() * '•'.len_utf8();
            let cursor_bytes = if focused { '▏'.len_utf8() } else { 0 };
            let mut masked = String::with_capacity(mask_bytes + cursor_bytes);
            for (offset, _) in self.content.char_indices() {
                if cursor == Some(offset) {
                    masked.push('▏');
                }
                masked.push('•');
            }
            if cursor == Some(self.content.len()) {
                masked.push('▏');
            }
            masked
        } else if let Some(cursor) = cursor {
            let mut shown = String::with_capacity(self.content.len() + '▏'.len_utf8());
            shown.push_str(&self.content[..cursor]);
            shown.push('▏');
            shown.push_str(&self.content[cursor..]);
            shown
        } else {
            self.content.clone()
        }
    }

    pub(in crate::ui) fn offset_to_utf16(&self, utf8_offset: usize) -> usize {
        let mut u16_off = 0;
        let mut u8_cnt = 0;
        for ch in self.content.chars() {
            if u8_cnt >= utf8_offset {
                break;
            }
            u8_cnt += ch.len_utf8();
            u16_off += ch.len_utf16();
        }
        u16_off
    }

    pub(in crate::ui) fn range_to_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(r.start)..self.offset_to_utf16(r.end)
    }

    pub(in crate::ui) fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        utf16_to_utf8_in(&self.content, r.start)
            ..utf16_to_utf8_in(&self.content, r.end.max(r.start))
    }

    pub(in crate::ui) fn replace_utf16(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
    ) {
        let range = self.clamped_range(
            range_utf16
                .map(|r| self.range_from_utf16(&r))
                .or_else(|| self.marked_range.clone())
                .unwrap_or_else(|| self.selected_range.clone()),
        );
        self.content.replace_range(range.clone(), new_text);
        let pos = range.start + new_text.len();
        self.selected_range = pos..pos;
        self.marked_range = None;
    }

    pub(in crate::ui) fn replace_and_mark_utf16(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_utf16: Option<Range<usize>>,
    ) {
        let range = self.clamped_range(
            range_utf16
                .map(|r| self.range_from_utf16(&r))
                .or_else(|| self.marked_range.clone())
                .unwrap_or_else(|| self.selected_range.clone()),
        );
        self.content.replace_range(range.clone(), new_text);
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_utf16
            .map(|r| {
                let s = utf16_to_utf8_in(new_text, r.start);
                let e = utf16_to_utf8_in(new_text, r.end.max(r.start));
                (range.start + s)..(range.start + e)
            })
            .unwrap_or_else(|| {
                let end = range.start + new_text.len();
                end..end
            });
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
        s..self.clamped_boundary(r.end).max(s)
    }

    fn prev_char_boundary(&self, pos: usize) -> usize {
        self.content[..self.clamped_boundary(pos)]
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

pub(in crate::ui) fn format_slider_value(value: f64, step: f64) -> String {
    if !value.is_finite() {
        return "0".into();
    }
    let s = if step.is_finite() && step.abs() > 1e-6 {
        step.abs()
    } else {
        1.0
    };
    if (s - s.round()).abs() < 1e-6 && (value - value.round()).abs() < 1e-6 {
        format!("{:.0}", value.round())
    } else if (s * 10.0 - (s * 10.0).round()).abs() < 1e-5 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

/// Pure data backing [`super::text_field::TextInputHub`] so search, palette, and
/// modal form lifecycles can be unit-tested without opening a GPUI window.
#[derive(Default)]
pub(in crate::ui) struct TextInputBuffers {
    pub(in crate::ui) search_open: bool,
    pub(in crate::ui) search_input: TextInputState,
    pub(in crate::ui) palette_input: TextInputState,
    pub(in crate::ui) form_fields: HashMap<String, TextInputState>,
    pub(in crate::ui) active_form_key: Option<String>,
    active_modal_tag: Option<(String, String)>,
}

impl TextInputBuffers {
    pub(in crate::ui) fn is_focused_given(
        &self,
        search_focused: bool,
        palette_focused: bool,
        form_focused: bool,
    ) -> bool {
        (self.search_open && search_focused)
            || palette_focused
            || (self.active_form_key.is_some() && form_focused)
    }

    pub(in crate::ui) fn clear_modal_fields(&mut self) {
        self.form_fields.retain(|k, _| !k.starts_with("modal:"));
        self.active_modal_tag = None;
        if self
            .active_form_key
            .as_deref()
            .is_some_and(|k| k.starts_with("modal:"))
        {
            self.active_form_key = None;
        }
    }

    pub(in crate::ui) fn sync_plugin_modal(
        &mut self,
        active: Option<(&str, &PluginModal)>,
    ) -> bool {
        let Some((pid, modal)) = active else {
            self.clear_modal_fields();
            return false;
        };
        let same = self
            .active_modal_tag
            .as_ref()
            .is_some_and(|(p, m)| p == pid && m == &modal.id);
        if !same {
            self.form_fields.retain(|k, _| !k.starts_with("modal:"));
            self.active_modal_tag = Some((pid.to_string(), modal.id.clone()));
        }
        let mut first_input = None;
        for w in &modal.widgets {
            let field_key = |id: &str| format!("modal:{pid}:{}:{id}", modal.id);
            match w {
                Widget::Input {
                    id, value, secret, ..
                } => {
                    let key = field_key(id);
                    first_input.get_or_insert_with(|| key.clone());
                    let state = self
                        .form_fields
                        .entry(key)
                        .or_insert_with(|| TextInputState::with_text(value.as_str()));
                    if *secret {
                        state.mark_secret();
                    }
                }
                Widget::Toggle { id, checked, .. } => {
                    self.form_fields
                        .entry(field_key(id))
                        .or_insert_with(|| TextInputState::with_text(checked.to_string()));
                }
                Widget::Select { id, selected, .. } => {
                    self.form_fields
                        .entry(field_key(id))
                        .or_insert_with(|| TextInputState::with_text(selected.as_str()));
                }
                Widget::Slider {
                    id, value, step, ..
                } => {
                    self.form_fields.entry(field_key(id)).or_insert_with(|| {
                        TextInputState::with_text(format_slider_value(*value, *step))
                    });
                }
                _ => {}
            }
        }
        if !same {
            self.active_form_key = first_input;
            return self.active_form_key.is_some();
        }
        false
    }

    pub(in crate::ui) fn collect_modal_values(
        &self,
        pid: &str,
        modal: &PluginModal,
    ) -> BTreeMap<String, String> {
        let mut values = BTreeMap::new();
        for widget in &modal.widgets {
            let (id, fallback) = match widget {
                Widget::Input { id, value, .. } => (id, value.clone()),
                Widget::Toggle { id, checked, .. } => (id, checked.to_string()),
                Widget::Select { id, selected, .. } => (id, selected.clone()),
                Widget::Slider {
                    id, value, step, ..
                } => (id, format_slider_value(*value, *step)),
                _ => continue,
            };
            let key = format!("modal:{pid}:{}:{id}", modal.id);
            let val = self
                .form_fields
                .get(&key)
                .map_or(fallback, |s| s.text().to_string());
            values.insert(id.clone(), val);
        }
        values
    }
}

fn utf16_to_utf8_in(s: &str, u16_off: usize) -> usize {
    let (mut utf8, mut u16_acc) = (0, 0);
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
    use super::TextInputState;

    #[test]
    fn text_input_handles_cjk_emoji_utf16_ime_and_masked_cursor() {
        let mut input = TextInputState::default();
        input.mark_secret();
        input.replace_and_mark_utf16(None, "ni", None);
        assert_eq!(input.marked_range, Some(0..2));
        input.replace_utf16(None, "你好🎬a");
        assert_eq!(input.offset_to_utf16(input.content.len()), 5);
        input.move_left();
        input.move_left();
        assert_eq!(input.display_text(true), "••▏••");
        input.delete_forward();
        assert_eq!(input.text(), "你好a");
        assert_eq!(input.display_text(true), "••▏•");
        assert_eq!(input.display_text(false), "•••");
    }

    #[test]
    fn secret_composition_and_selection_are_editable_but_cannot_be_read_back() {
        let mut input = TextInputState::with_text("前🔑後");
        input.mark_secret();
        let mut actual = Some(0..4);
        assert_eq!(
            input.native_text_for_range(0..usize::MAX, &mut actual),
            None
        );
        assert_eq!(actual, None);
        assert_eq!(input.clipboard_text(true), None);

        input.replace_and_mark_utf16(Some(1..3), "新🎬", Some(1..3));
        assert_eq!(input.range_to_utf16(&input.selected_range), 2..4);
        let marked = input.range_to_utf16(input.marked_range.as_ref().unwrap());
        assert_eq!(marked, 1..4);
        assert_eq!(input.native_text_for_range(marked, &mut actual), None);
        assert_eq!(input.clipboard_text(false), None);
        assert_eq!(input.display_text(true), "•••▏•");

        input.replace_utf16(None, "鍵");
        assert_eq!(input.text(), "前鍵後");
        assert_eq!(input.marked_range, None);
        input.select_all();
        assert_eq!(input.clipboard_text(true), None);
        assert_eq!(input.clipboard_text(false), None);
        input.backspace();
        input.insert_paste("😀\n中");
        assert_eq!(input.text(), "😀 中");
        assert_eq!(input.range_to_utf16(&input.selected_range), 4..4);
        assert_eq!(input.native_text_for_range(0..4, &mut actual), None);
    }

    #[test]
    fn ordinary_native_and_clipboard_reads_use_unicode_boundaries() {
        let mut input = TextInputState::with_text("前🔑後");
        let mut actual = None;
        assert_eq!(
            input.native_text_for_range(1..2, &mut actual).as_deref(),
            Some("🔑")
        );
        assert_eq!(actual, Some(1..3));
        assert_eq!(
            input
                .native_text_for_range(3..usize::MAX, &mut actual)
                .as_deref(),
            Some("後")
        );
        assert_eq!(actual, Some(3..4));
        assert_eq!(input.clipboard_text(false), None);
        assert_eq!(input.clipboard_text(true), Some("前🔑後"));
        input.selected_range = input.range_from_utf16(&(1..3));
        assert_eq!(input.clipboard_text(false), Some("🔑"));
        assert_eq!(input.clipboard_text(true), Some("🔑"));
    }

    #[test]
    fn autofocused_password_stays_secret_across_modal_refresh() {
        use super::{PluginModal, TextInputBuffers, Widget};

        let mut modal = PluginModal {
            id: "auth".into(),
            title: String::new(),
            subtitle: None,
            submit_label: None,
            cancel_label: None,
            widgets: vec![Widget::Input {
                id: "password".into(),
                label: None,
                value: "seed".into(),
                placeholder: String::new(),
                secret: true,
                action_id: None,
            }],
        };
        let mut buffers = TextInputBuffers::default();
        assert!(buffers.sync_plugin_modal(Some(("plugin", &modal))));
        let key = buffers.active_form_key.clone().unwrap();
        let input = buffers.form_fields.get_mut(&key).unwrap();
        assert_eq!(input.native_text_for_range(0..4, &mut None), None);
        input.select_all();
        input.replace_utf16(None, "typed");
        let Widget::Input { secret, .. } = &mut modal.widgets[0] else {
            unreachable!()
        };
        *secret = false;
        buffers.sync_plugin_modal(Some(("plugin", &modal)));
        let retained = &buffers.form_fields[&key];
        assert_eq!(retained.display_text(false), "•••••");
        assert_eq!(retained.clipboard_text(true), None);
        assert_eq!(retained.native_text_for_range(0..5, &mut None), None);
        assert_eq!(
            buffers.collect_modal_values("plugin", &modal)["password"],
            "typed"
        );

        buffers.clear_modal_fields();
        buffers.sync_plugin_modal(Some(("plugin", &modal)));
        assert_eq!(buffers.form_fields[&key].clipboard_text(true), Some("seed"));
    }
}
