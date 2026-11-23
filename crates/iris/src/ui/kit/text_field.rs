//! Centralized IME single-line text input hub (`TextInputHub`), `EntityInputHandler`,
//! and the single shared `render_text_field` widget across Search, Palette, and Forms.

use std::ops::{Deref, DerefMut, Range};
use std::rc::Rc;

use gpui::{
    AnyElement, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler,
    FocusHandle, InteractiveElement, IntoElement, KeyContext, KeyDownEvent, MouseButton,
    ParentElement, Pixels, Point, SharedString, Styled, UTF16Selection, Window, canvas, div,
    prelude::FluentBuilder, px,
};

use super::super::IrisWindow;
use super::icon;
pub(in crate::ui) use super::text_buffer::{TextInputBuffers, TextInputState};
use crate::assets::Icon;
use crate::theme::ThemeTokens;

pub(in crate::ui) type FormEnterCallback =
    Rc<dyn Fn(&mut IrisWindow, &mut Window, &mut Context<IrisWindow>)>;

pub(in crate::ui) struct TextInputHub {
    pub(in crate::ui) search_focus: FocusHandle,
    pub(in crate::ui) palette_focus: FocusHandle,
    pub(in crate::ui) form_focus: FocusHandle,
    pub(in crate::ui) buffers: TextInputBuffers,
}

impl Deref for TextInputHub {
    type Target = TextInputBuffers;

    fn deref(&self) -> &Self::Target {
        &self.buffers
    }
}

impl DerefMut for TextInputHub {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffers
    }
}

impl TextInputHub {
    pub(in crate::ui) fn new(cx: &mut Context<IrisWindow>) -> Self {
        Self {
            search_focus: cx.focus_handle(),
            palette_focus: cx.focus_handle(),
            form_focus: cx.focus_handle(),
            buffers: TextInputBuffers::default(),
        }
    }

    pub(in crate::ui) fn is_any_focused(&self, window: &Window) -> bool {
        self.buffers.is_focused_given(
            self.search_focus.is_focused(window),
            self.palette_focus.is_focused(window),
            self.form_focus.is_focused(window),
        )
    }
}

#[derive(Clone)]
pub(in crate::ui) enum TextFieldKind {
    Search,
    Palette,
    Form {
        key: String,
        seed: Option<String>,
        secret: bool,
        on_enter: Option<FormEnterCallback>,
    },
}

pub(in crate::ui) struct TextFieldSpec {
    pub(in crate::ui) id: SharedString,
    pub(in crate::ui) kind: TextFieldKind,
    pub(in crate::ui) placeholder: SharedString,
    pub(in crate::ui) height: Pixels,
    pub(in crate::ui) leading_icon: Option<Icon>,
    pub(in crate::ui) borderless: bool,
}

impl TextFieldSpec {
    pub(in crate::ui) fn form(
        key: String,
        seed: Option<String>,
        placeholder: impl Into<SharedString>,
        secret: bool,
        on_enter: Option<FormEnterCallback>,
    ) -> Self {
        Self {
            id: SharedString::from(format!("form:{key}")),
            kind: TextFieldKind::Form {
                key,
                seed,
                secret,
                on_enter,
            },
            placeholder: placeholder.into(),
            height: px(30.),
            leading_icon: None,
            borderless: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveSlot {
    Search,
    Palette,
    Form,
}

impl IrisWindow {
    fn active_slot(&self, window: &Window) -> Option<ActiveSlot> {
        let ti = &self.text_inputs;
        if self.overlays.palette_open && ti.palette_focus.is_focused(window) {
            Some(ActiveSlot::Palette)
        } else if ti.search_open && ti.search_focus.is_focused(window) {
            Some(ActiveSlot::Search)
        } else if ti.form_focus.is_focused(window) && ti.active_form_key.is_some() {
            Some(ActiveSlot::Form)
        } else {
            None
        }
    }

    fn slot_ref(&self, slot: ActiveSlot) -> Option<&TextInputState> {
        match slot {
            ActiveSlot::Search => Some(&self.text_inputs.search_input),
            ActiveSlot::Palette => Some(&self.text_inputs.palette_input),
            ActiveSlot::Form => self
                .text_inputs
                .form_fields
                .get(self.text_inputs.active_form_key.as_deref()?),
        }
    }

    fn slot_mut(&mut self, slot: ActiveSlot) -> Option<&mut TextInputState> {
        match slot {
            ActiveSlot::Search => Some(&mut self.text_inputs.search_input),
            ActiveSlot::Palette => Some(&mut self.text_inputs.palette_input),
            ActiveSlot::Form => {
                let buffers = &mut self.text_inputs.buffers;
                buffers
                    .form_fields
                    .get_mut(buffers.active_form_key.as_deref()?)
            }
        }
    }

    fn notify_slot_edited(&mut self, slot: ActiveSlot, cx: &mut Context<Self>) {
        match slot {
            ActiveSlot::Search => self.rebuild_directory_rows(),
            ActiveSlot::Palette => {
                self.overlays.palette_index = 0;
                self.overlays.palette_scroll.scroll_to_item(0);
            }
            ActiveSlot::Form => {
                self.overlays.submitting_plugin_modal = None;
            }
        }
        cx.notify();
    }

    fn focus_form_field(
        &mut self,
        key: String,
        seed: Option<&str>,
        secret: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self
            .text_inputs
            .form_fields
            .entry(key.clone())
            .or_insert_with(|| TextInputState::with_text(seed.unwrap_or("")));
        if secret {
            state.mark_secret();
        }
        self.text_inputs.active_form_key = Some(key);
        self.text_inputs.form_focus.focus(window);
        cx.notify();
    }

    pub(in crate::ui) fn render_text_field(
        &self,
        spec: TextFieldSpec,
        window: Option<&Window>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = ThemeTokens::of(cx);
        let fallback;
        let (state_ref, focused, focus_handle, ctx_tag) = match &spec.kind {
            TextFieldKind::Search => (
                &self.text_inputs.search_input,
                window.is_some_and(|w| self.text_inputs.search_focus.is_focused(w)),
                self.text_inputs.search_focus.clone(),
                "SearchInput",
            ),
            TextFieldKind::Palette => (
                &self.text_inputs.palette_input,
                window.is_some_and(|w| self.text_inputs.palette_focus.is_focused(w)),
                self.text_inputs.palette_focus.clone(),
                "CommandPalette",
            ),
            TextFieldKind::Form {
                key, seed, secret, ..
            } => {
                let active = self.text_inputs.active_form_key.as_deref() == Some(key.as_str());
                let is_foc = window.map_or(active, |w| {
                    active && self.text_inputs.form_focus.is_focused(w)
                });
                let st = if let Some(s) = self.text_inputs.form_fields.get(key) {
                    s
                } else {
                    fallback = TextInputState::with_text(seed.as_deref().unwrap_or(""));
                    &fallback
                };
                if *secret {
                    st.mark_secret();
                }
                (st, is_foc, self.text_inputs.form_focus.clone(), "FormInput")
            }
        };

        let is_empty = state_ref.is_empty();
        let shown = state_ref.display_text(focused);
        let placeholder = spec.placeholder;
        let (kind_click, kind_key) = (spec.kind.clone(), spec.kind);
        let (focus_ime, entity_ime) = (focus_handle.clone(), cx.entity().clone());

        let text_el = if !is_empty {
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_size(px(12.))
                .text_color(t.text)
                .child(shown)
                .into_any_element()
        } else if focused {
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .truncate()
                .text_size(px(12.))
                .child(div().text_color(t.accent).child("▏"))
                .when(!placeholder.is_empty(), |d| {
                    d.child(div().truncate().text_color(t.text_faint).child(placeholder))
                })
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_size(px(12.))
                .text_color(t.text_faint)
                .child(placeholder)
                .into_any_element()
        };

        let mut el = div()
            .id(spec.id)
            .relative()
            .h(spec.height)
            .w_full()
            .px(px(9.))
            .flex()
            .items_center()
            .gap(px(6.))
            .cursor_text()
            .when(!spec.borderless, |d| {
                d.rounded(t.radius_md)
                    .bg(t.window_bg)
                    .border_1()
                    .border_color(if focused { t.accent } else { t.line_strong })
                    .hover(|s| s.border_color(if focused { t.accent } else { t.text_faint }))
            })
            .when_some(spec.leading_icon, |d, g| {
                d.child(icon(g, 14., if focused { t.accent } else { t.text_dim }))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, w, cx| {
                    match &kind_click {
                        TextFieldKind::Search => this.text_inputs.search_focus.focus(w),
                        TextFieldKind::Palette => this.text_inputs.palette_focus.focus(w),
                        TextFieldKind::Form {
                            key, seed, secret, ..
                        } => this.focus_form_field(key.clone(), seed.as_deref(), *secret, w, cx),
                    }
                    cx.stop_propagation();
                    cx.notify();
                }),
            );

        if focused {
            let mut kc = KeyContext::default();
            kc.add(ctx_tag);
            kc.add("TextInput");
            el = el
                .track_focus(&focus_handle)
                .key_context(kc)
                .child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, w, cx| {
                            w.handle_input(
                                &focus_ime,
                                ElementInputHandler::new(bounds, entity_ime.clone()),
                                cx,
                            );
                        },
                    )
                    .absolute()
                    .inset_0(),
                )
                .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, w, cx| {
                    this.handle_text_field_key(&kind_key, ev, w, cx);
                }));
        }
        el.child(text_el).into_any_element()
    }

    fn handle_text_field_key(
        &mut self,
        kind: &TextFieldKind,
        ev: &KeyDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let slot = match kind {
            TextFieldKind::Search => ActiveSlot::Search,
            TextFieldKind::Palette => ActiveSlot::Palette,
            TextFieldKind::Form { key, .. }
                if self.text_inputs.active_form_key.as_deref() == Some(key.as_str()) =>
            {
                ActiveSlot::Form
            }
            TextFieldKind::Form { .. } => return,
        };
        if self
            .slot_ref(slot)
            .is_some_and(|s| s.marked_range.is_some())
        {
            return;
        }
        let mods = ev.keystroke.modifiers;
        if mods.secondary() && !mods.alt {
            match ev.keystroke.key.as_str() {
                "a" => {
                    if let Some(i) = self.slot_mut(slot) {
                        i.select_all();
                    }
                    cx.notify();
                }
                "c" => {
                    if let Some(text) = self.slot_ref(slot).and_then(|i| i.clipboard_text(true)) {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
                    }
                }
                "x" => {
                    if let Some(i) = self.slot_mut(slot)
                        && !i.selected_range.is_empty()
                    {
                        if let Some(text) = i.clipboard_text(false) {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
                        }
                        i.backspace();
                        self.notify_slot_edited(slot, cx);
                    }
                }
                "v" => {
                    if let Some(clip) = cx.read_from_clipboard().and_then(|c| c.text())
                        && let Some(i) = self.slot_mut(slot)
                    {
                        i.insert_paste(&clip);
                        self.notify_slot_edited(slot, cx);
                    }
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        match ev.keystroke.key.as_str() {
            "escape" => {
                match kind {
                    TextFieldKind::Search if !self.text_inputs.search_input.is_empty() => {
                        self.text_inputs.search_input.clear();
                        self.rebuild_directory_rows();
                    }
                    TextFieldKind::Search => {
                        self.text_inputs.search_open = false;
                        self.focus.focus(w);
                    }
                    TextFieldKind::Palette => {
                        self.overlays.palette_open = false;
                        self.text_inputs.palette_input.clear();
                        self.focus.focus(w);
                    }
                    TextFieldKind::Form { key, .. } => {
                        if key.starts_with("modal:") && self.plugins.active_modal().is_some() {
                            self.cancel_active_plugin_modal(Some(w), cx);
                            cx.stop_propagation();
                            return;
                        }
                        self.overlays.dialog = None;
                        self.text_inputs.active_form_key = None;
                        self.focus.focus(w);
                    }
                }
                cx.stop_propagation();
                cx.notify();
            }
            "enter" => {
                match kind {
                    TextFieldKind::Search => self.submit_directory_search(cx),
                    TextFieldKind::Palette => self.execute_palette_selected(w, cx),
                    TextFieldKind::Form {
                        on_enter: Some(cb), ..
                    } => cb(self, w, cx),
                    TextFieldKind::Form { on_enter: None, .. } => {
                        self.text_inputs.active_form_key = None;
                        self.focus.focus(w);
                        cx.notify();
                    }
                }
                cx.stop_propagation();
            }
            "up" | "down" if matches!(kind, TextFieldKind::Palette) => {
                self.move_palette_selection(if ev.keystroke.key == "down" { 1 } else { -1 }, cx);
                cx.stop_propagation();
            }
            "tab" if matches!(kind, TextFieldKind::Form { .. }) => {
                self.cycle_form_field(mods.shift, w, cx);
                cx.stop_propagation();
            }
            "backspace" | "delete" => {
                if let Some(i) = self.slot_mut(slot) {
                    if ev.keystroke.key == "backspace" {
                        i.backspace();
                    } else {
                        i.delete_forward();
                    }
                    self.notify_slot_edited(slot, cx);
                }
                cx.stop_propagation();
            }
            "left" | "right" | "home" | "end" => {
                if let Some(i) = self.slot_mut(slot) {
                    match ev.keystroke.key.as_str() {
                        "left" => i.move_left(),
                        "right" => i.move_right(),
                        "home" => i.move_home(),
                        _ => i.move_end(),
                    }
                    cx.notify();
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }
}

impl EntityInputHandler for IrisWindow {
    fn text_for_range(
        &mut self,
        r16: Range<usize>,
        adj: &mut Option<Range<usize>>,
        w: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let i = self.slot_ref(self.active_slot(w)?)?;
        i.native_text_for_range(r16, adj)
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        w: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let i = self.slot_ref(self.active_slot(w)?)?;
        Some(UTF16Selection {
            range: i.range_to_utf16(&i.selected_range),
            reversed: false,
        })
    }
    fn marked_text_range(&self, w: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let i = self.slot_ref(self.active_slot(w)?)?;
        i.marked_range.as_ref().map(|r| i.range_to_utf16(r))
    }
    fn unmark_text(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = self.active_slot(w)
            && let Some(i) = self.slot_mut(s)
        {
            i.marked_range = None;
            cx.notify();
        }
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(s) = self.active_slot(w) else { return };
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        if let Some(i) = self.slot_mut(s) {
            i.replace_utf16(range, &clean);
        }
        self.notify_slot_edited(s, cx);
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        sel: Option<Range<usize>>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(s) = self.active_slot(w) else { return };
        if let Some(i) = self.slot_mut(s) {
            i.replace_and_mark_utf16(range, text, sel);
        }
        self.notify_slot_edited(s, cx);
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
        let i = self.slot_ref(self.active_slot(w)?)?;
        Some(i.offset_to_utf16(i.text().len()))
    }
}
