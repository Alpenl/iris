//! `AppModel` text-input host integration: `TextFieldHost`, `EntityInputHandler`
//! IME bridge, and focus-out reconciliation across regional GPUI views.

use std::ops::Range;

use gpui::{Bounds, Context, EntityInputHandler, Pixels, Point, UTF16Selection, Window};
use iris_core::preset::SidebarMode;

use super::AppModel;
use super::kit::text_buffer::TextInputBuffers;
use super::kit::text_field::{
    ActiveSlot, TextFieldHost, TextInputHub, ime_character_index_for_point, ime_marked_text_range,
    ime_replace_and_mark_text_in_range, ime_replace_text_in_range, ime_selected_text_range,
    ime_text_for_range, ime_unmark_text,
};
use super::views::{DirtyRegions, dirty_regions_for_slot_edit};

impl AppModel {
    pub(in crate::ui) fn is_form_key_visible(&self, key: &str) -> bool {
        let modal_visible = self.has_host_consent() || self.plugins.active_modal().is_some();
        if key.starts_with("modal:") {
            !self.has_host_consent()
                && self.plugins.active_modal().is_some_and(|(pid, modal)| {
                    TextInputBuffers::modal_has_input_key(pid, modal, key)
                })
        } else if key.starts_with("sb:") {
            self.sidebar_is_visible()
                && self.catalog.sidebar_mode == SidebarMode::Plugins
                && !modal_visible
                && !self.overlays.has_menu_or_modal()
        } else {
            self.overlays.dialog.is_some() && !modal_visible
        }
    }

    pub(in crate::ui) fn reconcile_input_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut dirty = DirtyRegions::NONE;
        if !self.text_inputs.form_focus.is_focused(window)
            && let Some(prev) = self.text_inputs.active_form_key.take()
        {
            dirty = dirty.union(if prev.starts_with("sb:") {
                DirtyRegions::plugin_panel_only()
            } else {
                DirtyRegions::root_only()
            });
        }
        let modal_visible = self.has_host_consent() || self.plugins.active_modal().is_some();
        let files_visible = self.sidebar_is_visible()
            && self.catalog.sidebar_mode == SidebarMode::Files
            && !modal_visible
            && !self.overlays.has_menu_or_modal();
        let form_visible = self
            .text_inputs
            .active_form_key
            .as_deref()
            .is_some_and(|key| self.is_form_key_visible(key));
        if !form_visible && let Some(prev) = self.text_inputs.active_form_key.take() {
            dirty = dirty.union(if prev.starts_with("sb:") {
                DirtyRegions::plugin_panel_only()
            } else {
                DirtyRegions::root_only()
            });
        }
        let recorder_focused = self.text_inputs.recorder_focus.is_focused(window);
        let recording = self.overlays.is_recording(recorder_focused);
        if !recording && self.overlays.recording_shortcut.take().is_some() {
            dirty = dirty.union(DirtyRegions {
                root: true,
                settings: true,
                ..DirtyRegions::NONE
            });
        }
        if (!files_visible && self.text_inputs.search_focus.is_focused(window))
            || (!self.overlays.palette_open && self.text_inputs.palette_focus.is_focused(window))
            || (!form_visible && self.text_inputs.form_focus.is_focused(window))
            || (!recording && recorder_focused)
        {
            self.focus.focus(window);
            dirty = dirty.union(DirtyRegions::root_only());
        }
        if dirty.any() {
            self.notify_regions(dirty, cx);
        }
    }
}

impl TextFieldHost for AppModel {
    fn text_inputs(&self) -> &TextInputHub {
        &self.text_inputs
    }

    fn text_inputs_mut(&mut self) -> &mut TextInputHub {
        &mut self.text_inputs
    }

    fn is_palette_open(&self) -> bool {
        self.overlays.palette_open
    }

    fn focus_root(&self, window: &mut Window) {
        self.focus.focus(window);
    }

    fn on_slot_edited(&mut self, slot: ActiveSlot, cx: &mut Context<Self>) {
        match slot {
            ActiveSlot::Search => self.filter_directory_rows(),
            ActiveSlot::Palette => {
                self.overlays.palette_index = 0;
                self.overlays.palette_scroll.scroll_to_item(0);
            }
            ActiveSlot::Form => {
                self.plugin_view.modal.submitting = None;
                self.overlays.dialog_error = None;
                self.plugin_view.modal.error = None;
            }
        }
        let regions =
            dirty_regions_for_slot_edit(slot, self.text_inputs.active_form_key.as_deref());
        self.notify_regions(regions, cx);
    }

    fn on_slot_visual_change(&mut self, slot: ActiveSlot, cx: &mut Context<Self>) {
        let regions =
            dirty_regions_for_slot_edit(slot, self.text_inputs.active_form_key.as_deref());
        self.notify_regions(regions, cx);
    }

    fn on_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.handle_escape(window, cx);
    }

    fn on_search_enter(&mut self, cx: &mut Context<Self>) {
        self.submit_directory_search(cx);
    }

    fn on_palette_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_palette_selected(window, cx);
    }

    fn on_palette_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.move_palette_selection(delta, cx);
    }

    fn on_form_tab(&mut self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_form_field(reverse, window, cx);
    }
}

impl EntityInputHandler for AppModel {
    fn text_for_range(
        &mut self,
        r16: Range<usize>,
        adj: &mut Option<Range<usize>>,
        w: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        ime_text_for_range(self, r16, adj, w)
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        w: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        ime_selected_text_range(self, w)
    }

    fn marked_text_range(&self, w: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        ime_marked_text_range(self, w)
    }

    fn unmark_text(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        ime_unmark_text(self, w, cx);
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        ime_replace_text_in_range(self, range, text, w, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        sel: Option<Range<usize>>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        ime_replace_and_mark_text_in_range(self, range, text, sel, w, cx);
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        b: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(b)
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        w: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        ime_character_index_for_point(self, w)
    }
}
