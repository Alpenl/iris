use iris_api::v2::ErrorCode;

/// Timestamped whisper.cpp stdout is converted to inert WebVTT text by the
/// host. Do not use `-of -`: v1.8.2 opens CON on Windows instead of the pipe.
pub(super) fn transcription(bytes: &[u8]) -> Result<Vec<u8>, ErrorCode> {
    let text = std::str::from_utf8(bytes).map_err(|_| ErrorCode::InvalidArgument)?;
    let mut output = String::from("WEBVTT\n\n");
    let mut count = 0;
    let mut previous = 0i64;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        if line.len() > 8192 {
            return Err(ErrorCode::QuotaExceeded);
        }
        let line = line.trim();
        let (range, value) = line
            .strip_prefix('[')
            .and_then(|s| s.split_once(']'))
            .ok_or(ErrorCode::InvalidArgument)?;
        let (start, end) = range
            .split_once(" --> ")
            .ok_or(ErrorCode::InvalidArgument)?;
        let a = timestamp(start)?;
        let b = timestamp(end)?;
        if a < previous || b < a {
            return Err(ErrorCode::InvalidArgument);
        }
        previous = a;
        count += 1;
        if count > 10000 {
            return Err(ErrorCode::QuotaExceeded);
        }
        let value = value.trim();
        if value.chars().any(|c| c.is_control() && c != '\t') {
            return Err(ErrorCode::InvalidArgument);
        }
        output.push_str(start);
        output.push_str(" --> ");
        output.push_str(end);
        output.push('\n');
        output.push_str(
            &value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;"),
        );
        output.push_str("\n\n");
        if output.len() > 10 << 20 {
            return Err(ErrorCode::QuotaExceeded);
        }
    }
    Ok(output.into_bytes())
}
fn timestamp(value: &str) -> Result<i64, ErrorCode> {
    let pieces = value.split(':').collect::<Vec<_>>();
    if pieces.len() != 3 || pieces[0].len() > 3 || pieces[1].len() != 2 {
        return Err(ErrorCode::InvalidArgument);
    }
    let hours: i64 = pieces[0].parse().map_err(|_| ErrorCode::InvalidArgument)?;
    let minutes: i64 = pieces[1].parse().map_err(|_| ErrorCode::InvalidArgument)?;
    let seconds = decimal_ms(pieces[2])?;
    if !(0..=168).contains(&hours) || !(0..60).contains(&minutes) || !(0..60000).contains(&seconds)
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(hours * 3600000 + minutes * 60000 + seconds)
}
/// ffprobe packet timestamp parser, bounded independently from the output sink.
#[derive(Default)]
pub(super) struct Extent {
    line: Vec<u8>,
    pub first: Option<i64>,
    pub end: Option<i64>,
    total: usize,
    unknown: std::collections::BTreeMap<u32, i64>,
    known: std::collections::BTreeMap<u32, i64>,
}
impl Extent {
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ErrorCode> {
        self.total = self
            .total
            .checked_add(bytes.len())
            .ok_or(ErrorCode::QuotaExceeded)?;
        if self.total > 8 << 20 {
            return Err(ErrorCode::QuotaExceeded);
        }
        for &b in bytes {
            if b == b'\n' {
                self.consume()?;
                self.line.clear();
            } else {
                if self.line.len() == 4096 {
                    return Err(ErrorCode::QuotaExceeded);
                }
                self.line.push(b);
            }
        }
        Ok(())
    }
    fn consume(&mut self) -> Result<(), ErrorCode> {
        let line = std::str::from_utf8(&self.line)
            .map_err(|_| ErrorCode::InvalidArgument)?
            .trim();
        if !line.starts_with("packet|") {
            return Ok(());
        }
        let mut pts = None;
        let mut duration = None;
        let mut discard = false;
        let mut stream = 0u32;
        for part in line.split('|').skip(1) {
            if let Some(value) = part.strip_prefix("pts_time=") {
                pts = Some(decimal_ms(value)?);
            }
            if let Some(value) = part.strip_prefix("duration_time=")
                && value != "N/A"
            {
                duration = Some(decimal_ms(value)?);
            }
            if let Some(value) = part.strip_prefix("stream_index=") {
                stream = value.parse().map_err(|_| ErrorCode::InvalidArgument)?;
                if stream > 63 {
                    return Err(ErrorCode::Unsupported);
                }
            }
            if let Some(flags) = part.strip_prefix("flags=") {
                discard = flags.contains('D');
            }
        }
        if discard {
            return Ok(());
        }
        let pts = pts.ok_or(ErrorCode::Unsupported)?;
        self.first = Some(self.first.map_or(pts, |n| n.min(pts)));
        if let Some(duration) = duration.filter(|n| *n > 0) {
            let end = pts.checked_add(duration).ok_or(ErrorCode::RangeInvalid)?;
            self.end = Some(self.end.map_or(end, |n| n.max(end)));
            self.known
                .entry(stream)
                .and_modify(|n| *n = (*n).max(pts))
                .or_insert(pts);
        } else {
            self.unknown
                .entry(stream)
                .and_modify(|n| *n = (*n).max(pts))
                .or_insert(pts);
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<(i64, i64), ErrorCode> {
        if !self.line.is_empty() {
            self.consume()?;
        }
        // Some demuxers cannot give the first packet duration until the next
        // packet. A later timestamp in that stream bounds this interior packet;
        // an unknown final duration cannot produce a measured end and is refused.
        if self
            .unknown
            .iter()
            .any(|(stream, pts)| self.known.get(stream).is_none_or(|later| later <= pts))
        {
            return Err(ErrorCode::Unsupported);
        }
        Ok((
            self.first.ok_or(ErrorCode::Unsupported)?,
            self.end.ok_or(ErrorCode::Unsupported)?,
        ))
    }
}
// Decimal, not binary floating point; precision below a millisecond is rounded
// toward zero, and reported as millisecond packet extents, not frame boundaries.
fn decimal_ms(value: &str) -> Result<i64, ErrorCode> {
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |s| (true, s));
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || whole.len() > 9
        || fraction.len() > 9
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut millis: i64 = whole
        .parse::<i64>()
        .map_err(|_| ErrorCode::InvalidArgument)?
        * 1000;
    for (digit, scale) in fraction.bytes().take(3).zip([100, 10, 1]) {
        millis += i64::from(digit - b'0') * scale;
    }
    Ok(if negative { -millis } else { millis })
}
