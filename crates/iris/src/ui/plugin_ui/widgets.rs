//! Unified renderer for all 13 `iris_api::Widget` variants shared between
//! `SidebarPanel` and `PluginModal`, backed by `kit::text_field` and `kit::form`.

use std::rc::Rc;

use gpui::{
    AnyElement, Bounds, ClipboardItem, Context, Div, ElementId, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Rgba, SharedString, Stateful, StatefulInteractiveElement,
    Styled, Window, canvas, div, fill, point, prelude::FluentBuilder, px, relative, rgb, size,
};
use iris_api::Widget;

use super::super::IrisWindow;
use super::super::kit::form::{check_indicator, chip_button, stepper_box};
use super::super::kit::text_buffer::format_slider_value;
use super::super::kit::text_field::{FormEnterCallback, TextFieldSpec};
use super::super::kit::{badge_pill, hbetween, hstack, icon, surface_card, vstack};
use crate::assets::Icon;
use crate::theme::{FONT_MONO, FONT_UI, ThemeTokens};

type ValCallback<T> = Rc<dyn Fn(&mut IrisWindow, T, &mut Window, &mut Context<IrisWindow>)>;

#[derive(Clone)]
pub(super) enum WidgetSurface<'a> {
    Sidebar {
        window: &'a Window,
    },
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

fn txt(s: impl Into<SharedString>, size: Pixels, col: Rgba) -> Div {
    div().text_size(size).text_color(col).child(s.into())
}

fn trunc_txt(s: impl Into<SharedString>, size: Pixels, col: Rgba) -> Div {
    txt(s, size, col).truncate()
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
        this.plugin_view.modal.submitting = None;
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
    height: Pixels,
    rest_bg: Rgba,
    t: ThemeTokens,
) -> Stateful<Div> {
    hstack(6.)
        .id(id)
        .h(height)
        .px(px(9.))
        .rounded(t.radius_md)
        .cursor_pointer()
        .border_1()
        .border_color(if hi { t.accent } else { t.line_strong })
        .bg(if hi { t.selected } else { rest_bg })
        .text_size(t.text_sm)
        .text_color(if hi { t.text } else { t.text_muted })
        .hover(|s| s.bg(t.hover).text_color(t.text))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .when_some(glyph, |d, g| {
            d.child(icon(g, 12., if hi { t.accent } else { t.text_dim }))
        })
        .child(label.into())
}

/// ISO/IEC 18004 requires at least 4 modules of pure-white quiet zone on every side.
const QR_QUIET_MODULES: usize = 4;

struct QrMatrix {
    width: usize,
    height: usize,
    /// Per-row coalesced `[start_col, end_col)` dark runs so adjacent modules share exact
    /// edges and paint without horizontal seams or thousands of layout nodes.
    dark_runs: Vec<Vec<(usize, usize)>>,
}

impl QrMatrix {
    fn from_lines(matrix: &[String]) -> Option<Self> {
        let grid: Vec<Vec<bool>> = matrix
            .iter()
            .filter(|r| !r.is_empty())
            .take(96)
            .map(|row| row.chars().take(96).map(is_qr_dark_module).collect())
            .collect();
        let (mut min_r, mut max_r) = (usize::MAX, 0usize);
        let (mut min_c, mut max_c) = (usize::MAX, 0usize);
        for (ry, row) in grid.iter().enumerate() {
            for (cx, &dark) in row.iter().enumerate() {
                if dark {
                    min_r = min_r.min(ry);
                    max_r = max_r.max(ry);
                    min_c = min_c.min(cx);
                    max_c = max_c.max(cx);
                }
            }
        }
        if min_r > max_r || min_c > max_c {
            return None;
        }
        let width = max_c - min_c + 1;
        let height = max_r - min_r + 1;
        let mut dark_runs = Vec::with_capacity(height);
        for row in &grid[min_r..=max_r] {
            let mut runs = Vec::new();
            let mut run_start = None;
            for col in 0..width {
                let dark = row.get(min_c + col).copied().unwrap_or(false);
                match (run_start, dark) {
                    (None, true) => run_start = Some(col),
                    (Some(s), false) => {
                        runs.push((s, col));
                        run_start = None;
                    }
                    _ => {}
                }
            }
            if let Some(s) = run_start {
                runs.push((s, width));
            }
            dark_runs.push(runs);
        }
        Some(Self {
            width,
            height,
            dark_runs,
        })
    }

    fn dim(&self) -> usize {
        self.width.max(self.height).max(1)
    }
}

/// Computes `(module_dev_px, total_logical_side)` snapped to an exact integer number of
/// device pixels per module so fractional DPI scaling (125%/150%/175%) never produces
/// subpixel white seams between rows or columns.
fn qr_canvas_metrics(dim: usize, max_logical: f32, scale_factor: f32) -> (f32, f32) {
    let scale = if scale_factor.is_finite() && scale_factor >= 1.0 {
        scale_factor
    } else {
        1.0
    };
    let total_modules = (dim.max(1) + QR_QUIET_MODULES * 2) as f32;
    let max_dev = (max_logical.max(64.0) * scale).floor();
    let cap_dev = (8.0 * scale).ceil();
    let mod_dev = (max_dev / total_modules).floor().clamp(1.0, cap_dev);
    let total_logical = (mod_dev * total_modules) / scale;
    (mod_dev, total_logical)
}

fn render_qr_widget(
    base_id: usize,
    max_logical_side: f32,
    scale_factor: f32,
    (label, data, matrix, hint): (String, String, Vec<String>, Option<String>),
    t: ThemeTokens,
    cx: &Context<IrisWindow>,
) -> AnyElement {
    let parsed = QrMatrix::from_lines(&matrix);
    let copy_payload = data.clone();

    let grid = parsed.map(|qr| {
        let dim = qr.dim();
        let (_, total_logical) = qr_canvas_metrics(dim, max_logical_side, scale_factor);
        let qr = std::rc::Rc::new(qr);
        let qr_paint = qr.clone();
        let qr_canvas = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                window.paint_quad(fill(bounds, rgb(0xFFFFFF)));
                let scale = window.scale_factor();
                let scale = if scale.is_finite() && scale >= 1.0 {
                    scale
                } else {
                    1.0
                };
                let avail_w = f32::from(bounds.size.width) * scale;
                let avail_h = f32::from(bounds.size.height) * scale;
                let total_modules = (qr_paint.dim() + QR_QUIET_MODULES * 2) as f32;
                let mod_dev = ((avail_w.min(avail_h) + 0.05) / total_modules)
                    .floor()
                    .max(1.0);
                let qr_dev_w = mod_dev * qr_paint.width as f32;
                let qr_dev_h = mod_dev * qr_paint.height as f32;
                let origin_dev_x =
                    (f32::from(bounds.origin.x) * scale + (avail_w - qr_dev_w) * 0.5).round();
                let origin_dev_y =
                    (f32::from(bounds.origin.y) * scale + (avail_h - qr_dev_h) * 0.5).round();

                for (ry, runs) in qr_paint.dark_runs.iter().enumerate() {
                    if runs.is_empty() {
                        continue;
                    }
                    let y0 = px((origin_dev_y + ry as f32 * mod_dev) / scale);
                    let y1 = px((origin_dev_y + (ry + 1) as f32 * mod_dev) / scale);
                    let h = y1 - y0;
                    for &(rx0, rx1) in runs {
                        let x0 = px((origin_dev_x + rx0 as f32 * mod_dev) / scale);
                        let x1 = px((origin_dev_x + rx1 as f32 * mod_dev) / scale);
                        window.paint_quad(fill(
                            Bounds::new(point(x0, y0), size(x1 - x0, h)),
                            rgb(0x000000),
                        ));
                    }
                }
            },
        )
        .w(px(total_logical))
        .h(px(total_logical))
        .flex_none();

        let sheet = div()
            .flex_none()
            .bg(rgb(0xFFFFFF))
            .border_1()
            .border_color(t.line_strong)
            .child(qr_canvas);
        hstack(0.).w_full().justify_center().py(px(4.)).child(sheet)
    });
    let copy_row = (!data.trim().is_empty()).then(|| {
        let btn = chip_button(("pl-qr-copy", base_id), "复制内容", false, t).on_click(cx.listener(
            move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_payload.clone()));
                this.show_toast("二维码内容已复制到剪贴板", cx);
            },
        ));
        let url_txt = trunc_txt(data, t.text_xs, t.text_dim)
            .flex_1()
            .min_w(px(0.))
            .font_family(FONT_MONO);
        hbetween(8.).child(url_txt).child(btn)
    });

    surface_card(8., t)
        .when(!label.is_empty(), |d| {
            d.child(txt(label, t.text_sm, t.text_muted))
        })
        .children(grid)
        .children(copy_row)
        .when_some(hint.filter(|h| !h.trim().is_empty()), |d, h| {
            d.child(txt(h, t.text_sm, t.text_dim))
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
        WidgetSurface::Sidebar { .. } => None,
    };

    let content = match widget {
        Widget::Label { text, muted, mono } => {
            txt(text, t.text_sm, if muted { t.text_dim } else { t.text })
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
            .child(txt(label, t.text_sm, t.text_dim))
            .child(
                txt(value, t.text_sm, if accent { t.accent } else { t.text }).font_family(FONT_MONO),
            )
            .into_any_element(),

        Widget::Progress { label, ratio } => {
            let pct = if ratio.is_finite() {
                ratio.clamp(0., 1.)
            } else {
                0.
            };
            vstack(4.)
                .when(!label.is_empty(), |d| {
                    d.child(txt(label, t.text_xs, t.text_dim))
                })
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
            let lbl = trunc_txt(
                label,
                t.text_sm,
                if is_modal { t.text_muted } else { t.text_dim },
            )
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
            t.control_h_md,
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
                icon_pill_button(
                    ("pl-gbtn", bi),
                    btn.label,
                    g,
                    btn.accent,
                    t.control_h_sm,
                    rest_bg,
                    t,
                )
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
                        .when_some(lbl, |d, l| d.child(txt(l, t.text_sm, t.text_muted)))
                        .child(win.render_text_field(spec, Some(window), cx))
                        .into_any_element()
                }
                WidgetSurface::Sidebar { window } => {
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
                        .h(px(f32::from(t.control_h_md).max(30.)))
                        .on_click(cx.listener(move |this, _, w, cx| btn_cb(this, w, cx)));
                    let field = div().flex_1().min_w(px(0.)).child(win.render_text_field(
                        spec,
                        Some(window),
                        cx,
                    ));
                    vstack(4.)
                        .when_some(lbl, |d, l| d.child(txt(l, t.text_xs, t.text_dim)))
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
            let lbl = trunc_txt(label, if is_modal { t.text_md } else { t.text_sm }, lbl_col)
                .flex_1()
                .min_w(px(0.));
            row_box(
                SharedString::from(format!("pl-tgl:{base_id}:{id}")),
                rest_bg,
                border,
                t,
            )
            .h(if is_modal {
                t.control_h_md
            } else {
                t.control_h_sm
            })
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
                    .h(if is_modal {
                        t.control_h_md
                    } else {
                        t.control_h_sm
                    })
                    .on_click(cx.listener(move |this, _, w, cx| cb(this, val.clone(), w, cx)))
            });
            let lbl_col = if is_modal { t.text_muted } else { t.text_dim };
            vstack(4.)
                .when_some(label.filter(|l| !l.is_empty()), |d, l| {
                    d.child(txt(
                        l,
                        if is_modal { t.text_sm } else { t.text_xs },
                        lbl_col,
                    ))
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
                .child(trunc_txt(title, t.text_sm, t_col))
                .when_some(subtitle.filter(|s| !s.is_empty()), |d, sub| {
                    d.child(trunc_txt(sub, t.text_xs, t.text_faint))
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
            ..
        } => {
            let window = match &surface {
                WidgetSurface::Sidebar { window } | WidgetSurface::Modal { window, .. } => *window,
            };
            let max_logical_side = if is_modal {
                (f32::from(window.viewport_size().width) - 108.0).clamp(160.0, 260.0)
            } else {
                (win.settings.directory_width - 64.0).clamp(140.0, 256.0)
            };
            render_qr_widget(
                base_id,
                max_logical_side,
                window.scale_factor(),
                (label, data, matrix, hint),
                t,
                cx,
            )
        }

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
            let copy_btn = icon_pill_button(
                btn_id,
                btn_lbl,
                Some(Icon::Link),
                true,
                t.control_h_md,
                t.panel,
                t,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_url.clone()));
                this.show_toast("已复制链接到剪贴板", cx);
            }));
            surface_card(6., t)
                .when(!label.is_empty(), |d| {
                    d.child(txt(label, t.text_sm, t.text_muted))
                })
                .child(trunc_txt(url, t.text_xs, t.text_dim).font_family(FONT_MONO))
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
    use super::{
        QR_QUIET_MODULES, QrMatrix, finite_value, qr_canvas_metrics, sanitize_range, step_clamped,
    };

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

    #[test]
    fn qr_matrix_trims_outer_margins_and_coalesces_dark_runs() {
        let raw = vec![
            "0000000".into(),
            "0110110".into(),
            "0100010".into(),
            "0111110".into(),
            "0000000".into(),
        ];
        let parsed = QrMatrix::from_lines(&raw).expect("non-empty qr");
        assert_eq!(parsed.width, 5);
        assert_eq!(parsed.height, 3);
        assert_eq!(parsed.dark_runs[0], vec![(0, 2), (3, 5)]);
        assert_eq!(parsed.dark_runs[1], vec![(0, 1), (4, 5)]);
        assert_eq!(parsed.dark_runs[2], vec![(0, 5)]);
        assert!(QrMatrix::from_lines(&["000".into(), "000".into()]).is_none());
    }

    #[test]
    fn qr_canvas_metrics_snaps_to_integer_device_pixels_across_dpi_scales() {
        for scale in [1.0f32, 1.25, 1.5, 1.75, 2.0] {
            for dim in [29usize, 37, 49] {
                let (mod_dev, total_logical) = qr_canvas_metrics(dim, 224.0, scale);
                assert_eq!(mod_dev.fract(), 0.0, "scale={scale} dim={dim}");
                assert!(mod_dev >= 3.0, "scale={scale} dim={dim} mod_dev={mod_dev}");
                let total_dev = (total_logical * scale).round();
                let quiet_dev = (total_dev - mod_dev * dim as f32) * 0.5;
                assert!(
                    quiet_dev >= mod_dev * QR_QUIET_MODULES as f32 - 0.01,
                    "scale={scale} dim={dim} quiet_dev={quiet_dev} mod_dev={mod_dev}"
                );
                assert!(total_logical <= 224.01, "overflowed container: {total_logical}");
            }
        }
    }
}
