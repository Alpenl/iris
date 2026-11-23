//! Unified renderer for all 13 `iris_api::Widget` variants shared between
//! `SidebarPanel` and `PluginModal`, backed by `kit::text_field` and `kit::form`.

use std::rc::Rc;

use gpui::{
    AnyElement, ClipboardItem, Context, Div, ElementId, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Rgba, SharedString, Stateful, StatefulInteractiveElement, Styled,
    Window, div, prelude::FluentBuilder, px, relative, rgb,
};
use iris_api::Widget;

use super::super::IrisWindow;
use super::super::kit::form::{check_indicator, chip_button, stepper_box};
use super::super::kit::text_buffer::format_slider_value;
use super::super::kit::text_field::{FormEnterCallback, TextFieldSpec};
use super::super::kit::{badge_pill, hbetween, hstack, icon, vstack};
use crate::assets::Icon;
use crate::theme::{FONT_MONO, FONT_UI, ThemeTokens};

type ValCallback<T> = Rc<dyn Fn(&mut IrisWindow, T, &mut Window, &mut Context<IrisWindow>)>;

#[derive(Clone)]
pub(super) enum WidgetSurface<'a> {
    Sidebar,
    Modal {
        modal_id: &'a str,
        on_submit: FormEnterCallback,
        window: &'a Window,
    },
}

fn is_qr_dark_module(ch: char) -> bool {
    matches!(ch, '1' | '#' | '█' | '■' | '▓' | 'X' | 'x' | '@')
}

fn step_clamped(cur: f64, delta: f64, min: f64, max: f64) -> f64 {
    let raw = (cur + delta).clamp(min, max);
    ((raw * 1_000_000.0).round() / 1_000_000.0).clamp(min, max)
}

fn sanitize_range(value: f64, min: f64, max: f64, step: f64) -> (f64, f64, f64, f64) {
    const LIMIT: f64 = 1e9;
    let lo = if min.is_finite() {
        min.clamp(-LIMIT, LIMIT)
    } else {
        0.
    };
    let hi = max.clamp(-LIMIT, LIMIT);
    let (lo, hi) = if hi.is_finite() && hi > lo {
        (lo, hi)
    } else if lo <= LIMIT - 100. {
        (lo, lo + 100.)
    } else {
        (LIMIT - 100., LIMIT)
    };
    let st = if step.is_finite() && step.abs() > 1e-6 {
        step.abs().min(LIMIT)
    } else {
        1.0
    };
    (lo, hi, st, finite_value(value, lo, hi, lo))
}

fn finite_value(value: f64, lo: f64, hi: f64, fallback: f64) -> f64 {
    if value.is_finite() {
        value.clamp(lo, hi)
    } else {
        fallback
    }
}

fn txt(s: impl Into<SharedString>, size: f32, col: Rgba) -> Div {
    div().text_size(px(size)).text_color(col).child(s.into())
}

fn trunc_txt(s: impl Into<SharedString>, size: f32, col: Rgba) -> Div {
    txt(s, size, col).truncate()
}

fn card_box(gap: f32, t: ThemeTokens) -> Div {
    vstack(gap)
        .p(px(10.))
        .rounded(t.radius_md)
        .bg(t.window_bg)
        .border_1()
        .border_color(t.line)
}

fn row_box(id: impl Into<ElementId>, bg: Rgba, border: Rgba, t: ThemeTokens) -> Stateful<Div> {
    hbetween(8.)
        .id(id)
        .rounded(t.radius_md)
        .cursor_pointer()
        .bg(bg)
        .border_1()
        .border_color(border)
        .hover(|s| s.bg(t.hover))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

fn progress_bar(ratio: f32, t: ThemeTokens) -> Div {
    div()
        .h(px(4.))
        .w_full()
        .bg(t.line)
        .child(div().h_full().w(relative(ratio)).bg(t.accent))
}

fn dispatch_widget_value(
    this: &mut IrisWindow,
    target: (&str, &str, Option<&str>, Option<&str>),
    next_val: String,
    w: &mut Window,
    cx: &mut Context<IrisWindow>,
) {
    let (pid, id, action_id, modal_key) = target;
    let explicit_act = action_id.filter(|a| !a.is_empty());
    if let Some(mk) = modal_key {
        this.overlays.submitting_plugin_modal = None;
        this.set_form_value(mk, next_val.clone());
        if let Some(act) = explicit_act {
            this.invoke_plugin_widget_action(pid, act, Some(next_val), w, cx);
        } else {
            cx.notify();
        }
    } else {
        this.invoke_plugin_widget_action(pid, explicit_act.unwrap_or(id), Some(next_val), w, cx);
    }
}

fn icon_pill_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    glyph: Option<Icon>,
    hi: bool,
    height: f32,
    rest_bg: Rgba,
    t: ThemeTokens,
) -> Stateful<Div> {
    hstack(6.)
        .id(id)
        .h(px(height))
        .px(px(9.))
        .rounded(t.radius_md)
        .cursor_pointer()
        .border_1()
        .border_color(if hi { t.accent } else { t.line_strong })
        .bg(if hi { t.selected } else { rest_bg })
        .text_size(px(11.))
        .text_color(if hi { t.text } else { t.text_muted })
        .hover(|s| s.bg(t.hover).text_color(t.text))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .when_some(glyph, |d, g| {
            d.child(icon(g, 12., if hi { t.accent } else { t.text_dim }))
        })
        .child(label.into())
}

fn render_qr_widget(
    base_id: usize,
    is_modal: bool,
    (label, data, matrix, hint): (String, String, Vec<String>, Option<String>),
    t: ThemeTokens,
    cx: &Context<IrisWindow>,
) -> AnyElement {
    let parsed_rows: Vec<Vec<bool>> = matrix
        .iter()
        .filter(|r| !r.is_empty())
        .take(96)
        .map(|row| row.chars().take(96).map(is_qr_dark_module).collect())
        .collect();
    let max_cols = parsed_rows.iter().map(Vec::len).max().unwrap_or(0);
    let dim = parsed_rows.len().max(max_cols).max(1);
    let cell = ((if is_modal { 168.0 } else { 148.0 }) / dim as f32)
        .floor()
        .clamp(2.0, 8.0);
    let copy_payload = data.clone();

    let grid = (!parsed_rows.is_empty() && max_cols > 0).then(|| {
        let rows = parsed_rows.into_iter().map(|row| {
            let cells = (0..max_cols).map(|ci| {
                let dark = row.get(ci).copied().unwrap_or(false);
                div().size(px(cell)).when(dark, |m| m.bg(rgb(0x000000)))
            });
            div().flex().h(px(cell)).children(cells)
        });
        let sheet = vstack(0.)
            .p(px(10.))
            .bg(rgb(0xFFFFFF))
            .border_1()
            .border_color(t.line_strong)
            .children(rows);
        hstack(0.).w_full().justify_center().py(px(4.)).child(sheet)
    });
    let copy_row = (!data.trim().is_empty()).then(|| {
        let btn = chip_button(("pl-qr-copy", base_id), "复制内容", false, t).on_click(cx.listener(
            move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_payload.clone()));
                this.show_toast("二维码内容已复制到剪贴板", cx);
            },
        ));
        let url_txt = trunc_txt(data, 10., t.text_dim)
            .flex_1()
            .min_w(px(0.))
            .font_family(FONT_MONO);
        hbetween(8.).child(url_txt).child(btn)
    });

    card_box(8., t)
        .when(!label.is_empty(), |d| {
            d.child(txt(label, 11., t.text_muted))
        })
        .children(grid)
        .children(copy_row)
        .when_some(hint.filter(|h| !h.trim().is_empty()), |d, h| {
            d.child(txt(h, 11., t.text_dim))
        })
        .into_any_element()
}

pub(super) fn render_plugin_widget(
    win: &IrisWindow,
    pid: &str,
    base_id: usize,
    widget: Widget,
    surface: WidgetSurface<'_>,
    t: ThemeTokens,
    cx: &mut Context<IrisWindow>,
) -> AnyElement {
    let is_modal = matches!(surface, WidgetSurface::Modal { .. });
    let rest_bg = if is_modal { t.window_bg } else { t.panel };
    let pid_owned = pid.to_string();
    let modal_key = |id: &str| match &surface {
        WidgetSurface::Modal { modal_id, .. } => Some(format!("modal:{pid}:{modal_id}:{id}")),
        WidgetSurface::Sidebar => None,
    };

    let content = match widget {
        Widget::Label { text, muted, mono } => {
            txt(text, 11., if muted { t.text_dim } else { t.text })
                .font_family(if mono { FONT_MONO } else { FONT_UI })
                .when(is_modal, |d| {
                    d.p(px(8.))
                        .rounded(t.radius_md)
                        .bg(t.window_bg)
                        .border_1()
                        .border_color(t.line)
                })
                .into_any_element()
        }

        Widget::Stat {
            label,
            value,
            accent,
        } => hbetween(8.)
            .child(txt(label, 11., t.text_dim))
            .child(txt(value, 11., if accent { t.accent } else { t.text }).font_family(FONT_MONO))
            .into_any_element(),

        Widget::Progress { label, ratio } => {
            let pct = if ratio.is_finite() {
                ratio.clamp(0., 1.)
            } else {
                0.
            };
            vstack(4.)
                .when(!label.is_empty(), |d| d.child(txt(label, 10., t.text_dim)))
                .child(progress_bar(pct, t))
                .into_any_element()
        }

        Widget::Slider {
            id,
            label,
            value,
            min,
            max,
            step,
            unit,
            action_id,
        } => {
            let (lo, hi, st, fallback) = sanitize_range(value, min, max, step);
            let mk = modal_key(&id);
            let cur_val = mk
                .as_ref()
                .and_then(|k| win.text_inputs.form_fields.get(k))
                .and_then(|s| s.text().trim().parse::<f64>().ok())
                .map_or(fallback, |v| finite_value(v, lo, hi, fallback));
            let ratio = ((cur_val - lo) / (hi - lo)).clamp(0.0, 1.0) as f32;
            let val_str = format_slider_value(cur_val, st);
            let readout = unit
                .filter(|u| !u.is_empty())
                .map_or(val_str.clone(), |u| format!("{val_str}{u}"));

            let step_cb: ValCallback<f64> = Rc::new(move |this, delta, w, cx| {
                let base = mk
                    .as_deref()
                    .and_then(|k| this.text_inputs.form_fields.get(k))
                    .and_then(|s| s.text().trim().parse::<f64>().ok())
                    .map_or(cur_val, |v| finite_value(v, lo, hi, cur_val));
                let next_s = format_slider_value(step_clamped(base, delta, lo, hi), st);
                let target = (&pid_owned[..], &id[..], Some(&action_id[..]), mk.as_deref());
                dispatch_widget_value(this, target, next_s, w, cx);
            });
            let dec_cb = step_cb.clone();
            let controls = stepper_box(
                ("pl-sldr-dec", base_id),
                ("pl-sldr-inc", base_id),
                readout,
                rest_bg,
                t,
                cx.listener(move |this, _, w, cx| dec_cb(this, -st, w, cx)),
                cx.listener(move |this, _, w, cx| step_cb(this, st, w, cx)),
            );
            let lbl = trunc_txt(label, 11., if is_modal { t.text_muted } else { t.text_dim })
                .flex_1()
                .min_w(px(0.));
            vstack(5.)
                .child(hbetween(8.).child(lbl).child(controls))
                .child(progress_bar(ratio, t))
                .into_any_element()
        }

        Widget::Button {
            label,
            action_id,
            icon: icon_name,
            accent,
        } => icon_pill_button(
            ("pl-btn", base_id),
            label,
            icon_name.as_deref().and_then(Icon::from_name),
            accent,
            26.,
            rest_bg,
            t,
        )
        .on_click(cx.listener(move |this, _, w, cx| {
            this.invoke_plugin_action(&pid_owned, &action_id, w, cx)
        }))
        .into_any_element(),

        Widget::ButtonGroup { buttons } => div()
            .flex()
            .flex_wrap()
            .gap(px(4.))
            .children(buttons.into_iter().enumerate().map(|(bi, btn)| {
                let (pid_b, act) = (pid_owned.clone(), btn.action_id);
                let g = btn.icon.as_deref().and_then(Icon::from_name);
                icon_pill_button(("pl-gbtn", bi), btn.label, g, btn.accent, 24., rest_bg, t)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.invoke_plugin_action(&pid_b, &act, w, cx)
                    }))
            }))
            .into_any_element(),

        Widget::Input {
            id,
            label,
            value,
            placeholder,
            secret,
            action_id,
        } => {
            let lbl = label.filter(|l| !l.is_empty());
            match surface {
                WidgetSurface::Modal {
                    modal_id,
                    on_submit,
                    window,
                } => {
                    let key = format!("modal:{pid_owned}:{modal_id}:{id}");
                    let spec =
                        TextFieldSpec::form(key, Some(value), placeholder, secret, Some(on_submit));
                    vstack(5.)
                        .when_some(lbl, |d, l| d.child(txt(l, 11., t.text_muted)))
                        .child(win.render_text_field(spec, Some(window), cx))
                        .into_any_element()
                }
                WidgetSurface::Sidebar => {
                    let field_key = format!("sb:{pid_owned}:{id}");
                    let seed_sub = value.clone();
                    let target_act = action_id.filter(|a| !a.is_empty()).unwrap_or(id);
                    let fk_sub = field_key.clone();
                    let submit_cb: FormEnterCallback = Rc::new(move |this, w, cx| {
                        let had_modal = this.plugins.active_modal().is_some();
                        let val = this
                            .text_inputs
                            .form_fields
                            .get(&fk_sub)
                            .map_or_else(|| seed_sub.clone(), |s| s.text().to_string());
                        this.invoke_plugin_widget_action(&pid_owned, &target_act, Some(val), w, cx);
                        if had_modal || this.plugins.active_modal().is_none() {
                            this.text_inputs.active_form_key = None;
                            this.focus.focus(w);
                        }
                        cx.notify();
                    });
                    let btn_cb = submit_cb.clone();
                    let spec = TextFieldSpec::form(
                        field_key,
                        Some(value),
                        placeholder,
                        secret,
                        Some(submit_cb),
                    );
                    let btn = chip_button(("pl-inp-sub", base_id), "提交", false, t)
                        .h(px(30.))
                        .on_click(cx.listener(move |this, _, w, cx| btn_cb(this, w, cx)));
                    let field = div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(win.render_text_field(spec, None, cx));
                    vstack(4.)
                        .when_some(lbl, |d, l| d.child(txt(l, 10., t.text_dim)))
                        .child(hstack(4.).child(field).child(btn))
                        .into_any_element()
                }
            }
        }

        Widget::Toggle {
            id,
            label,
            checked,
            action_id,
        } => {
            let mk = modal_key(&id);
            let cur = mk
                .as_ref()
                .and_then(|k| win.text_inputs.form_fields.get(k))
                .map_or(checked, |s| s.text() == "true");
            let border = match (cur, is_modal) {
                (true, _) => t.accent,
                (_, true) => t.line_strong,
                _ => t.line,
            };
            let lbl_col = if cur || is_modal {
                t.text
            } else {
                t.text_muted
            };
            let lbl = trunc_txt(label, if is_modal { 12. } else { 11. }, lbl_col)
                .flex_1()
                .min_w(px(0.));
            row_box(
                SharedString::from(format!("pl-tgl:{base_id}:{id}")),
                rest_bg,
                border,
                t,
            )
            .h(px(if is_modal { 30. } else { 26. }))
            .px(px(if is_modal { 10. } else { 8. }))
            .on_click(cx.listener(move |this, _, w, cx| {
                let next_s = if !cur { "true" } else { "false" };
                let target = (&pid_owned[..], &id[..], action_id.as_deref(), mk.as_deref());
                dispatch_widget_value(this, target, next_s.into(), w, cx);
            }))
            .child(lbl)
            .child(check_indicator(cur, 14., t))
            .into_any_element()
        }

        Widget::Select {
            id,
            label,
            selected,
            options,
            action_id,
        } => {
            let mk = modal_key(&id);
            let cur_sel = mk
                .as_ref()
                .and_then(|k| win.text_inputs.form_fields.get(k))
                .map_or(selected, |s| s.text().to_string());
            let on_sel: ValCallback<String> = Rc::new(move |this, val, w, cx| {
                let target = (&pid_owned[..], &id[..], action_id.as_deref(), mk.as_deref());
                dispatch_widget_value(this, target, val, w, cx);
            });
            let chips = options.into_iter().enumerate().map(|(oi, opt)| {
                let (cb, val) = (on_sel.clone(), opt.value);
                chip_button(("pl-sel", oi), opt.label, val == cur_sel, t)
                    .h(px(if is_modal { 26. } else { 24. }))
                    .on_click(cx.listener(move |this, _, w, cx| cb(this, val.clone(), w, cx)))
            });
            let lbl_col = if is_modal { t.text_muted } else { t.text_dim };
            vstack(4.)
                .when_some(label.filter(|l| !l.is_empty()), |d, l| {
                    d.child(txt(l, if is_modal { 11. } else { 10. }, lbl_col))
                })
                .child(div().flex().flex_wrap().gap(px(4.)).children(chips))
                .into_any_element()
        }

        Widget::ListItem {
            id,
            title,
            subtitle,
            badge,
            active,
            action_id,
        } => {
            let t_col = if active { t.text } else { t.text_muted };
            let text_col = vstack(1.)
                .flex_1()
                .min_w(px(0.))
                .child(trunc_txt(title, 11., t_col))
                .when_some(subtitle.filter(|s| !s.is_empty()), |d, sub| {
                    d.child(trunc_txt(sub, 10., t.text_faint))
                });
            let (bg, border) = if active {
                (t.selected, t.accent)
            } else {
                (rest_bg, t.line)
            };
            let rid = SharedString::from(format!("pl-li:{base_id}:{id}"));
            row_box(rid, bg, border, t)
                .px(px(8.))
                .py(px(6.))
                .on_click(cx.listener(move |this, _, w, cx| {
                    this.invoke_plugin_action(&pid_owned, &action_id, w, cx)
                }))
                .child(text_col)
                .when_some(badge.filter(|b| !b.is_empty()), |d, b| {
                    d.child(badge_pill(b, active, t))
                })
                .into_any_element()
        }

        Widget::QrCode {
            label,
            data,
            matrix,
            hint,
        } => render_qr_widget(base_id, is_modal, (label, data, matrix, hint), t, cx),

        Widget::Link {
            label,
            url,
            button_text,
        } => {
            let copy_url = url.clone();
            let btn_lbl = button_text
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("复制链接")
                .to_string();
            let btn_id = ("pl-lnk-copy", base_id);
            let copy_btn =
                icon_pill_button(btn_id, btn_lbl, Some(Icon::Link), true, 26., t.panel, t)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_url.clone()));
                        this.show_toast("已复制链接到剪贴板", cx);
                    }));
            card_box(6., t)
                .when(!label.is_empty(), |d| {
                    d.child(txt(label, 11., t.text_muted))
                })
                .child(trunc_txt(url, 10., t.text_dim).font_family(FONT_MONO))
                .child(hstack(0.).child(copy_btn))
                .into_any_element()
        }

        Widget::Divider => div()
            .h(px(1.))
            .w_full()
            .my(px(2.))
            .bg(t.line)
            .into_any_element(),
    };
    div()
        .id(("plugin-widget", base_id))
        .child(content)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{finite_value, sanitize_range, step_clamped};

    #[test]
    fn slider_extreme_ranges_and_invalid_drafts_stay_finite() {
        for (min, max) in [
            (-1e308, 1e308),
            (1e308, 1e308),
            (f64::NAN, f64::NAN),
            (100., -100.),
        ] {
            let (lo, hi, step, value) = sanitize_range(1e308, min, max, 1e308);
            assert!(lo < hi && lo.abs() <= 1e9 && hi.abs() <= 1e9);
            assert!((0. ..=1.).contains(&((value - lo) / (hi - lo))));
            assert_eq!(finite_value(f64::NAN, lo, hi, value), value);
            assert!((lo..=hi).contains(&step_clamped(value, step, lo, hi)));
        }
    }
}
