//! Unified, zero-network, bounded HLS (`RFC 8216`) lexical, line, attribute-list, and
//! scalar value syntax parser shared by `iris-core` and `iris-plugin-host`.
//!
//! Callers select explicit [`HlsLineOptions`] and [`AttributeOptions`] presets so direct
//! HTTP playlist rewriting (`iris-core`) and structured `VirtualHls` conversion
//! (`iris-plugin-host`) share one grammar implementation while preserving their source
//! compatibility and quota policies.

use std::collections::BTreeMap;
use std::fmt;

use crate::net::range::parse_decimal_u64;

/// Categorized HLS lexical/syntactic failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HlsError {
    InvalidSyntax(&'static str),
    NumberInvalid(&'static str),
    QuotaExceeded(&'static str),
    Unsupported(&'static str),
}

impl HlsError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::InvalidSyntax(msg)
            | Self::NumberInvalid(msg)
            | Self::QuotaExceeded(msg)
            | Self::Unsupported(msg) => msg,
        }
    }
}

impl fmt::Display for HlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for HlsError {}

/// Returns `true` when `prefix` starts with `#EXTM3U` (after an optional UTF-8 BOM).
pub fn has_hls_magic(prefix: &[u8]) -> bool {
    prefix
        .strip_prefix(b"\xef\xbb\xbf")
        .unwrap_or(prefix)
        .starts_with(b"#EXTM3U")
}

/// Line-level lexical options controlling BOM stripping, whitespace trimming, and bounds.
#[derive(Clone, Copy, Debug)]
pub struct HlsLineOptions {
    pub max_input_bytes: Option<usize>,
    pub max_line_bytes: Option<usize>,
    pub trim_ascii_whitespace: bool,
    pub reject_unicode_controls: bool,
    pub reject_variable_substitution: bool,
    pub strip_multiple_bom: bool,
}

impl HlsLineOptions {
    /// Policy for `iris-core` direct HTTP playlist rewriting (trims surrounding ASCII
    /// whitespace, rejects `{$` variable indirection, checks rewritten line length downstream).
    pub const CORE_REWRITE: Self = Self {
        max_input_bytes: None,
        max_line_bytes: None,
        trim_ascii_whitespace: true,
        reject_unicode_controls: false,
        reject_variable_substitution: true,
        strip_multiple_bom: true,
    };

    /// Policy for `iris-plugin-host` remote HLS conversion (enforces input byte cap,
    /// `<= 4096` input line cap, exact trimmed lines, and zero Unicode control characters).
    pub const fn strict_remote(max_input_bytes: usize) -> Self {
        Self {
            max_input_bytes: Some(max_input_bytes),
            max_line_bytes: Some(4096),
            trim_ascii_whitespace: false,
            reject_unicode_controls: true,
            reject_variable_substitution: false,
            strip_multiple_bom: false,
        }
    }
}

/// Classified logical line after `#EXTM3U` validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HlsLine<'a> {
    Empty,
    /// `#EXT...` tag line (`name` includes leading `#`; `value` is the text after `:`, if any).
    Tag {
        raw: &'a str,
        name: &'a str,
        value: Option<&'a str>,
    },
    /// Non-`#EXT` comment line beginning with `#`.
    Comment(&'a str),
    /// Non-empty, non-`#` URI reference line.
    Uri(&'a str),
}

/// Validated HLS document body iterator over lines following the `#EXTM3U` header.
pub struct ParsedHlsLines<'a> {
    body: &'a str,
    lines: std::str::Lines<'a>,
    opts: HlsLineOptions,
}

impl<'a> ParsedHlsLines<'a> {
    /// Checks whether any raw line in the document begins with `tag_colon` (e.g. `"#EXT-X-MAP:"`).
    pub fn any_line_starts_with(&self, prefix: &str) -> bool {
        self.body.lines().any(|line| line.starts_with(prefix))
    }
}

impl<'a> Iterator for ParsedHlsLines<'a> {
    type Item = Result<HlsLine<'a>, HlsError>;

    fn next(&mut self) -> Option<Self::Item> {
        let raw = self.lines.next()?;
        Some(classify_line(raw, self.opts))
    }
}

/// Validates document-level invariants and the `#EXTM3U` header, returning a lazy line parser.
pub fn parse_lines(input: &str, opts: HlsLineOptions) -> Result<ParsedHlsLines<'_>, HlsError> {
    if let Some(limit) = opts.max_input_bytes
        && input.len() > limit
    {
        return Err(HlsError::QuotaExceeded("HLS manifest exceeds byte limit"));
    }
    let body = if opts.strip_multiple_bom {
        input.trim_start_matches('\u{feff}')
    } else {
        input.strip_prefix('\u{feff}').unwrap_or(input)
    };
    if body.contains('\0') || (opts.reject_variable_substitution && body.contains("{$")) {
        return Err(HlsError::InvalidSyntax("unsupported HLS playlist syntax"));
    }
    let mut lines = body.lines();
    if lines.next() != Some("#EXTM3U") {
        return Err(HlsError::InvalidSyntax("invalid HLS playlist"));
    }
    Ok(ParsedHlsLines { body, lines, opts })
}

fn classify_line<'a>(raw: &'a str, opts: HlsLineOptions) -> Result<HlsLine<'a>, HlsError> {
    if let Some(limit) = opts.max_line_bytes
        && raw.len() > limit
    {
        return Err(HlsError::InvalidSyntax("HLS line exceeds byte limit"));
    }
    let line = if opts.trim_ascii_whitespace {
        if !raw.bytes().all(|b| b >= 0x20 || b == b'\t') {
            return Err(HlsError::InvalidSyntax("invalid HLS line"));
        }
        raw.trim_ascii()
    } else {
        if raw.trim_ascii() != raw {
            return Err(HlsError::InvalidSyntax("invalid HLS line whitespace"));
        }
        raw
    };
    if opts.reject_unicode_controls && line.chars().any(char::is_control) {
        return Err(HlsError::InvalidSyntax("invalid HLS control character"));
    }
    if line.is_empty() {
        return Ok(HlsLine::Empty);
    }
    if line.starts_with("#EXT") {
        let (name, value) = line
            .split_once(':')
            .map_or((line, None), |(name, value)| (name, Some(value)));
        Ok(HlsLine::Tag {
            raw: line,
            name,
            value,
        })
    } else if line.starts_with('#') {
        Ok(HlsLine::Comment(line))
    } else {
        Ok(HlsLine::Uri(line))
    }
}

/// Allowed byte set for unquoted RFC 8216 attribute values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnquotedCharset {
    /// Any non-whitespace byte except `'='`, `'"'`, and `'\\'` (`iris-core` rewriter).
    NonWhitespaceNoQuotes,
    /// Strict ASCII alphanumeric plus `'.'`, `'-'`, `'_'` (`iris-plugin-host` converter).
    StrictToken,
}

/// Policy knobs for RFC 8216 §4.2 attribute-list parsing.
#[derive(Clone, Copy, Debug)]
pub struct AttributeOptions {
    pub allow_empty_quoted: bool,
    pub max_quoted_value_bytes: Option<usize>,
    pub reject_unicode_controls_in_quoted: bool,
    pub unquoted_charset: UnquotedCharset,
    pub max_attributes: Option<usize>,
}

impl AttributeOptions {
    /// Policy for `iris-core` streaming attribute rewriting.
    pub const CORE_REWRITE: Self = Self {
        allow_empty_quoted: true,
        max_quoted_value_bytes: None,
        reject_unicode_controls_in_quoted: false,
        unquoted_charset: UnquotedCharset::NonWhitespaceNoQuotes,
        max_attributes: None,
    };

    /// Policy for `iris-plugin-host` structured attribute maps (`<= 16` unique attributes).
    pub const STRICT_REMOTE: Self = Self {
        allow_empty_quoted: false,
        max_quoted_value_bytes: Some(4096),
        reject_unicode_controls_in_quoted: true,
        unquoted_charset: UnquotedCharset::StrictToken,
        max_attributes: Some(16),
    };
}

/// One parsed attribute from an HLS `#EXT-X-*` attribute list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HlsAttribute<'a> {
    pub name: &'a str,
    pub value: &'a str,
    pub quoted: bool,
}

/// Parses an RFC 8216 §4.2 comma-separated attribute list in encounter order.
pub fn parse_attributes(
    mut input: &str,
    opts: AttributeOptions,
) -> Result<Vec<HlsAttribute<'_>>, HlsError> {
    let mut out = Vec::new();
    loop {
        let (name, rest) = input
            .split_once('=')
            .ok_or(HlsError::InvalidSyntax("missing HLS attribute value"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(HlsError::InvalidSyntax("invalid HLS attribute name"));
        }
        let (value, tail, quoted) = if let Some(rest) = rest.strip_prefix('"') {
            let (v, tail) = rest
                .split_once('"')
                .ok_or(HlsError::InvalidSyntax("unterminated HLS attribute"))?;
            if (!opts.allow_empty_quoted && v.is_empty())
                || opts.max_quoted_value_bytes.is_some_and(|cap| v.len() > cap)
                || v.contains(['\\', '\t'])
                || (opts.reject_unicode_controls_in_quoted && v.chars().any(char::is_control))
            {
                return Err(HlsError::InvalidSyntax("invalid quoted HLS attribute"));
            }
            (v, tail, true)
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let v = &rest[..end];
            let valid_chars = match opts.unquoted_charset {
                UnquotedCharset::NonWhitespaceNoQuotes => v
                    .bytes()
                    .all(|b| !b.is_ascii_whitespace() && !matches!(b, b'=' | b'"' | b'\\')),
                UnquotedCharset::StrictToken => v
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)),
            };
            if v.is_empty() || !valid_chars {
                return Err(HlsError::InvalidSyntax("invalid unquoted HLS attribute"));
            }
            (v, &rest[end..], false)
        };
        if opts.max_attributes.is_some_and(|cap| out.len() >= cap) {
            return Err(HlsError::InvalidSyntax("too many HLS attributes"));
        }
        out.push(HlsAttribute {
            name,
            value,
            quoted,
        });
        if tail.is_empty() {
            return Ok(out);
        }
        input = tail
            .strip_prefix(',')
            .filter(|s| !s.is_empty())
            .ok_or(HlsError::InvalidSyntax("invalid HLS attribute separator"))?;
    }
}

/// Parses an RFC 8216 §4.2 attribute list into a duplicate-rejecting map `(value, quoted)`.
pub fn parse_attribute_map(
    input: &str,
    opts: AttributeOptions,
) -> Result<BTreeMap<String, (String, bool)>, HlsError> {
    let list = parse_attributes(input, opts)?;
    let mut map = BTreeMap::new();
    for attr in list {
        if map
            .insert(attr.name.to_owned(), (attr.value.to_owned(), attr.quoted))
            .is_some()
        {
            return Err(HlsError::InvalidSyntax("duplicate HLS attribute"));
        }
    }
    Ok(map)
}

/// Parses `#EXT-X-VERSION:<n>`, requiring `1..=7`.
pub fn parse_version(value: &str) -> Result<u64, HlsError> {
    let version =
        parse_decimal_u64(value).map_err(|_| HlsError::NumberInvalid("invalid HLS version"))?;
    if !(1..=7).contains(&version) {
        return Err(HlsError::Unsupported("unsupported HLS version"));
    }
    Ok(version)
}

/// Parses `#EXTINF:<duration>,[<title>]` into `(duration_ms, title)` (`1..=86_400_000` ms, `<= 3` fractional digits).
pub fn parse_extinf(value: &str, max_title_bytes: Option<usize>) -> Result<(u64, &str), HlsError> {
    let (seconds, title) = value
        .split_once(',')
        .ok_or(HlsError::InvalidSyntax("missing EXTINF comma"))?;
    if max_title_bytes.is_some_and(|cap| title.len() > cap) {
        return Err(HlsError::QuotaExceeded("EXTINF title exceeds byte limit"));
    }
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err(HlsError::InvalidSyntax("invalid EXTINF duration"));
    }
    let fraction = fraction.trim_end_matches('0');
    if fraction.len() > 3 {
        return Err(HlsError::Unsupported(
            "submillisecond EXTINF precision unsupported",
        ));
    }
    let whole =
        parse_decimal_u64(whole).map_err(|_| HlsError::NumberInvalid("invalid EXTINF seconds"))?;
    let fraction_ms = if fraction.is_empty() {
        0
    } else {
        parse_decimal_u64(fraction)
            .map_err(|_| HlsError::NumberInvalid("invalid EXTINF fraction"))?
            * 10u64.pow(3 - fraction.len() as u32)
    };
    let ms = whole
        .checked_mul(1000)
        .and_then(|n| n.checked_add(fraction_ms))
        .ok_or(HlsError::NumberInvalid("EXTINF duration overflow"))?;
    if ms == 0 || ms > 86_400_000 {
        return Err(HlsError::InvalidSyntax("EXTINF duration out of bounds"));
    }
    Ok((ms, title))
}

/// Parses `#EXT-X-BYTERANGE` or `BYTERANGE` `len[@offset]` (`len > 0`).
pub fn parse_byterange(value: &str) -> Result<(u64, Option<u64>), HlsError> {
    let (len, offset) = value
        .split_once('@')
        .map_or((value, None), |(len, offset)| (len, Some(offset)));
    let len = parse_decimal_u64(len)
        .map_err(|_| HlsError::NumberInvalid("invalid HLS byte range length"))?;
    if len == 0 {
        return Err(HlsError::NumberInvalid("zero HLS byte range length"));
    }
    let offset = offset
        .map(|raw| {
            parse_decimal_u64(raw)
                .map_err(|_| HlsError::NumberInvalid("invalid HLS byte range offset"))
        })
        .transpose()?;
    Ok((len, offset))
}

/// Validates an unquoted 128-bit hexadecimal HLS `IV=0x...` value and returns the 32 hex digits.
pub fn parse_hex_iv_128(value: &str, quoted: bool) -> Result<&str, HlsError> {
    let hex = value
        .strip_prefix("0x")
        .ok_or(HlsError::InvalidSyntax("missing HLS IV 0x prefix"))?;
    if quoted || hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(HlsError::InvalidSyntax("invalid HLS IV hex value"));
    }
    Ok(hex)
}

/// Parses an HLS `RESOLUTION=<width>x<height>` attribute value.
pub fn parse_resolution(value: &str) -> Result<(u32, u32), HlsError> {
    let (w, h) = value
        .split_once('x')
        .ok_or(HlsError::InvalidSyntax("invalid HLS resolution"))?;
    let w = parse_decimal_u64(w)
        .map_err(|_| HlsError::NumberInvalid("invalid HLS width"))
        .and_then(|n| {
            u32::try_from(n).map_err(|_| HlsError::InvalidSyntax("HLS width overflow"))
        })?;
    let h = parse_decimal_u64(h)
        .map_err(|_| HlsError::NumberInvalid("invalid HLS height"))
        .and_then(|n| {
            u32::try_from(n).map_err(|_| HlsError::InvalidSyntax("HLS height overflow"))
        })?;
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::{
        AttributeOptions, HlsError, HlsLine, HlsLineOptions, has_hls_magic, parse_attribute_map,
        parse_attributes, parse_byterange, parse_extinf, parse_hex_iv_128, parse_lines,
        parse_resolution, parse_version,
    };

    #[test]
    fn line_and_attribute_options_preserve_core_and_remote_policies() {
        assert!(has_hls_magic(b"\xef\xbb\xbf#EXTM3U\n"));
        assert!(!has_hls_magic(b"#EXTINF:1,\n"));

        let padded = "#EXTM3U\n  #EXTINF:2.500,clip  \n\tseg0.ts  \n";
        let core_lines: Vec<_> = parse_lines(padded, HlsLineOptions::CORE_REWRITE)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            core_lines,
            vec![
                HlsLine::Tag {
                    raw: "#EXTINF:2.500,clip",
                    name: "#EXTINF",
                    value: Some("2.500,clip"),
                },
                HlsLine::Uri("seg0.ts"),
            ]
        );
        let mut strict_lines = parse_lines(padded, HlsLineOptions::strict_remote(4096)).unwrap();
        assert!(matches!(
            strict_lines.next(),
            Some(Err(HlsError::InvalidSyntax(_)))
        ));

        // Core allows empty quoted non-URI attributes and unquoted `:` / `-`, while strict remote rejects both.
        assert!(parse_attributes("ID=\"ad-1\",CLASS=\"\"", AttributeOptions::CORE_REWRITE).is_ok());
        assert!(
            parse_attribute_map("ID=\"ad-1\",CLASS=\"\"", AttributeOptions::STRICT_REMOTE).is_err()
        );
        assert!(parse_attributes("CODECS=avc1:640028", AttributeOptions::CORE_REWRITE).is_ok());
        assert!(
            parse_attribute_map("CODECS=avc1:640028", AttributeOptions::STRICT_REMOTE).is_err()
        );

        assert_eq!(parse_version("7"), Ok(7));
        assert!(matches!(parse_version("8"), Err(HlsError::Unsupported(_))));
        assert_eq!(
            parse_extinf("1.000000,title", Some(256)),
            Ok((1000, "title"))
        );
        assert!(matches!(
            parse_extinf("0.999999,", Some(256)),
            Err(HlsError::Unsupported(_))
        ));
        assert_eq!(parse_byterange("512@0"), Ok((512, Some(0))));
        assert!(matches!(
            parse_byterange("0@0"),
            Err(HlsError::NumberInvalid(_))
        ));
        assert_eq!(
            parse_hex_iv_128("0x00000000000000000000000000000001", false),
            Ok("00000000000000000000000000000001")
        );
        assert_eq!(parse_resolution("3840x2160"), Ok((3840, 2160)));
    }
}
