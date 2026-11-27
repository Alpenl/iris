//! Bounded, zero-network parser entry points exercised by `tests/plugin_v2_fuzz.rs`.

use std::io::{Cursor, Read};

use crate::net::hls::{
    AttributeOptions, HlsLineOptions, has_hls_magic, parse_attribute_map, parse_attributes,
    parse_byterange, parse_extinf, parse_hex_iv_128, parse_lines, parse_resolution, parse_version,
};
use crate::net::range::{ByteRangeSpec, HttpRange};

pub fn exercise(input: &[u8]) {
    let _ = has_hls_magic(input);
    let _ = crate::net::http_client::parse_response_head(input);
    let mut decoded = Vec::new();
    let _ = crate::net::http_client::ChunkedReader::new(Cursor::new(input))
        .take(64 * 1024)
        .read_to_end(&mut decoded);
    let _ = crate::net::loopback_http::parse_http_request(input, "127.0.0.1:18080");
    let _ = crate::net::http_client::doh::parse_doh_response_bytes(input);
    let _ = crate::net::http_client::doh::parse_dns_a_response(input);
    if let Ok(text) = std::str::from_utf8(input) {
        let trimmed = text.trim();
        let _ = ByteRangeSpec::parse(trimmed);
        for size in [None, Some(0), Some(1024), Some(u64::MAX)] {
            if let Ok(range) = HttpRange::parse(Some(trimmed), size) {
                if let Some(len) = range.len {
                    assert!(len > 0);
                    assert!(range.offset.checked_add(len - 1).is_some());
                    if let Some(size) = size {
                        assert!(range.offset + len - 1 < size);
                    }
                }
                let _ = range.content_range();
            }
        }
        if let Ok(lines) = parse_lines(text, HlsLineOptions::CORE_REWRITE) {
            for line in lines {
                let _ = line;
            }
        }
        if let Ok(lines) = parse_lines(text, HlsLineOptions::strict_remote(4 << 20)) {
            for line in lines {
                let _ = line;
            }
        }
        let _ = parse_attributes(trimmed, AttributeOptions::CORE_REWRITE);
        let _ = parse_attribute_map(trimmed, AttributeOptions::STRICT_REMOTE);
        let _ = parse_extinf(trimmed, Some(256));
        let _ = parse_byterange(trimmed);
        let _ = parse_hex_iv_128(trimmed, false);
        let _ = parse_resolution(trimmed);
        let _ = parse_version(trimmed);
        let _ = crate::net::http_client::doh::encode_dns_a_query(text);
        let _ = crate::net::http_client::doh::normalize_doh_resolver(text);
        let _ = crate::net::http_client::validate_stream_url(text);
    }
}
