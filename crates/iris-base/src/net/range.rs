//! Unified HTTP single-byte-range grammar, slice resolution, and `Content-Range` formatting
//! with checked `u64` arithmetic and no invented representation length.

use std::fmt;

/// Error returned when an HTTP `Range` header or decimal integer violates syntax or bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeError;

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid or unsatisfiable HTTP byte range")
    }
}

impl std::error::Error for RangeError {}

/// Parses a non-empty ASCII decimal integer into `u64`, rejecting signs, whitespace, and overflow.
pub fn parse_decimal_u64(value: &str) -> Result<u64, RangeError> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(RangeError);
    }
    value.parse::<u64>().map_err(|_| RangeError)
}

/// Syntactically valid single HTTP byte-range specifier (`RFC 9110 §14.1.2` subset).
///
/// Multi-range headers (`bytes=0-1,2-3`), reversed bounds (`start > end`), zero-length
/// suffix ranges (`bytes=-0`), signs, whitespace, and `u64` overflows are rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteRangeSpec {
    /// `bytes=start-end` where `start <= end`.
    Closed { start: u64, end: u64 },
    /// `bytes=start-`.
    From { start: u64 },
    /// `bytes=-len` where `len > 0`.
    Suffix { len: u64 },
}

impl ByteRangeSpec {
    /// Parses a single `bytes=...` header value without requiring a known representation size.
    pub fn parse(header: &str) -> Result<Self, RangeError> {
        let (start, end) = header
            .strip_prefix("bytes=")
            .and_then(|v| v.split_once('-'))
            .ok_or(RangeError)?;
        match (start.is_empty(), end.is_empty()) {
            (true, true) => Err(RangeError),
            (true, false) => {
                let len = parse_decimal_u64(end)?;
                if len == 0 {
                    return Err(RangeError);
                }
                Ok(Self::Suffix { len })
            }
            (false, true) => Ok(Self::From {
                start: parse_decimal_u64(start)?,
            }),
            (false, false) => {
                let start = parse_decimal_u64(start)?;
                let end = parse_decimal_u64(end)?;
                if start > end {
                    return Err(RangeError);
                }
                Ok(Self::Closed { start, end })
            }
        }
    }

    /// Resolves this byte-range specifier against an optional known total representation `size`.
    ///
    /// With no `size` and no explicit `end`, only `bytes=0-` can be served without buffering
    /// to EOF first (as a non-partial full stream); non-zero open ranges and suffix ranges
    /// on unsized streams are rejected.
    pub fn resolve(self, size: Option<u64>) -> Result<HttpRange, RangeError> {
        match self {
            Self::Suffix { len: suffix } => {
                let total = size.filter(|&n| n != 0).ok_or(RangeError)?;
                let len = suffix.min(total);
                Ok(HttpRange {
                    offset: total - len,
                    len: Some(len),
                    partial: true,
                    total: size,
                })
            }
            Self::From { start: offset } => {
                if size.is_some_and(|n| offset >= n) {
                    return Err(RangeError);
                }
                match size {
                    Some(n) => Ok(HttpRange {
                        offset,
                        len: Some(n - offset),
                        partial: true,
                        total: size,
                    }),
                    None if offset == 0 => Ok(HttpRange {
                        offset: 0,
                        len: None,
                        partial: false,
                        total: None,
                    }),
                    None => Err(RangeError),
                }
            }
            Self::Closed { start: offset, end } => {
                if size.is_some_and(|n| offset >= n) {
                    return Err(RangeError);
                }
                let end = size.map_or(end, |n| end.min(n - 1));
                let len = end
                    .checked_sub(offset)
                    .and_then(|n| n.checked_add(1))
                    .ok_or(RangeError)?;
                Ok(HttpRange {
                    offset,
                    len: Some(len),
                    partial: true,
                    total: size,
                })
            }
        }
    }
}

/// Resolved HTTP byte range ready for local slicing and `200`/`206` header generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpRange {
    pub offset: u64,
    pub len: Option<u64>,
    pub partial: bool,
    pub total: Option<u64>,
}

impl HttpRange {
    /// Parses an optional `Range` header and resolves it against an optional total `size`.
    pub fn parse(header: Option<&str>, size: Option<u64>) -> Result<Self, RangeError> {
        match header {
            None => Ok(Self {
                offset: 0,
                len: size,
                partial: false,
                total: size,
            }),
            Some(value) => ByteRangeSpec::parse(value)?.resolve(size),
        }
    }

    /// Formats the `Content-Range` response header value when `self.partial` is `true`.
    pub fn content_range(&self) -> Option<String> {
        if !self.partial {
            return None;
        }
        let end = self.offset.checked_add(self.len?.checked_sub(1)?)?;
        Some(format!(
            "bytes {}-{}/{}",
            self.offset,
            end,
            self.total.map_or_else(|| "*".into(), |n| n.to_string())
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{ByteRangeSpec, HttpRange, parse_decimal_u64};

    #[test]
    fn spec_and_resolved_ranges_preserve_proxy_and_local_slice_invariants() {
        assert_eq!(parse_decimal_u64("0"), Ok(0));
        assert!(parse_decimal_u64("+1").is_err());
        assert!(parse_decimal_u64("18446744073709551616").is_err());

        assert_eq!(
            ByteRangeSpec::parse("bytes=0-"),
            Ok(ByteRangeSpec::From { start: 0 })
        );
        assert_eq!(
            ByteRangeSpec::parse("bytes=128-255"),
            Ok(ByteRangeSpec::Closed {
                start: 128,
                end: 255
            })
        );
        assert_eq!(
            ByteRangeSpec::parse("bytes=-512"),
            Ok(ByteRangeSpec::Suffix { len: 512 })
        );
        // Transparent proxy accepts syntax-valid `0-u64::MAX`, while unsized local slice
        // rejects `u64::MAX + 1` length overflow.
        let full_u64 = ByteRangeSpec::parse("bytes=0-18446744073709551615").unwrap();
        assert!(full_u64.resolve(None).is_err());
        assert_eq!(
            full_u64
                .resolve(Some(10))
                .unwrap()
                .content_range()
                .as_deref(),
            Some("bytes 0-9/10")
        );

        assert_eq!(
            HttpRange::parse(Some("bytes=8-100"), Some(10))
                .unwrap()
                .content_range()
                .as_deref(),
            Some("bytes 8-9/10")
        );
        assert_eq!(
            HttpRange::parse(Some("bytes=-100"), Some(10))
                .unwrap()
                .offset,
            0
        );
        assert_eq!(
            HttpRange::parse(Some("bytes=3-5"), None)
                .unwrap()
                .content_range()
                .as_deref(),
            Some("bytes 3-5/*")
        );
        for bad in [
            "bytes=",
            "bytes=-",
            "bytes=-0",
            "bytes=2-1",
            "bytes=0-1,3-4",
            "bytes= 0-1",
            "items=0-1",
        ] {
            assert!(ByteRangeSpec::parse(bad).is_err(), "{bad}");
        }
        assert!(HttpRange::parse(Some("bytes=-1"), None).is_err());
        assert!(!HttpRange::parse(Some("bytes=0-"), None).unwrap().partial);
        assert!(HttpRange::parse(Some("bytes=1-"), None).is_err());
        assert_eq!(HttpRange::parse(None, Some(0)).unwrap().len, Some(0));
    }
}
