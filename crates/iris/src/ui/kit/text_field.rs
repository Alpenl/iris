//! Centralized IME single-line text input hub (`TextInputHub`), `TextFieldHost` trait,
//! and the single shared `render_text_field` widget across Search, Palette, and Forms.

use std::ops::{Deref, DerefMut, Range};
use std::rc::Rc;

use gpui::{
    AnyElement, App, ClipboardItem, ContentMask, Context, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, InteractiveElement, IntoElement, KeyContext, KeyDownEvent,
    MouseButton, ParentElement, Pixels, SharedString, Styled, TextRun, UTF16Selection, Window,
    canvas, div, prelude::FluentBuilder, px,
};

use super::icon;
pub(in crate::ui) use super::text_buffer::{TextInputBuffers, TextInputState};
use crate::assets::Icon;
use crate::platform::IS_MACOS;
use crate::theme::ThemeTokens;

pub(in crate::ui) type FormEnterCallback<H> = Rc<dyn Fn(&mut H, &mut Window, &mut Context<H>)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui) enum ActiveSlot {
    Search,
    Palette,
    Form,
}

pub(in crate::ui) trait TextFieldHost: EntityInputHandler + Sized + 'static {
    fn text_inputs(&self) -> &TextInputHub;
    fn text_inputs_mut(&mut self) -> &mut TextInputHub;
    fn is_palette_open(&self) -> bool;
    fn focus_root(&self, window: &mut Window);
    fn on_slot_edited(&mut self, slot: ActiveSlot, cx: &mut Context<Self>);
    fn on_slot_visual_change(&mut self, slot: ActiveSlot, cx: &mut Context<Self>);
    fn on_escape(&mut self, window: &mut Window, cx: &mut Context<Self>);
    fn on_search_enter(&mut self, cx: &mut Context<Self>);
    fn on_palette_enter(&mut self, window: &mut Window, cx: &mut Context<Self>);
    fn on_palette_move(&mut self, delta: isize, cx: &mut Context<Self>);
    fn on_form_tab(&mut self, reverse: bool, window: &mut Window, cx: &mut Context<Self>);
}

pub(in crate::ui) struct TextInputHub {
    pub(in crate::ui) search_focus: FocusHandle,
    pub(in crate::ui) palette_focus: FocusHandle,
    pub(in crate::ui) form_focus: FocusHandle,
    pub(in crate::ui) recorder_focus: FocusHandle,
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
    pub(in crate::ui) fn new<H: 'static>(cx: &mut Context<H>) -> Self {
        Self {
            search_focus: cx.focus_handle(),
            palette_focus: cx.focus_handle(),
            form_focus: cx.focus_handle(),
            recorder_focus: cx.focus_handle(),
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

pub(in crate::ui) enum TextFieldKind<H> {
    Search,
    Palette,
    Form {
        key: String,
        seed: Option<String>,
        secret: bool,
        on_enter: Option<FormEnterCallback<H>>,
    },
}

impl<H> Clone for TextFieldKind<H> {
    fn clone(&self) -> Self {
        match self {
            Self::Search => Self::Search,
            Self::Palette => Self::Palette,
            Self::Form {
                key,
                seed,
                secret,
                on_enter,
            } => Self::Form {
                key: key.clone(),
                seed: seed.clone(),
                secret: *secret,
                on_enter: on_enter.clone(),
            },
        }
    }
}

pub(in crate::ui) struct TextFieldSpec<H> {
    pub(in crate::ui) id: SharedString,
    pub(in crate::ui) kind: TextFieldKind<H>,
    pub(in crate::ui) placeholder: SharedString,
    pub(in crate::ui) height: Pixels,
    pub(in crate::ui) leading_icon: Option<Icon>,
    pub(in crate::ui) borderless: bool,
}

impl<H> TextFieldSpec<H> {
    pub(in crate::ui) fn form(
        key: String,
        seed: Option<String>,
        placeholder: impl Into<SharedString>,
        secret: bool,
        on_enter: Option<FormEnterCallback<H>>,
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

fn active_slot<H: TextFieldHost>(host: &H, window: &Window) -> Option<ActiveSlot> {
    let ti = host.text_inputs();
    if host.is_palette_open() && ti.palette_focus.is_focused(window) {
        Some(ActiveSlot::Palette)
    } else if ti.search_open && ti.search_focus.is_focused(window) {
        Some(ActiveSlot::Search)
    } else if ti.form_focus.is_focused(window) && ti.active_form_key.is_some() {
        Some(ActiveSlot::Form)
    } else {
        None
    }
}

fn slot_ref<H: TextFieldHost>(host: &H, slot: ActiveSlot) -> Option<&TextInputState> {
    let ti = host.text_inputs();
    match slot {
        ActiveSlot::Search => Some(&ti.search_input),
        ActiveSlot::Palette => Some(&ti.palette_input),
        ActiveSlot::Form => ti.form_fields.get(ti.active_form_key.as_deref()?),
    }
}

fn slot_mut<H: TextFieldHost>(host: &mut H, slot: ActiveSlot) -> Option<&mut TextInputState> {
    let buffers = &mut host.text_inputs_mut().buffers;
    match slot {
        ActiveSlot::Search => Some(&mut buffers.search_input),
        ActiveSlot::Palette => Some(&mut buffers.palette_input),
        ActiveSlot::Form => buffers
            .form_fields
            .get_mut(buffers.active_form_key.as_deref()?),
    }
}

fn focus_form_field<H: TextFieldHost>(
    host: &mut H,
    key: String,
    seed: Option<&str>,
    secret: bool,
    window: &mut Window,
    cx: &mut Context<H>,
) {
    let ti = host.text_inputs_mut();
    let state = ti
        .form_fields
        .entry(key.clone())
        .or_insert_with(|| TextInputState::with_text(seed.unwrap_or("")));
    if secret {
        state.mark_secret();
    }
    ti.active_form_key = Some(key);
    let handle = ti.form_focus.clone();
    handle.focus(window);
    host.on_slot_visual_change(ActiveSlot::Form, cx);
}

pub(in crate::ui) fn render_text_field<H: TextFieldHost>(
    host_entity: &Entity<H>,
    spec: TextFieldSpec<H>,
    window: Option<&Window>,
    cx: &App,
) -> AnyElement {
    let t = ThemeTokens::of(cx);
    let host = host_entity.read(cx);
    let ti = host.text_inputs();
    let fallback;
    let (state_ref, focused, focus_handle, ctx_tag) = match &spec.kind {
        TextFieldKind::Search => (
            &ti.search_input,
            window.is_some_and(|w| ti.search_focus.is_focused(w)),
            ti.search_focus.clone(),
            "SearchInput",
        ),
        TextFieldKind::Palette => (
            &ti.palette_input,
            window.is_some_and(|w| ti.palette_focus.is_focused(w)),
            ti.palette_focus.clone(),
            "CommandPalette",
        ),
        TextFieldKind::Form {
            key, seed, secret, ..
        } => {
            let active = ti.active_form_key.as_deref() == Some(key.as_str());
            let is_foc = window.map_or(active, |w| active && ti.form_focus.is_focused(w));
            let st = if let Some(s) = ti.form_fields.get(key) {
                s
            } else {
                fallback = TextInputState::with_text(seed.as_deref().unwrap_or(""));
                &fallback
            };
            if *secret {
                st.mark_secret();
            }
            (st, is_foc, ti.form_focus.clone(), "FormInput")
        }
    };

    let is_empty = state_ref.is_empty();
    let shown = state_ref.display_text(focused);
    let placeholder = spec.placeholder;
    let (kind_click, kind_key) = (spec.kind.clone(), spec.kind);
    let (focus_ime, entity_ime) = (focus_handle.clone(), host_entity.clone());
    let weak_click = host_entity.downgrade();
    let weak_key = host_entity.downgrade();

    let text_el = if focused && !is_empty {
        let caret = state_ref.display_caret_index();
        let shown = SharedString::from(shown);
        let font_size = t.text_md;
        let line_height = px((f32::from(font_size) * 1.45).ceil());
        canvas(
            move |_, w, _| {
                let run = TextRun {
                    len: shown.len(),
                    font: w.text_style().font(),
                    color: t.text.into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                w.text_system().shape_line(shown, font_size, &[run], None)
            },
            move |bounds, line, w, cx| {
                let right = line.x_for_index(caret + '▏'.len_utf8());
                let mut origin = bounds.origin;
                origin.x -= (right - bounds.size.width).max(px(0.));
                origin.y += (bounds.size.height - line_height) * 0.5;
                w.with_content_mask(Some(ContentMask { bounds }), |w| {
                    let _ = line.paint(origin, line_height, w, cx);
                });
            },
        )
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .into_any_element()
    } else if !is_empty {
        div()
            .flex_1()
            .min_w(px(0.))
            .truncate()
            .text_size(t.text_md)
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
            .text_size(t.text_md)
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
            .text_size(t.text_md)
            .text_color(t.text_faint)
            .child(placeholder)
            .into_any_element()
    };

    let field_h = px(f32::from(spec.height).max(f32::from(t.control_h_md)));
    let mut el = div()
        .id(spec.id)
        .relative()
        .h(field_h)
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
        .on_mouse_down(MouseButton::Left, move |_, w, cx| {
            let _ = weak_click.update(cx, |this, cx| match &kind_click {
                TextFieldKind::Search => {
                    this.text_inputs().search_focus.focus(w);
                    this.on_slot_visual_change(ActiveSlot::Search, cx);
                }
                TextFieldKind::Palette => {
                    this.text_inputs().palette_focus.focus(w);
                    this.on_slot_visual_change(ActiveSlot::Palette, cx);
                }
                TextFieldKind::Form {
                    key, seed, secret, ..
                } => focus_form_field(this, key.clone(), seed.as_deref(), *secret, w, cx),
            });
            cx.stop_propagation();
        });

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
            .on_key_down(move |ev: &KeyDownEvent, w, cx| {
                let _ = weak_key.update(cx, |this, cx| {
                    handle_text_field_key(this, &kind_key, ev, w, cx);
                });
            });
    }
    el.child(text_el).into_any_element()
}

fn handle_text_field_key<H: TextFieldHost>(
    host: &mut H,
    kind: &TextFieldKind<H>,
    ev: &KeyDownEvent,
    w: &mut Window,
    cx: &mut Context<H>,
) {
    let slot = match kind {
        TextFieldKind::Search => ActiveSlot::Search,
        TextFieldKind::Palette => ActiveSlot::Palette,
        TextFieldKind::Form { key, .. }
            if host.text_inputs().active_form_key.as_deref() == Some(key.as_str()) =>
        {
            ActiveSlot::Form
        }
        TextFieldKind::Form { .. } => return,
    };
    if slot_ref(host, slot).is_some_and(|s| s.marked_range.is_some()) {
        return;
    }
    let mods = ev.keystroke.modifiers;
    let key = ev.keystroke.key.as_str();
    let edit = match key {
        "a" | "c" | "x" if mods.secondary() && !mods.alt && !mods.shift => Some(key),
        "v" if mods.secondary() && !mods.alt => Some("v"),
        "insert" if !IS_MACOS && !mods.alt && !mods.platform && mods.control && !mods.shift => {
            Some("c")
        }
        "insert" if !IS_MACOS && !mods.alt && !mods.platform && !mods.control && mods.shift => {
            Some("v")
        }
        "delete" if !IS_MACOS && !mods.alt && !mods.platform && !mods.control && mods.shift => {
            Some("x")
        }
        _ => None,
    };
    if let Some(edit) = edit {
        match edit {
            "a" => {
                if let Some(i) = slot_mut(host, slot) {
                    i.select_all();
                }
                host.on_slot_visual_change(slot, cx);
            }
            "c" => {
                if let Some(text) = slot_ref(host, slot).and_then(|i| i.clipboard_text(true)) {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
                }
            }
            "x" => {
                if let Some(i) = slot_mut(host, slot)
                    && let Some(text) = i.clipboard_text(false)
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
                    i.backspace();
                    host.on_slot_edited(slot, cx);
                }
            }
            "v" => {
                if let Some(clip) = cx.read_from_clipboard().and_then(|c| c.text())
                    && let Some(i) = slot_mut(host, slot)
                {
                    i.insert_paste(&clip);
                    host.on_slot_edited(slot, cx);
                }
            }
            _ => unreachable!("unknown text editing shortcut"),
        }
        cx.stop_propagation();
        return;
    }
    if mods.alt || mods.control || mods.platform {
        return;
    }
    match ev.keystroke.key.as_str() {
        "escape" => {
            host.on_escape(w, cx);
            cx.stop_propagation();
        }
        "enter" => {
            match kind {
                TextFieldKind::Search => host.on_search_enter(cx),
                TextFieldKind::Palette => host.on_palette_enter(w, cx),
                TextFieldKind::Form {
                    on_enter: Some(cb), ..
                } => cb(host, w, cx),
                TextFieldKind::Form { on_enter: None, .. } => {
                    host.on_slot_visual_change(ActiveSlot::Form, cx);
                    host.text_inputs_mut().active_form_key = None;
                    host.focus_root(w);
                }
            }
            cx.stop_propagation();
        }
        "up" | "down" if matches!(kind, TextFieldKind::Palette) => {
            host.on_palette_move(if ev.keystroke.key == "down" { 1 } else { -1 }, cx);
            cx.stop_propagation();
        }
        "tab" if matches!(kind, TextFieldKind::Form { .. }) => {
            host.on_form_tab(mods.shift, w, cx);
            cx.stop_propagation();
        }
        "backspace" | "delete" => {
            if let Some(i) = slot_mut(host, slot) {
                if ev.keystroke.key == "backspace" {
                    i.backspace();
                } else {
                    i.delete_forward();
                }
                host.on_slot_edited(slot, cx);
            }
            cx.stop_propagation();
        }
        "left" | "right" | "home" | "end" => {
            if let Some(i) = slot_mut(host, slot) {
                match ev.keystroke.key.as_str() {
                    "left" => i.move_left(),
                    "right" => i.move_right(),
                    "home" => i.move_home(),
                    _ => i.move_end(),
                }
                host.on_slot_visual_change(slot, cx);
            }
            cx.stop_propagation();
        }
        _ => {}
    }
}

pub(in crate::ui) fn ime_text_for_range<H: TextFieldHost>(
    host: &H,
    r16: Range<usize>,
    adj: &mut Option<Range<usize>>,
    w: &Window,
) -> Option<String> {
    let i = slot_ref(host, active_slot(host, w)?)?;
    i.native_text_for_range(r16, adj)
}

pub(in crate::ui) fn ime_selected_text_range<H: TextFieldHost>(
    host: &H,
    w: &Window,
) -> Option<UTF16Selection> {
    let i = slot_ref(host, active_slot(host, w)?)?;
    Some(UTF16Selection {
        range: i.range_to_utf16(&i.selected_range),
        reversed: false,
    })
}

pub(in crate::ui) fn ime_marked_text_range<H: TextFieldHost>(
    host: &H,
    w: &Window,
) -> Option<Range<usize>> {
    let i = slot_ref(host, active_slot(host, w)?)?;
    i.marked_range.as_ref().map(|r| i.range_to_utf16(r))
}

pub(in crate::ui) fn ime_unmark_text<H: TextFieldHost>(
    host: &mut H,
    w: &Window,
    cx: &mut Context<H>,
) {
    if let Some(s) = active_slot(host, w)
        && let Some(i) = slot_mut(host, s)
    {
        i.marked_range = None;
        host.on_slot_visual_change(s, cx);
    }
}

pub(in crate::ui) fn ime_replace_text_in_range<H: TextFieldHost>(
    host: &mut H,
    range: Option<Range<usize>>,
    text: &str,
    w: &Window,
    cx: &mut Context<H>,
) {
    let Some(s) = active_slot(host, w) else {
        return;
    };
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if let Some(i) = slot_mut(host, s) {
        i.replace_utf16(range, &clean);
    }
    host.on_slot_edited(s, cx);
}

pub(in crate::ui) fn ime_replace_and_mark_text_in_range<H: TextFieldHost>(
    host: &mut H,
    range: Option<Range<usize>>,
    text: &str,
    sel: Option<Range<usize>>,
    w: &Window,
    cx: &mut Context<H>,
) {
    let Some(s) = active_slot(host, w) else {
        return;
    };
    if let Some(i) = slot_mut(host, s) {
        i.replace_and_mark_utf16(range, text, sel);
    }
    host.on_slot_edited(s, cx);
}

pub(in crate::ui) fn ime_character_index_for_point<H: TextFieldHost>(
    host: &H,
    w: &Window,
) -> Option<usize> {
    let i = slot_ref(host, active_slot(host, w)?)?;
    Some(i.offset_to_utf16(i.text().len()))
}
