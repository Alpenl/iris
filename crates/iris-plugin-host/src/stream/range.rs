//! Plugin-stream byte-range adapter over `iris_base::net::range`, mapping range
//! errors to protocol-2 [`ErrorCode::RangeInvalid`].
use iris_api::v2::ErrorCode;
use iris_base::net::range::{HttpRange, parse_decimal_u64};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Range {
    pub offset: u64,
    pub len: Option<u64>,
    pub partial: bool,
    pub total: Option<u64>,
}

impl Range {
    pub(crate) fn parse(header: Option<&str>, size: Option<u64>) -> Result<Self, ErrorCode> {
        let parsed = HttpRange::parse(header, size).map_err(|_| ErrorCode::RangeInvalid)?;
        Ok(Self {
            offset: parsed.offset,
            len: parsed.len,
            partial: parsed.partial,
            total: parsed.total,
        })
    }

    pub(crate) fn content_range(&self) -> Option<String> {
        HttpRange {
            offset: self.offset,
            len: self.len,
            partial: self.partial,
            total: self.total,
        }
        .content_range()
    }
}

pub(crate) fn decimal(value: &str) -> Result<u64, ErrorCode> {
    parse_decimal_u64(value).map_err(|_| ErrorCode::RangeInvalid)
}

#[cfg(test)]
mod tests {
    use super::Range;

    #[test]
    fn boundaries_suffix_and_unknown_lengths() {
        assert_eq!(
            Range::parse(Some("bytes=8-100"), Some(10))
                .unwrap()
                .content_range()
                .as_deref(),
            Some("bytes 8-9/10")
        );
        assert_eq!(
            Range::parse(Some("bytes=-100"), Some(10)).unwrap().offset,
            0
        );
        assert_eq!(
            Range::parse(Some("bytes=3-5"), None)
                .unwrap()
                .content_range()
                .as_deref(),
            Some("bytes 3-5/*")
        );
        for value in [
            "bytes=10-",
            "bytes=2-1",
            "bytes=0-1,3-4",
            "bytes=-0",
            "bytes=+1-",
            "bytes=18446744073709551616-",
            "bytes=0-18446744073709551615",
        ] {
            assert!(
                Range::parse(
                    Some(value),
                    if value.ends_with("615") {
                        None
                    } else {
                        Some(10)
                    }
                )
                .is_err(),
                "{value}"
            );
        }
        assert!(Range::parse(Some("bytes=-1"), None).is_err());
        assert!(!Range::parse(Some("bytes=0-"), None).unwrap().partial);
        assert!(Range::parse(Some("bytes=1-"), None).is_err());
        assert_eq!(Range::parse(None, Some(0)).unwrap().len, Some(0));
    }
}
