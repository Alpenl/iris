//! Synthetic in-memory loopback throughput baseline for `iris-core`'s `PlaybackRelay`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use url::Url;

use super::{CLIENT_TIMEOUT, MAX_REQUEST, PlaybackRelay};
use crate::loopback_http::read_request;

const PAYLOAD_BYTES: usize = 64 << 20;
const ROUNDS: usize = 5;

fn synthetic_payload(len: usize) -> (Vec<u8>, u64) {
    let mut data = vec![0u8; len];
    let header = b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2";
    data[..header.len()].copy_from_slice(header);
    for (i, byte) in data.iter_mut().enumerate().skip(header.len()) {
        *byte = ((i.wrapping_mul(31) ^ (i >> 11) ^ (i >> 19)) & 0xff) as u8;
    }
    let checksum = rolling_checksum(&data);
    (data, checksum)
}

fn rolling_checksum(bytes: &[u8]) -> u64 {
    bytes
        .chunks(4096)
        .fold(0xcbf2_9ce4_8422_2325u64, |acc, chunk| {
            let head = u64::from(chunk[0]);
            let tail = u64::from(*chunk.last().unwrap_or(&0));
            acc.wrapping_mul(0x0100_0000_01b3) ^ head ^ (tail << 8) ^ (chunk.len() as u64)
        })
}

fn drain_http_response(socket: &mut TcpStream, expected_len: usize) -> (usize, u64) {
    let mut head = Vec::with_capacity(256);
    let mut buf = [0u8; 64 << 10];
    let body_prefix = loop {
        let n = socket.read(&mut buf).expect("read relay response header");
        assert!(n > 0, "unexpected EOF before HTTP header terminator");
        head.extend_from_slice(&buf[..n]);
        if let Some(pos) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            let start = pos + 4;
            let status_line = std::str::from_utf8(&head[..pos]).expect("utf-8 response headers");
            assert!(
                status_line.starts_with("HTTP/1.1 200"),
                "unexpected status: {status_line}"
            );
            break head[start..].to_vec();
        }
        assert!(head.len() <= 16 << 10, "oversized relay response header");
    };

    let mut body = Vec::with_capacity(expected_len);
    body.extend_from_slice(&body_prefix);
    loop {
        let n = socket.read(&mut buf).expect("read relay response body");
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    (body.len(), rolling_checksum(&body))
}

#[test]
#[ignore = "synthetic loopback throughput baseline; run manually with --ignored --nocapture"]
fn loopback_relay_synthetic_throughput_baseline() {
    let (payload, expected_checksum) = synthetic_payload(PAYLOAD_BYTES);
    let shared = Arc::new(payload);
    let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let origin_addr = origin.local_addr().unwrap();
    let upstream_payload = Arc::clone(&shared);

    let upstream = thread::spawn(move || {
        let response_head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            upstream_payload.len()
        );
        for _ in 0..ROUNDS {
            let (mut sock, _) = origin.accept().unwrap();
            let req =
                String::from_utf8(read_request(&mut sock, CLIENT_TIMEOUT, MAX_REQUEST).unwrap())
                    .unwrap();
            assert!(req.lines().any(|line| line == "X-Bench-Auth: r01-core"));
            sock.write_all(response_head.as_bytes()).unwrap();
            for chunk in upstream_payload.chunks(256 << 10) {
                sock.write_all(chunk).unwrap();
            }
        }
    });

    let source_url = Url::parse(&format!("http://{origin_addr}/synthetic.mp4")).unwrap();
    let relay = PlaybackRelay::start(
        source_url.clone(),
        BTreeMap::from([("X-Bench-Auth".into(), "r01-core".into())]),
    )
    .unwrap();
    let sealed = Url::parse(&relay.url(&source_url).unwrap()).unwrap();
    let authority = relay.service.routes.authority;

    let mut elapsed_ms = Vec::with_capacity(ROUNDS);
    let mut mib_per_sec = Vec::with_capacity(ROUNDS);
    let payload_mib = (PAYLOAD_BYTES as f64) / ((1usize << 20) as f64);

    for round in 1..=ROUNDS {
        let start = Instant::now();
        let mut client = TcpStream::connect(authority).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        write!(
            client,
            "GET {} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n",
            sealed.path()
        )
        .unwrap();
        let (received, checksum) = drain_http_response(&mut client, PAYLOAD_BYTES);
        let secs = start.elapsed().as_secs_f64();
        assert_eq!(received, PAYLOAD_BYTES);
        assert_eq!(checksum, expected_checksum);
        let ms = secs * 1000.0;
        let rate = payload_mib / secs;
        println!(
            "iris-core relay round {round}/{ROUNDS}: payload_mib={payload_mib:.1} elapsed_ms={ms:.2} throughput_mib_s={rate:.2}"
        );
        elapsed_ms.push(ms);
        mib_per_sec.push(rate);
    }

    upstream.join().unwrap();
    drop(relay);

    elapsed_ms.sort_by(f64::total_cmp);
    mib_per_sec.sort_by(f64::total_cmp);
    let median_ms = elapsed_ms[ROUNDS / 2];
    let median_mib_s = mib_per_sec[ROUNDS / 2];
    let iqr_mib_s = mib_per_sec[3] - mib_per_sec[1];
    let min_mib_s = mib_per_sec[0];
    let max_mib_s = mib_per_sec[ROUNDS - 1];
    let spread_pct = ((max_mib_s - min_mib_s) / median_mib_s) * 100.0;
    println!(
        "iris-core relay baseline summary: rounds={ROUNDS} payload_mib={payload_mib:.1} median_ms={median_ms:.2} median_mib_s={median_mib_s:.2} iqr_mib_s={iqr_mib_s:.2} min_mib_s={min_mib_s:.2} max_mib_s={max_mib_s:.2} spread_pct={spread_pct:.2}%"
    );
}
