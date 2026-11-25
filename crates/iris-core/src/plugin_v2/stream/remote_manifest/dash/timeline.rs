use super::*;
pub(super) fn duration(value: &str) -> Result<u64, ErrorCode> {
    let mut input = value.strip_prefix("PT").ok_or(ErrorCode::Unsupported)?;
    let mut total = 0u64;
    let mut order = 0;
    let mut any = false;
    while !input.is_empty() {
        let end = input
            .find(|c: char| c.is_ascii_uppercase())
            .ok_or(ErrorCode::InvalidArgument)?;
        let number = &input[..end];
        let unit = input.as_bytes()[end];
        input = &input[end + 1..];
        let (rank, multiplier) = match unit {
            b'H' => (1, 3_600_000),
            b'M' => (2, 60_000),
            b'S' => (3, 1000),
            _ => return Err(ErrorCode::Unsupported),
        };
        if rank <= order {
            return Err(ErrorCode::InvalidArgument);
        }
        order = rank;
        any = true;
        let n = if unit == b'S' {
            let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
            let fraction = fraction.trim_end_matches('0');
            if fraction.len() > 3 {
                return Err(ErrorCode::Unsupported);
            }
            let millis = if fraction.is_empty() {
                0
            } else {
                decimal(fraction)? * 10u64.pow(3 - fraction.len() as u32)
            };
            decimal(whole)?
                .checked_mul(1000)
                .and_then(|n| n.checked_add(millis))
                .ok_or(ErrorCode::RangeInvalid)?
        } else {
            decimal(number)?
                .checked_mul(multiplier)
                .ok_or(ErrorCode::RangeInvalid)?
        };
        total = total.checked_add(n).ok_or(ErrorCode::RangeInvalid)?;
    }
    if !any || total > 7 * 24 * 3600 * 1000 {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(total)
}
pub(super) fn expand(
    address: Node<'_, '_>,
    duration: u64,
    timescale: u32,
    offset: u64,
    count: Option<usize>,
) -> Result<Vec<(u64, u64)>, ErrorCode> {
    let ticks = u128::from(duration) * u128::from(timescale);
    let period_end = u64::try_from(ticks.div_ceil(1000))
        .ok()
        .and_then(|n| n.checked_add(offset))
        .ok_or(ErrorCode::RangeInvalid)?;
    let Some(timeline) = one(address, "SegmentTimeline")? else {
        if offset != 0 {
            return Err(ErrorCode::Unsupported);
        }
        let delta = decimal(need(address, "duration")?)?;
        if delta == 0 {
            return Err(ErrorCode::InvalidArgument);
        }
        let count = count
            .map(|n| n as u64)
            .unwrap_or(period_end.div_ceil(delta));
        if count == 0 || count > manifest::RESOURCE_LIMIT as u64 {
            return Err(ErrorCode::QuotaExceeded);
        }
        return (0..count)
            .map(|n| {
                n.checked_mul(delta)
                    .map(|t| (t, delta))
                    .ok_or(ErrorCode::RangeInvalid)
            })
            .collect();
    };
    node(timeline, &[], &["S"])?;
    let entries: Vec<_> = timeline
        .children()
        .filter(|n| n.has_tag_name((NS, "S")))
        .collect();
    if entries.is_empty() || entries.len() > manifest::RESOURCE_LIMIT {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut result = Vec::new();
    let mut time = 0u64;
    for (index, entry) in entries.iter().enumerate() {
        node(*entry, &["t", "d", "r"], &[])?;
        if let Some(t) = entry.attribute("t") {
            time = decimal(t)?;
        }
        let delta = decimal(need(*entry, "d")?)?;
        if delta == 0 {
            return Err(ErrorCode::InvalidArgument);
        }
        let repeat = entry.attribute("r").unwrap_or("0");
        let count = if repeat == "-1" {
            let next = entries
                .get(index + 1)
                .map(|n| need(*n, "t").and_then(decimal))
                .transpose()?;
            let span = next
                .unwrap_or(period_end)
                .checked_sub(time)
                .ok_or(ErrorCode::RangeInvalid)?;
            if span == 0 || next.is_some() && span % delta != 0 {
                return Err(ErrorCode::InvalidArgument);
            }
            span.div_ceil(delta)
        } else {
            decimal(repeat)?
                .checked_add(1)
                .ok_or(ErrorCode::RangeInvalid)?
        };
        if count > manifest::RESOURCE_LIMIT as u64
            || result.len() as u64 + count > manifest::RESOURCE_LIMIT as u64
        {
            return Err(ErrorCode::QuotaExceeded);
        }
        for _ in 0..count {
            result.push((time, delta));
            time = time.checked_add(delta).ok_or(ErrorCode::RangeInvalid)?;
        }
    }
    Ok(result)
}
pub(super) fn template(
    input: &str,
    id: &str,
    bandwidth: u64,
    number: Option<u64>,
    time: Option<u64>,
) -> Result<String, ErrorCode> {
    if input.len() > 4096 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut out = String::new();
    let mut rest = input;
    while let Some(index) = rest.find('$') {
        out.push_str(&rest[..index]);
        rest = &rest[index + 1..];
        if let Some(tail) = rest.strip_prefix('$') {
            out.push('$');
            rest = tail;
            continue;
        }
        let (key, tail) = rest.split_once('$').ok_or(ErrorCode::InvalidArgument)?;
        rest = tail;
        if key == "RepresentationID" {
            out.push_str(id);
            continue;
        }
        let (name, width) = if let Some((name, format)) = key.split_once('%') {
            let width = format
                .strip_prefix('0')
                .and_then(|s| s.strip_suffix('d'))
                .ok_or(ErrorCode::Unsupported)?;
            let width = decimal(width)?;
            if !(1..=12).contains(&width) {
                return Err(ErrorCode::Unsupported);
            }
            (name, width as usize)
        } else {
            (key, 0)
        };
        let value = match name {
            "Bandwidth" => bandwidth,
            "Number" => number.ok_or(ErrorCode::Unsupported)?,
            "Time" => time.ok_or(ErrorCode::Unsupported)?,
            _ => return Err(ErrorCode::Unsupported),
        };
        use std::fmt::Write;
        write!(out, "{value:0width$}").map_err(|_| ErrorCode::Internal)?;
        if out.len() > 4096 {
            return Err(ErrorCode::QuotaExceeded);
        }
    }
    out.push_str(rest);
    if out.len() > 4096 {
        return Err(ErrorCode::QuotaExceeded);
    }
    Ok(out)
}
