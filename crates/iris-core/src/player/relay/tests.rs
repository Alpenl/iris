use super::*;
use crate::library::Kind;
use crate::media_uri::MediaUri;
use crate::player::{RemoteMediaRequest, RemoteSubtitle, Session};
use crate::settings::Settings;
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
        "#EXT-X-MEDIA:TYPE=SUBTITLES,NAME=\"zh, tw\",URI=\"/text/subtitles.m3u8\"\n",
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
        urls.push(key.open(local.path()).unwrap().to_string());
    }
    assert_eq!(
        urls,
        [
            "https://media.example/text/subtitles.m3u8",
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
                let Ok(raw) = read_request(&mut socket) else {
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
    let cdn = Fixture::new(|_| {
        (
            "200 OK",
            "Content-Type: audio/mpeg\r\n".into(),
            silent_mp3(),
        )
    });
    let cdn_addr = cdn.address;
    let origin = Fixture::new(move |path| {
        match path {
        "/start" => ("302 Found", "Location: /hls/master.m3u8\r\n".into(), Vec::new()),
        "/hls/master.m3u8" => ("200 OK", "Content-Type: application/vnd.apple.mpegurl\r\n".into(), b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000\ntracks/list.m3u8\n".to_vec()),
        "/hls/tracks/list.m3u8" => ("200 OK", "Content-Type: application/vnd.apple.mpegurl\r\n".into(), b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:3\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2.09,\npart.mp3\n#EXT-X-ENDLIST\n".to_vec()),
        "/hls/tracks/part.mp3" => ("302 Found", format!("Location: http://{cdn_addr}/part.mp3\r\n"), Vec::new()),
        "/captions.srt" => ("200 OK", "Content-Type: text/plain\r\n".into(), b"1\n00:00:00,000 --> 00:00:01,000\nLocal fixture\n".to_vec()),
        _ => ("404 Not Found", String::new(), Vec::new()),
    }
    });
    let mut session = Session::new().unwrap();
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
        match session.mpv.wait_event(0.05) {
            Some(iris_mpv::Event::FileLoaded) => loaded = true,
            Some(iris_mpv::Event::EndFile { error, .. }) if error < 0 => {
                panic!("native playback failed: {error}")
            }
            _ => (),
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
    let cdn_hits = cdn.seen.lock().unwrap();
    assert!(cdn_hits.iter().any(|(p, _)| p == "/part.mp3"));
    assert!(cdn_hits.iter().all(|(_, auth)| !auth));
}

#[test]
fn playback_seek_preserves_range_and_representation_length() {
    let origin = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = Url::parse(&format!("http://{}/clip.mp4", origin.local_addr().unwrap())).unwrap();
    let upstream = thread::spawn(move || {
        let (mut socket, _) = origin.accept().unwrap();
        let request = String::from_utf8(read_request(&mut socket).unwrap()).unwrap();
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
