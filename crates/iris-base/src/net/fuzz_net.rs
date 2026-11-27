//! Bounded, zero-network parser entry points exercised by `tests/plugin_v2_fuzz.rs`.

use std::io::{Cursor, Read};

pub fn exercise(input: &[u8]) {
    let _ = crate::net::http_client::parse_response_head(input);
    let mut decoded = Vec::new();
    let _ = crate::net::http_client::ChunkedReader::new(Cursor::new(input))
        .take(64 * 1024)
        .read_to_end(&mut decoded);
    let _ = crate::net::loopback_http::parse_http_request(input, "127.0.0.1:18080");
    let _ = crate::net::http_client::doh::parse_doh_response_bytes(input);
    let _ = crate::net::http_client::doh::parse_dns_a_response(input);
    if let Ok(text) = std::str::from_utf8(input) {
        let _ = crate::net::http_client::doh::encode_dns_a_query(text);
        let _ = crate::net::http_client::doh::normalize_doh_resolver(text);
        let _ = crate::net::http_client::validate_stream_url(text);
    }
}
