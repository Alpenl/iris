use super::{finite, identifier, text};
use iris_api::v2::ErrorCode;
use iris_api::{OverlayNode, UiThemeSpec, Widget};
use std::collections::BTreeSet;

pub(crate) fn theme(value: &UiThemeSpec) -> Result<(), ErrorCode> {
    if let Some(name) = &value.name {
        text(name, 128)?;
    }
    for color in [
        &value.window_bg,
        &value.panel,
        &value.hover,
        &value.selected,
        &value.line,
        &value.line_strong,
        &value.text,
        &value.text_muted,
        &value.text_dim,
        &value.text_faint,
        &value.accent,
        &value.overlay,
        &value.scrim,
    ]
    .into_iter()
    .flatten()
    {
        if !iris_api::is_valid_hex_color(color) {
            return Err(ErrorCode::InvalidArgument);
        }
    }
    for metric in [
        value.titlebar_height,
        value.dock_height,
        value.row_height,
        value.corner_radius,
    ]
    .into_iter()
    .flatten()
    {
        finite(metric.into(), 0., 512.)?;
    }
    Ok(())
}
pub(crate) fn widgets(values: &[Widget]) -> Result<(), ErrorCode> {
    if values.len() > 256 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut ids = BTreeSet::new();
    let mut count = values.len();
    for widget in values {
        // The transport cap also bounds total strings/QR data. Per-field bounds
        // keep a single widget from monopolizing layout and text shaping.
        let fields = serde_json::to_value(widget).map_err(|_| ErrorCode::InvalidArgument)?;
        for (_, field) in fields.as_object().ok_or(ErrorCode::Internal)? {
            if let Some(value) = field.as_str() {
                text(value, 4096)?;
            }
        }
        let id = match widget {
            Widget::Slider { id, .. }
            | Widget::Input { id, .. }
            | Widget::Toggle { id, .. }
            | Widget::Select { id, .. }
            | Widget::ListItem { id, .. } => Some(id),
            _ => None,
        };
        if let Some(id) = id {
            identifier(id)?;
            if !ids.insert(id) {
                return Err(ErrorCode::InvalidArgument);
            }
        }
        match widget {
            Widget::Progress { ratio, .. } => finite((*ratio).into(), 0., 1.)?,
            Widget::Slider {
                value,
                min,
                max,
                step,
                action_id,
                ..
            } => {
                finite(*min, -1e9, 1e9)?;
                finite(*max, *min, 1e9)?;
                finite(*value, *min, *max)?;
                finite(*step, f64::EPSILON, 1e9)?;
                identifier(action_id)?;
                if min == max {
                    return Err(ErrorCode::InvalidArgument);
                }
            }
            Widget::Button {
                action_id, icon, ..
            } => {
                identifier(action_id)?;
                if let Some(icon) = icon {
                    identifier(icon)?;
                }
            }
            Widget::ListItem { action_id, .. } => identifier(action_id)?,
            Widget::Input { action_id, .. }
            | Widget::Toggle { action_id, .. }
            | Widget::Select { action_id, .. } => {
                if let Some(id) = action_id {
                    identifier(id)?;
                }
            }
            Widget::ButtonGroup { buttons } => {
                count += buttons.len();
                for button in buttons {
                    text(&button.label, 256)?;
                    identifier(&button.action_id)?;
                    if let Some(icon) = &button.icon {
                        identifier(icon)?;
                    }
                }
            }
            Widget::PixelGrid { rows, .. } => {
                if rows.len() > 177 {
                    return Err(ErrorCode::InvalidArgument);
                }
                let expected_width = rows.first().map(String::len).unwrap_or(0);
                if expected_width > 177
                    || rows.iter().any(|row| {
                        row.len() != expected_width || !row.bytes().all(|b| b"01# ".contains(&b))
                    })
                {
                    return Err(ErrorCode::InvalidArgument);
                }
            }
            Widget::Link { url, .. } => {
                crate::url_resolver::normalize_url(url, crate::url_resolver::TransportKind::Http)?;
            }
            _ => {}
        }
        if let Widget::Select {
            selected, options, ..
        } = widget
        {
            count += options.len();
            let mut keys = BTreeSet::new();
            for option in options {
                text(&option.label, 256)?;
                text(&option.value, 1024)?;
                if !keys.insert(&option.value) {
                    return Err(ErrorCode::InvalidArgument);
                }
            }
            if !keys.contains(selected) {
                return Err(ErrorCode::InvalidArgument);
            }
        }
        if count > 256 {
            return Err(ErrorCode::QuotaExceeded);
        }
    }
    Ok(())
}
fn numbers(values: &[f32], min: f64, max: f64) -> Result<(), ErrorCode> {
    for value in values {
        finite((*value).into(), min, max)?;
    }
    Ok(())
}
pub(crate) fn overlay(nodes: &[OverlayNode], interactive: bool) -> Result<(), ErrorCode> {
    overlay_with_limit(nodes, interactive, 512)
}
pub(crate) fn overlay_with_limit(
    nodes: &[OverlayNode],
    interactive: bool,
    limit: usize,
) -> Result<(), ErrorCode> {
    if nodes.len() > 64 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut elements = 0;
    for node in nodes {
        use OverlayNode::*;
        let cost = match node {
            Box {
                x,
                y,
                w,
                h,
                border_width,
                radius,
                opacity,
                ..
            } => {
                numbers(&[*x, *y, *w, *h, *opacity], 0., 1.)?;
                numbers(&[*border_width, *radius], 0., 64.)?;
                1
            }
            Line {
                x1,
                y1,
                x2,
                y2,
                width,
                opacity,
                ..
            } => {
                numbers(&[*x1, *y1, *x2, *y2, *opacity], 0., 1.)?;
                numbers(&[*width], 0., 16.)?;
                1
            }
            Grid {
                cols,
                rows,
                opacity,
                ..
            } => {
                if !(2..=16).contains(cols) || !(2..=16).contains(rows) {
                    return Err(ErrorCode::InvalidArgument);
                }
                numbers(&[*opacity], 0., 1.)?;
                usize::from(cols + rows)
            }
            Crosshair {
                x,
                y,
                size,
                opacity,
                ..
            } => {
                numbers(&[*x, *y, *size, *opacity], 0., 1.)?;
                2
            }
            Text {
                x,
                y,
                text: value,
                size,
                opacity,
                ..
            } => {
                numbers(&[*x, *y, *opacity], 0., 1.)?;
                numbers(&[*size], 9., 64.)?;
                text(value, 4096)?;
                4
            }
            Bars {
                x,
                y,
                w,
                h,
                values,
                opacity,
                ..
            } => {
                numbers(&[*x, *y, *w, *h, *opacity], 0., 1.)?;
                numbers(values, 0., 1.)?;
                if values.len() > 128 {
                    return Err(ErrorCode::QuotaExceeded);
                }
                values.len() + 1
            }
            Button {
                id,
                x,
                y,
                label,
                icon,
                action_id,
                ..
            } => {
                if !interactive {
                    return Err(ErrorCode::Denied);
                }
                identifier(id)?;
                identifier(action_id)?;
                text(label, 256)?;
                numbers(&[*x, *y], 0., 1.)?;
                if let Some(icon) = icon {
                    identifier(icon)?;
                }
                4
            }
        };
        elements += cost;
        if elements > limit {
            return Err(ErrorCode::QuotaExceeded);
        }
    }
    Ok(())
}
