//! Seeded mutation regression for host-facing parsers. This is bounded mutation
//! fuzzing, not coverage-guided fuzzing or a substitute for sanitizer campaigns.
use iris_api::v2::{
    HostCommand, MESSAGE_LIMIT, Manifest,
    stream::{ManifestFormat, TrackKind},
    task::Task,
};
use iris_core::bounded_json::decode_exact;
use iris_plugin_host::{
    package::{Package, Provenance},
    stream::{manifest, prepare, remote_manifest},
    validation,
};
use std::{path::PathBuf, time::Instant};

fn exercise(input: &[u8]) {
    if let Ok(text) = std::str::from_utf8(input) {
        let _ = iris_plugin_host::network::normalize_url(
            text,
            iris_plugin_host::network::TransportKind::Http,
        );
        let _ = iris_plugin_host::network::parse_consent_origin_url(text);
        let _ = iris_plugin_host::package::validate_path(text);
        for size in [None, Some(0), Some(1024), Some(u64::MAX)] {
            if let Ok(range) =
                iris_plugin_host::stream::range::Range::parse(Some(text.trim()), size)
            {
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
        let _ = remote_manifest::parse_hls_attributes(text);
        let mut bound = 0usize;
        let _ = remote_manifest::parse_hls_media(text, TrackKind::Video, |_, _, _| {
            bound += 1;
            Ok(format!("seg_{bound}"))
        });
    }
    let _ = prepare::subtitle_format(input);
    iris_core::vfs::fuzz_net::exercise(input);
    if let Ok(command) = decode_exact::<HostCommand>(input, MESSAGE_LIMIT) {
        let _ = validation::command(&command);
    }
    let _ = decode_exact::<Task>(input, MESSAGE_LIMIT);
    if let Ok(manifest) = decode_exact::<Manifest>(input, MESSAGE_LIMIT) {
        let _ = manifest.validate();
    }
    let _ = Package::from_zip(input, Provenance::User);
    if let Ok(value) = manifest::compile(ManifestFormat::VirtualHls, input, &|id, _| {
        Ok(format!("http://127.0.0.1:1234/{id}"))
    }) {
        for document in value.documents {
            assert!(document.bytes.len() <= manifest::OUTPUT_LIMIT);
            std::str::from_utf8(&document.bytes).unwrap();
        }
    }
}

#[test]
fn seeded_plugin_parser_mutations() {
    let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/plugin-v2");
    // Valid seeds must actually reach the renderers and parsers; malformed-input-only
    // mutation campaigns would miss most conversion logic.
    manifest::compile(
        ManifestFormat::VirtualHls,
        &std::fs::read(corpus.join("virtual-hls.json")).unwrap(),
        &|id, _| Ok(format!("http://127.0.0.1:1234/{id}")),
    )
    .expect("valid renderer seed");
    Package::from_zip(
        &std::fs::read(corpus.join("theme.irisplugin")).unwrap(),
        Provenance::User,
    )
    .expect("valid package seed");
    let hls_media_seed = concat!(
        "#EXTM3U\n",
        "#EXT-X-VERSION:7\n",
        "#EXT-X-TARGETDURATION:4\n",
        "#EXT-X-MEDIA-SEQUENCE:1\n",
        "#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"512@0\"\n",
        "#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x00000000000000000000000000000001\n",
        "#EXTINF:3.500,\n",
        "seg0.m4s\n",
        "#EXT-X-ENDLIST\n"
    );
    remote_manifest::parse_hls_media(hls_media_seed, TrackKind::Video, |_, _, _| {
        Ok("bound_res".into())
    })
    .expect("valid hls media seed");
    let vtt_seed = b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhello\n";
    let srt_seed = b"1\n00:00:00,000 --> 00:00:01,500\nhello\n";
    let ass_seed = b"[Script Info]\nTitle: test\n[Events]\nDialogue: 0,0:00:00.00,0:00:01.00,Default,,0,0,0,,hi\n";
    assert_eq!(prepare::subtitle_format(vtt_seed), Ok("vtt"));
    assert_eq!(prepare::subtitle_format(srt_seed), Ok("srt"));
    assert_eq!(prepare::subtitle_format(ass_seed), Ok("ass"));
    let mut files: Vec<_> = std::fs::read_dir(corpus)
        .unwrap()
        .map(|p| p.unwrap().path())
        .collect();
    files.sort();
    let mut seeds: Vec<Vec<u8>> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let http_206_seed = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/10\r\nContent-Length: 4\r\nContent-Type: video/mp4\r\n\r\nabcd";
    let chunked_seed = b"4;ext=val\r\nWiki\r\n5\r\npedia\r\n0\r\nX-Trailer: ok\r\n\r\n";
    let loopback_req_seed =
        b"GET /m/1/abc.mp4 HTTP/1.1\r\nHost: 127.0.0.1:18080\r\nRange: bytes=0-1023\r\n\r\n";
    let mut doh_resp_seed = Vec::new();
    let dns_wire = [
        0u8, 0, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0, 7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3,
        b'c', b'o', b'm', 0, 0, 1, 0, 1, 0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 216,
        34,
    ];
    doh_resp_seed.extend_from_slice(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: {}\r\n\r\n",
            dns_wire.len()
        )
        .as_bytes(),
    );
    doh_resp_seed.extend_from_slice(&dns_wire);
    seeds.push(hls_media_seed.as_bytes().to_vec());
    seeds.push(vtt_seed.to_vec());
    seeds.push(srt_seed.to_vec());
    seeds.push(ass_seed.to_vec());
    seeds.push(http_206_seed.to_vec());
    seeds.push(chunked_seed.to_vec());
    seeds.push(loopback_req_seed.to_vec());
    seeds.push(doh_resp_seed);
    seeds.push(dns_wire.to_vec());
    for seed in &seeds {
        exercise(seed);
    }
    let iterations = std::env::var("IRIS_FUZZ_ITERATIONS")
        .ok()
        .map(|v| v.parse::<u64>().unwrap())
        .unwrap_or(2000);
    let initial = std::env::var("IRIS_FUZZ_SEED")
        .ok()
        .map(|v| v.parse::<u64>().unwrap())
        .unwrap_or(0x495249535632);
    assert!(initial != 0 && iterations <= 20_000_000);
    let mut random = initial;
    let start = Instant::now();
    let mut next = || {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        random as usize
    };
    for index in 0..iterations {
        let mut input = seeds[next() % seeds.len()].clone();
        for _ in 0..(1 + next() % 8) {
            let pos = next() % (input.len() + 1);
            match next() % 5 {
                0 if !input.is_empty() => {
                    let n = pos.min(input.len() - 1);
                    input[n] ^= (next() & 255) as u8;
                }
                1 => input.truncate(pos),
                2 if input.len() < 8192 => input.insert(pos, (next() & 255) as u8),
                3 if input.len() < 8192 => {
                    let piece = seeds[next() % seeds.len()].as_slice();
                    input.splice(pos..pos, piece.iter().copied().take(512));
                }
                _ => {}
            }
        }
        if std::panic::catch_unwind(|| exercise(&input)).is_err() {
            let path = std::env::temp_dir().join(format!("iris-v2-fuzz-{initial}-{index}.bin"));
            std::fs::write(&path, &input).unwrap();
            panic!(
                "parser mutation failed; seed={initial}, iteration={index}, input={}",
                path.display()
            );
        }
    }
    println!(
        "mutation receipt: seed={initial} iterations={iterations} elapsed_ms={}",
        start.elapsed().as_millis()
    );
}
