//! Sustained transport measurement, deliberately separate from decoder/UI claims.
use super::*;
use ring::digest::{self, Context, SHA256};

#[test]
#[ignore = "run scripts/test-plugin-v2-transport.py; takes ten minutes"]
fn sustained_4k_scoped_transport() {
    let directory = std::env::var_os("IRIS_V2_TRANSPORT_FIXTURE").expect("4K fixture directory");
    let directory = Path::new(&directory);
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("fixture.json")).unwrap()).unwrap();
    let reference = std::fs::read(directory.join("reference.mp4")).unwrap();
    assert!(reference.len() <= 32 << 20);
    let digest = digest::digest(&SHA256, &reference);
    let peak = metadata["peak_one_second_bytes"].as_u64().unwrap();
    assert!(peak > 0 && metadata["width"] == 3840 && metadata["height"] == 2160);
    let budget = Duration::from_secs_f64(reference.len() as f64 / (peak as f64 * 1.25));
    drop(reference);
    let server = Server::start(directory);
    let ledger = Ledger::default();
    let grant = NetworkGrant::new(
        "transport-benchmark".into(),
        BTreeSet::from([Capability::NetHttp]),
        NetworkDeclaration::default(),
        &ledger,
    )
    .unwrap();
    let origin = grant
        .confirm_origin(
            &server.url,
            TransportKind::Http,
            &["127.0.0.1".parse().unwrap()],
        )
        .unwrap();
    let spec = StreamSpec {
        source_id: Some("fixture".into()),
        entry_id: Some("reference.mp4".into()),
        title: None,
        kind: Some(iris_api::SourceEntryKind::Video),
        is_live: false,
        source: StreamSource::DirectHttp {
            primary: RemoteResource {
                url: format!("{}/reference.mp4", server.url),
                headers: Vec::new(),
                origin_grant: Some(origin),
            },
            fallback_urls: Vec::new(),
            identity: None,
        },
        variants: Vec::new(),
        active_variant: None,
        audio_tracks: Vec::new(),
        subtitles: Vec::new(),
    };
    let playback = Preparation {
        network: grant.clone(),
        ledger: ledger.clone(),
        generation: Generation::default(),
        guest: None,
    }
    .prepare(&spec, &BTreeMap::new(), &HttpCancel::default())
    .unwrap();
    let url = url::Url::parse(&playback.url).unwrap();
    let authority = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    let started = Instant::now();
    let mut samples = Vec::new();
    let mut total = 0_u64;
    let mut misses = 0;
    let mut peak_host = 0;
    let mut next_progress = Duration::from_secs(60);
    while started.elapsed() < Duration::from_secs(600) {
        let start = Instant::now();
        let mut socket = TcpStream::connect(&authority).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        write!(
            socket,
            "GET {} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n",
            url.path()
        )
        .unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
            assert!(header.len() <= 16384);
        }
        assert!(header.starts_with(b"HTTP/1.1 200"));
        let mut hash = Context::new(&SHA256);
        let mut bytes = 0_u64;
        let mut buffer = [0_u8; 65536];
        loop {
            let length = socket.read(&mut buffer).unwrap();
            if length == 0 {
                break;
            }
            hash.update(&buffer[..length]);
            bytes += length as u64;
            assert!(bytes <= 32 << 20);
            peak_host = peak_host.max(ledger.used(None, Resource::HostBytes));
        }
        assert_eq!(hash.finish().as_ref(), digest.as_ref());
        let elapsed = start.elapsed();
        misses += usize::from(elapsed > budget);
        total += bytes;
        samples.push(serde_json::json!({"bytes":bytes,"fetch_ms":elapsed.as_secs_f64()*1000.,
            "bytes_per_second":bytes as f64/elapsed.as_secs_f64(),"deadline_missed":elapsed>budget}));
        if started.elapsed() >= next_progress {
            println!(
                "transport: {:.0}s, {} reads, {misses} deadline misses",
                started.elapsed().as_secs_f64(),
                samples.len()
            );
            next_progress += Duration::from_secs(60);
        }
        std::thread::sleep(
            budget
                .saturating_sub(start.elapsed())
                .min(Duration::from_secs(600).saturating_sub(started.elapsed())),
        );
    }
    let elapsed = started.elapsed().as_secs_f64();
    let cleanup = Instant::now();
    playback.relay.stop();
    while !playback.relay.is_quiescent() {
        assert!(cleanup.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(playback);
    drop(grant);
    for resource in [
        Resource::HostBytes,
        Resource::PlaybackSession,
        Resource::RelayWorker,
        Resource::Handle,
    ] {
        assert_eq!(ledger.used(None, resource), 0);
    }
    println!(
        "IRIS_V2_TRANSPORT_RECEIPT={}",
        serde_json::json!({
            "elapsed_seconds":elapsed,"total_bytes":total,"target_bytes_per_second":peak as f64*1.25,
            "paced_bytes_per_second":total as f64/elapsed,"deadline_misses":misses,"samples":samples,
            "sampled_host_ledger_bytes":peak_host,"cleanup_ms":cleanup.elapsed().as_secs_f64()*1000.,
            "debug":cfg!(debug_assertions),"scope":"DirectHttp local RAM origin + authorized relay; repeated complete 4K sample; no decoder, display, Wasm transformation or WAN; deadline misses are transport-budget misses, not player underruns"
        })
    );
    assert_eq!(
        misses, 0,
        "transport failed the frozen 1.25x peak-one-second supply budget"
    );
}
