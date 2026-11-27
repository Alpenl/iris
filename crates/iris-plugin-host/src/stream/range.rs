//! One HTTP byte range, with checked arithmetic and no invented total length.
use iris_api::v2::ErrorCode;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub offset: u64,
    pub len: Option<u64>,
    pub partial: bool,
    pub total: Option<u64>,
}
impl Range {
    pub fn parse(header: Option<&str>, size: Option<u64>) -> Result<Self, ErrorCode> {
        let Some(value) = header else {
            return Ok(Self {
                offset: 0,
                len: size,
                partial: false,
                total: size,
            });
        };
        let (start, end) = value
            .strip_prefix("bytes=")
            .and_then(|v| v.split_once('-'))
            .ok_or(ErrorCode::RangeInvalid)?;
        if start.is_empty() {
            let suffix = decimal(end)?;
            let total = size.filter(|n| *n != 0).ok_or(ErrorCode::RangeInvalid)?;
            if suffix == 0 {
                return Err(ErrorCode::RangeInvalid);
            }
            let len = suffix.min(total);
            Ok(Self {
                offset: total - len,
                len: Some(len),
                partial: true,
                total: size,
            })
        } else {
            let offset = decimal(start)?;
            if size.is_some_and(|n| offset >= n) {
                return Err(ErrorCode::RangeInvalid);
            }
            let len = if end.is_empty() {
                size.map(|n| n - offset)
            } else {
                let end = decimal(end)?;
                let end = size.map_or(end, |n| end.min(n - 1));
                Some(
                    end.checked_sub(offset)
                        .and_then(|n| n.checked_add(1))
                        .ok_or(ErrorCode::RangeInvalid)?,
                )
            };
            // With no size and no explicit end, a valid Content-Range cannot be
            // produced before reading to EOF. Reject instead of buffering.
            if len.is_none() {
                if offset == 0 {
                    return Ok(Self {
                        offset,
                        len: None,
                        partial: false,
                        total: None,
                    });
                }
                return Err(ErrorCode::RangeInvalid);
            }
            Ok(Self {
                offset,
                len,
                partial: true,
                total: size,
            })
        }
    }
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
pub(crate) fn decimal(value: &str) -> Result<u64, ErrorCode> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ErrorCode::RangeInvalid);
    }
    value.parse().map_err(|_| ErrorCode::RangeInvalid)
}
#[cfg(test)]
mod tests {
    use super::*;
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
