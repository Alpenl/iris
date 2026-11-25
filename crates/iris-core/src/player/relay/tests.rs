use super::*;
use crate::library::Kind;
use crate::media_uri::MediaUri;
use crate::player::{RemoteMediaRequest, RemoteSubtitle, Session};
use crate::settings::Settings;
use std::net::TcpListener;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

#[test]
fn playlist_routes_bind_urls_extensions_and_playback_keys() {
    let addr = "127.0.0.1:9898".parse().unwrap();
    let key = Routes::new(addr).unwrap();
    let other = Routes::new(addr).unwrap();
    let base = Url::parse("https://media.example/season/list.m3u8?token=source-secret").unwrap();
    let playlist = concat!(
        "#EXTM3U\n",
        "#EXT-X-MEDIA:TYPE=SUBTITLES,NAME=\"zh, tw\",URI=\"/text/subtitles\"\n",
        "#EXT-X-KEY:METHOD=AES-128,URI=\"key?token=source-secret\"\n",
        "#EXT-X-MAP:URI=\"../init.mp4\",BYTERANGE=\"128@0\"\n",
        "#EXT-X-STREAM-INF:BANDWIDTH=80000\n",
        "https://cdn.example/variant.m3u8\n",
        "#EXTINF:2,\nsegment.ts?token=source-secret\n",
    );
    let manifest = rewrite_playlist(playlist, &base, &key).unwrap();
    assert!(!manifest.contains("source-secret"));
    assert!(!manifest.contains("media.example"));
    assert!(manifest.contains("NAME=\"zh, tw\""));
    let mut urls = Vec::new();
    for line in manifest.lines() {
        let raw = if let Some((_, value)) = line.split_once("URI=\"") {
            value.split_once('"').unwrap().0
        } else if line.starts_with("http:") {
            line
        } else {
            continue;
        };
        let local = Url::parse(raw).unwrap();
        assert!(other.open(local.path()).is_err());
        assert!(key.open(&format!("{}.mp4", local.path())).is_err());
        let (upstream, _) = key.open(local.path()).unwrap();
        if upstream.path() == "/text/subtitles" {
            assert!(local.path().ends_with(".m3u8"));
        }
        urls.push(upstream.to_string());
    }
    assert_eq!(
        urls,
        [
            "https://media.example/text/subtitles",
            "https://media.example/season/key?token=source-secret",
            "https://media.example/init.mp4",
            "https://cdn.example/variant.m3u8",
            "https://media.example/season/segment.ts?token=source-secret",
        ]
    );
    assert!(rewrite_playlist("#EXTM3U\nfile:///local/file.ts\n", &base, &key).is_err());
    assert!(valid_range("bytes=0-"));
    assert!(valid_range("bytes=128-255"));
    assert!(!valid_range("bytes=3-4,10-20"));
    assert!(is_hls(b"\xef\xbb\xbf#EXTM3U"));
    assert!(
        rewrite_playlist(
            "#EXTM3U\n#EXT-X-DEFINE:NAME=\"a\",VALUE=\"b\"\n",
            &base,
            &key
        )
        .is_err()
    );
}

#[test]
fn playlist_rejects_ambiguous_attribute_grammar() {
    let routes = Routes::new("127.0.0.1:9898".parse().unwrap()).unwrap();
    let base = Url::parse("https://media.example/playlist.m3u8").unwrap();
    for attrs in [
        "METHOD=AES-128 IV=0x1234",
        "METHOD=AES-128\tIV=0x1234",
        "METHOD= \"NONE\"",
        "NAME=\"a\\\"b\"",
        "NAME=\"a\"X=1",
        "NAME=a=b",
        "NAME=a,",
        "name=a",
    ] {
        assert!(
            rewrite_playlist(&format!("#EXTM3U\n#EXT-X-KEY:{attrs}\n"), &base, &routes).is_err(),
            "{attrs}"
        );
    }
    let scalars =
        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n";
    assert_eq!(rewrite_playlist(scalars, &base, &routes).unwrap(), scalars);
    let extension = "#EXTM3U\n#EXT-X-CUE-OUT:30\n#EXT-X-TARGETDURATION:2\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n";
    assert_eq!(
        rewrite_playlist(extension, &base, &routes).unwrap(),
        scalars
    );
}

/// Static fixture origins run entirely on loopback. Every requested resource and
/// every observed credential is recorded for assertions after libmpv opens the file.
struct Fixture {
    address: SocketAddr,
    seen: Arc<Mutex<Vec<(String, bool)>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new(handler: impl Fn(&str) -> (&'static str, String, Vec<u8>) + Send + 'static) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (stop, record) = (Arc::clone(&stopped), Arc::clone(&seen));
        let worker = thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::park_timeout(Duration::from_millis(5));
                    continue;
                };
                let Ok(raw) = read_request(&mut socket, CLIENT_TIMEOUT, MAX_REQUEST) else {
                    continue;
                };
                let req = String::from_utf8(raw).unwrap();
                let path = req.split_ascii_whitespace().nth(1).unwrap();
                let auth = req.lines().any(|line| line == "X-Media-Key: fixture");
                record.lock().unwrap().push((path.to_owned(), auth));
                let (status, headers, body) = handler(path);
                let _ = write!(
                    socket,
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(&body);
            }
        });
        Self {
            address,
            seen,
            stopped,
            worker: Some(worker),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        let worker = self.worker.take().unwrap();
        worker.thread().unpark();
        worker.join().unwrap();
    }
}

fn silent_mp3() -> Vec<u8> {
    // MPEG-1 Layer III, 128 kb/s, 44.1 kHz, zero side-info and silent spectral data.
    let mut frame = [0; 417];
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
    frame.repeat(80)
}

#[test]
fn authenticated_hls_plays_through_native_session() {
    let origin = Fixture::new(|path| {
        match path {
        "/start" => ("302 Found", "Location: /hls/master.m3u8\r\n".into(), Vec::new()),
        "/hls/master.m3u8" => ("200 OK", "Content-Type: application/vnd.apple.mpegurl\r\n".into(), b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000\ntracks/list.m3u8\n".to_vec()),
        "/hls/tracks/list.m3u8" => ("200 OK", "Content-Type: application/vnd.apple.mpegurl\r\n".into(), b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:3\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2.09,\npart.mp3\n#EXT-X-ENDLIST\n".to_vec()),
        "/hls/tracks/part.mp3" => ("200 OK", "Content-Type: audio/mpeg\r\n".into(), silent_mp3()),
        "/captions.srt" => ("200 OK", "Content-Type: text/plain\r\n".into(), b"1\n00:00:00,000 --> 00:00:01,000\nLocal fixture\n".to_vec()),
        _ => ("404 Not Found", String::new(), Vec::new()),
    }
    });
    let mut session = Session::new().unwrap();
    let mut events = session.events().unwrap();
    session.mpv.set_property_string("ao", "null").unwrap();
    let playback_url = format!("http://{}/start", origin.address);
    session
        .open_remote(
            RemoteMediaRequest {
                uri: MediaUri::HttpUrl(playback_url.clone()),
                playback_url,
                title: "Relay fixture".into(),
                headers: BTreeMap::from([("X-Media-Key".into(), "fixture".into())]),
                kind: Kind::Audio,
                is_live: false,
                photo: None,
                subtitles: vec![RemoteSubtitle::Url(format!(
                    "http://{}/captions.srt",
                    origin.address
                ))],
            },
            &Settings::default(),
            0.0,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut loaded = false;
    while Instant::now() < deadline {
        if let Ok(update) = events.try_recv() {
            match session.apply_update(&update) {
                Some(crate::player::SessionEvent::Loaded) => loaded = true,
                Some(crate::player::SessionEvent::Error(error)) => {
                    panic!("native playback failed: {error}")
                }
                _ => (),
            }
        } else {
            thread::sleep(Duration::from_millis(5));
        }
        if loaded
            && origin
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _)| p == "/captions.srt")
        {
            break;
        }
    }
    session.shutdown();
    let origin_hits = origin.seen.lock().unwrap();
    assert!(
        loaded,
        "libmpv never loaded fixture, origin requests: {origin_hits:?}"
    );
    for path in [
        "/start",
        "/hls/master.m3u8",
        "/hls/tracks/list.m3u8",
        "/hls/tracks/part.mp3",
        "/captions.srt",
    ] {
        assert!(
            origin_hits.iter().any(|(p, auth)| p == path && *auth),
            "missing authorized {path}, requests: {origin_hits:?}"
        );
    }
}

#[test]
fn playback_seek_preserves_range_and_representation_length() {
    let origin = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = Url::parse(&format!("http://{}/clip.mp4", origin.local_addr().unwrap())).unwrap();
    let upstream = thread::spawn(move || {
        let (mut socket, _) = origin.accept().unwrap();
        let request =
            String::from_utf8(read_request(&mut socket, CLIENT_TIMEOUT, MAX_REQUEST).unwrap()).unwrap();
        assert!(request.lines().any(|line| line == "Range: bytes=10-13"));
        assert!(request.lines().any(|line| line == "X-Media-Key: fixture"));
        socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 10-13/100\r\nContent-Length: 4\r\nAccept-Ranges: bytes\r\n\r\nabcd").unwrap();
    });
    let relay = PlaybackRelay::start(
        url.clone(),
        BTreeMap::from([("X-Media-Key".into(), "fixture".into())]),
    )
    .unwrap();
    let local = Url::parse(&relay.url(&url).unwrap()).unwrap();
    let authority = relay.service.routes.authority;
    let mut client = TcpStream::connect(authority).unwrap();
    client.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    write!(
        client,
        "GET {} HTTP/1.1\r\nHost: {authority}\r\nRange: bytes=10-13\r\n\r\n",
        local.path()
    )
    .unwrap();
    let mut reply = String::new();
    client.read_to_string(&mut reply).unwrap();
    upstream.join().unwrap();
    assert!(reply.starts_with("HTTP/1.1 206"));
    assert!(reply.contains("content-range: bytes 10-13/100\r\n"));
    assert!(reply.contains("content-length: 4\r\n"));
    assert!(reply.ends_with("\r\n\r\nabcd"));
}

#[test]
fn saturated_relay_admits_seek_after_a_worker_releases_its_slot() {
    let origin = Fixture::new(|_| {
        (
            "206 Partial Content",
            "Content-Range: bytes 0-3/100\r\n".into(),
            b"abcd".to_vec(),
        )
    });
    let url = Url::parse(&format!("http://{}/clip.mp4", origin.address)).unwrap();
    let relay = PlaybackRelay::start(url.clone(), BTreeMap::new()).unwrap();
    let local = Url::parse(&relay.url(&url).unwrap()).unwrap();
    let authority = relay.service.routes.authority;
    let mut occupied: Vec<_> = (0..RELAY_WORKERS)
        .map(|_| relay.service.try_reserve_worker().unwrap())
        .collect();

    let mut client = TcpStream::connect(authority).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    write!(
        client,
        "GET {} HTTP/1.1\r\nHost: {authority}\r\nRange: bytes=0-3\r\n\r\n",
        local.path()
    )
    .unwrap();
    let pending = client.peek(&mut [0]);
    assert!(
        matches!(&pending, Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)),
        "seek was answered at capacity: {pending:?}"
    );
    drop(occupied.pop());

    client.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    let mut reply = String::new();
    client.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 206"), "{reply}");
    assert!(reply.ends_with("\r\n\r\nabcd"));
}

#[test]
fn relay_terminates_clean_chunks_only() {
    for (body, clean) in [
        (&b"4\r\ndata\r\n0\r\n\r\n"[..], true),
        (b"4\r\ndata", false),
    ] {
        let origin = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let url = Url::parse(&format!("http://{}/audio", origin.local_addr().unwrap())).unwrap();
        let upstream = thread::spawn(move || {
            let (mut socket, _) = origin.accept().unwrap();
            read_request(&mut socket, CLIENT_TIMEOUT, MAX_REQUEST).unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap();
            socket.write_all(body).unwrap();
        });
        let relay = PlaybackRelay::start(url.clone(), BTreeMap::new()).unwrap();
        let local = Url::parse(&relay.url(&url).unwrap()).unwrap();
        let authority = relay.service.routes.authority;
        let mut client = TcpStream::connect(authority).unwrap();
        client.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
        write!(
            client,
            "GET {} HTTP/1.1\r\nHost: {authority}\r\n\r\n",
            local.path()
        )
        .unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        upstream.join().unwrap();
        assert!(reply.contains("Transfer-Encoding: chunked\r\n"));
        assert!(reply.contains("\r\n\r\n4\r\ndata\r\n"));
        assert_eq!(reply.ends_with("0\r\n\r\n"), clean);
    }
}

#[test]
fn abandoned_slow_seeks_release_slots_and_keep_listener_alive() {
    let origin = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    origin.set_nonblocking(true).unwrap();
    let url = Url::parse(&format!("http://{}/clip.mp4", origin.local_addr().unwrap())).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = Arc::clone(&stop);
    let upstream = thread::spawn(move || {
        while !stop_bg.load(Ordering::Relaxed) {
            let Ok((mut socket, _)) = origin.accept() else {
                thread::sleep(Duration::from_millis(2));
                continue;
            };
            thread::spawn(move || {
                let Ok(raw) = read_request(&mut socket, Duration::from_secs(2), MAX_REQUEST) else {
                    return;
                };
                let req = String::from_utf8(raw).unwrap();
                if req.contains("Range: bytes=99-102") {
                    let _ = socket.write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 99-102/200\r\nContent-Length: 4\r\n\r\ndone",
                    );
                } else {
                    // Simulate a stalled TTFB until the relay cancels the abandoned upstream socket.
                    let _ = socket.set_read_timeout(Some(Duration::from_secs(15)));
                    let mut buf = [0u8; 1];
                    let _ = socket.read(&mut buf);
                }
            });
        }
    });

    let relay = PlaybackRelay::start(url.clone(), BTreeMap::new()).unwrap();
    let local = Url::parse(&relay.url(&url).unwrap()).unwrap();
    let authority = relay.service.routes.authority;

    // Exceed RELAY_WORKERS (8) with abandoned slow Range requests.
    for i in 0..(RELAY_WORKERS + 6) {
        let mut client = TcpStream::connect(authority).unwrap();
        write!(
            client,
            "GET {} HTTP/1.1\r\nHost: {authority}\r\nRange: bytes={i}-{}\r\n\r\n",
            local.path(),
            i + 3
        )
        .unwrap();
        drop(client);
    }

    let mut final_seek = TcpStream::connect(authority).unwrap();
    final_seek.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    write!(
        final_seek,
        "GET {} HTTP/1.1\r\nHost: {authority}\r\nRange: bytes=99-102\r\n\r\n",
        local.path()
    )
    .unwrap();
    let mut reply = String::new();
    final_seek.read_to_string(&mut reply).unwrap();
    stop.store(true, Ordering::Relaxed);
    upstream.join().unwrap();
    assert!(
        reply.starts_with("HTTP/1.1 206"),
        "unexpected reply: {reply}"
    );
    assert!(reply.ends_with("\r\n\r\ndone"));
}

#[test]
fn playlist_probe_falls_back_when_range_rejected_and_subtitle_keeps_explicit_auth() {
    let origin = Fixture::new(|path| match path {
        "/live" => (
            "200 OK",
            "Content-Type: application/vnd.apple.mpegurl\r\n".into(),
            b"#EXTM3U\n#EXT-X-VERSION:3\n".to_vec(),
        ),
        _ => ("404 Not Found", String::new(), Vec::new()),
    });
    // Non-Range server that rejects Range headers with 416.
    let no_range = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let no_range_addr = no_range.local_addr().unwrap();
    let upstream = thread::spawn(move || {
        for _ in 0..2 {
            let (mut sock, _) = no_range.accept().unwrap();
            let req = String::from_utf8(read_request(&mut sock, CLIENT_TIMEOUT, MAX_REQUEST).unwrap()).unwrap();
            if req.lines().any(|l| l.starts_with("Range:")) {
                sock.write_all(
                    b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            } else {
                sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 25\r\nConnection: close\r\n\r\n#EXTM3U\n#EXT-X-VERSION:3\n",
                )
                .unwrap();
            }
        }
    });
    let main = Url::parse(&format!("http://{no_range_addr}/stream")).unwrap();
    let relay = PlaybackRelay::start(main.clone(), BTreeMap::new()).unwrap();
    let media_route = relay.url(&main).unwrap();
    let probed = relay.service.playlist_url(&media_route).unwrap();
    upstream.join().unwrap();
    assert!(
        probed.is_some(),
        "expected fallback GET probe to detect #EXTM3U"
    );

    // Same-origin subtitle with explicit userinfo must send its own Basic credentials.
    let auth_srv = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let auth_addr = auth_srv.local_addr().unwrap();
    let sub_upstream = thread::spawn(move || {
        let (mut sock, _) = auth_srv.accept().unwrap();
        let req = String::from_utf8(read_request(&mut sock, CLIENT_TIMEOUT, MAX_REQUEST).unwrap()).unwrap();
        assert!(
            req.lines()
                .any(|l| l.eq_ignore_ascii_case("authorization: basic c3VidXNlcjpzdWJwYXNz")),
            "expected subuser:subpass Basic header, got: {req}"
        );
        sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi")
            .unwrap();
    });
    let main_with_auth = Url::parse(&format!("http://main:secret@{auth_addr}/video.mp4")).unwrap();
    let sub_with_auth = Url::parse(&format!("http://subuser:subpass@{auth_addr}/sub.srt")).unwrap();
    let auth_relay = PlaybackRelay::start(main_with_auth, BTreeMap::new()).unwrap();
    let sub_local = Url::parse(&auth_relay.subtitle_url(&sub_with_auth).unwrap()).unwrap();
    let authority = auth_relay.service.routes.authority;
    let mut client = TcpStream::connect(authority).unwrap();
    client.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    write!(
        client,
        "GET {} HTTP/1.1\r\nHost: {authority}\r\n\r\n",
        sub_local.path()
    )
    .unwrap();
    let mut sub_reply = String::new();
    client.read_to_string(&mut sub_reply).unwrap();
    sub_upstream.join().unwrap();
    assert!(sub_reply.ends_with("\r\n\r\nhi"));
    drop(origin);
}
