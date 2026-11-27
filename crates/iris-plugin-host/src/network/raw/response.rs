use std::io::{Cursor, Read};
use std::time::Instant;

use iris_api::v2::ErrorCode;

use super::{Connection, Headers, RawRequest, Response, io_error, unique_header};
use iris_net::http_client::{ChunkedReader, HttpBody, parse_decimal_u64};

pub(super) fn receive(
    mut connection: Connection,
    request: &RawRequest,
    deadline: Instant,
) -> Result<Response, ErrorCode> {
    connection
        .transport
        .set_body_deadline(Some(deadline))
        .map_err(|e| io_error(e, &connection.grant, &connection.cancel))?;
    let (status, headers, prefix) = head(&mut connection, deadline)?;
    if let Some(jar) = &request.cookies {
        jar.receive(&request.url, &headers)?;
    }
    let content_len = unique_header(&headers, "content-length")?
        .map(|v| parse_decimal_u64(v).ok_or(ErrorCode::InvalidArgument))
        .transpose()?;
    let transfer = unique_header(&headers, "transfer-encoding")?;
    if transfer.is_some() && content_len.is_some() {
        return Err(ErrorCode::InvalidArgument);
    }
    if transfer.is_some_and(|v| !v.eq_ignore_ascii_case("chunked")) {
        return Err(ErrorCode::Unsupported);
    }
    let chained = Cursor::new(prefix).chain(connection.transport);
    let body = if request.method == "HEAD" || matches!(status, 204 | 304) {
        HttpBody::Empty
    } else if transfer.is_some() {
        HttpBody::Chunked(Box::new(ChunkedReader::new(chained)))
    } else if let Some(len) = content_len {
        HttpBody::Fixed(chained, len)
    } else {
        HttpBody::CloseDelimited(chained)
    };
    Ok(Response {
        status,
        url: request.url.clone(),
        headers,
        body,
        grant: connection.grant,
        cancel: connection.cancel,
        _registration: connection.registration,
    })
}

fn head(conn: &mut Connection, deadline: Instant) -> Result<(u16, Headers, Vec<u8>), ErrorCode> {
    let mut raw = Vec::with_capacity(4096);
    let mut start = 0;
    let mut scan = 0;
    let mut informational = 0;
    loop {
        conn.grant.check_live()?;
        if conn.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(ErrorCode::Timeout);
        }
        let Some(at) = raw[scan..].windows(4).position(|w| w == b"\r\n\r\n") else {
            if raw.len() >= 65536 {
                return Err(ErrorCode::QuotaExceeded);
            }
            scan = raw.len().saturating_sub(3).max(start);
            let mut buf = [0; 4096];
            let cap = buf.len().min(65536 - raw.len());
            let n = conn
                .transport
                .read(&mut buf[..cap])
                .map_err(|e| io_error(e, &conn.grant, &conn.cancel))?;
            if n == 0 {
                return Err(ErrorCode::IoError);
            }
            raw.extend_from_slice(&buf[..n]);
            continue;
        };
        let end = scan + at + 4;
        let mut slots = [httparse::EMPTY_HEADER; 128];
        let mut response = httparse::Response::new(&mut slots);
        if !response
            .parse(&raw[start..end])
            .map_err(|_| ErrorCode::InvalidArgument)?
            .is_complete()
        {
            return Err(ErrorCode::InvalidArgument);
        }
        let status = response.code.ok_or(ErrorCode::InvalidArgument)?;
        if (100..200).contains(&status) {
            informational += 1;
            if status == 101 || informational > 8 {
                return Err(ErrorCode::Unsupported);
            }
            start = end;
            scan = end;
            continue;
        }
        if !(200..=599).contains(&status) {
            return Err(ErrorCode::InvalidArgument);
        }
        let mut headers = Vec::with_capacity(response.headers.len());
        for header in response.headers {
            if header
                .value
                .iter()
                .any(|&b| b == 0x7f || (b < 0x20 && b != b'\t'))
            {
                return Err(ErrorCode::InvalidArgument);
            }
            let value: String = header.value.iter().map(|&b| char::from(b)).collect();
            headers.push((header.name.to_ascii_lowercase(), value));
        }
        return Ok((status, headers, raw[end..].to_vec()));
    }
}
