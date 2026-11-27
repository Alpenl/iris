use super::*;
use iris_api::v2::Id;
use iris_plugin_host::{BrowseTarget, PluginStatus};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct Server {
    origin: String,
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<String>>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (quit, requests) = (stop.clone(), seen.clone());
        let join = std::thread::spawn(move || {
            while !quit.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &requests),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("fixture listener: {e}"),
                }
            }
        });
        Self {
            origin,
            stop,
            seen,
            join: Some(join),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let joined = self.join.take().unwrap().join();
        if !std::thread::panicking() {
            joined.unwrap();
        }
    }
}

fn serve(mut stream: TcpStream, seen: &Mutex<Vec<String>>) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut scratch = [0; 4096];
    let end = loop {
        let len = stream.read(&mut scratch).unwrap();
        if len == 0 {
            return;
        }
        request.extend_from_slice(&scratch[..len]);
        assert!(request.len() < 64 * 1024);
        if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let header = String::from_utf8(request[..end].to_vec()).unwrap();
    let lower = header.to_ascii_lowercase();
    let length = lower
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while request.len() < end + length {
        let n = stream.read(&mut scratch).unwrap();
        if n == 0 {
            return;
        }
        request.extend_from_slice(&scratch[..n]);
    }
    seen.lock()
        .unwrap()
        .push(header.lines().next().unwrap().into());
    assert!(
        lower.contains(
            "authorization: basic dXNlcjpwYXNzd29yZA=="
                .to_ascii_lowercase()
                .as_str()
        ),
        "host must forward the explicitly authorized credentials"
    );
    let (status, mime, body): (&str, &str, &[u8]) = if header.starts_with("PROPFIND /dav/") {
        (
            "207 Multi-Status",
            "application/xml",
            br#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/dav/movie.mp4</d:href><d:prop><d:getcontentlength>21</d:getcontentlength></d:prop></d:response><d:response><d:href>/dav/movie.en.srt</d:href><d:prop/></d:response></d:multistatus>"#,
        )
    } else if header.contains("/movie.en.srt ") {
        (
            "200 OK",
            "application/x-subrip",
            b"1\n00:00:00,000 --> 00:00:01,000\nfixture\n",
        )
    } else {
        ("200 OK", "video/mp4", b"fixture-video-content")
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    if !header.starts_with("HEAD ") {
        let _ = stream.write_all(body);
    }
}

fn check_errors(notices: &[Notice]) {
    for notice in notices {
        if let Notice::Error { code, message, .. } = notice {
            panic!("official WebDAV: {code:?} {message:?}");
        }
    }
}

fn target() -> BrowseTarget {
    BrowseTarget {
        source: "fixture-nas".into(),
        folder: None,
        query: None,
        filters: BTreeMap::new(),
    }
}

fn await_page(session: &mut WindowSession, request: Id) -> iris_api::v2::source::SourcePage {
    let mut result = None;
    wait(|| {
        let notices = session.poll().notices;
        check_errors(&notices);
        for notice in notices {
            if let Notice::Page {
                request: r, page, ..
            } = notice
            {
                assert_eq!(r, request);
                result = Some(page.page.clone());
            }
        }
        result.is_some()
    });
    result.unwrap()
}

fn fetch_loopback_body(url: &str, max_bytes: usize) -> Vec<u8> {
    let parsed = url::Url::parse(url).unwrap();
    let authority = format!(
        "{}:{}",
        parsed.host_str().unwrap(),
        parsed.port_or_known_default().unwrap()
    );
    let mut socket = TcpStream::connect(&authority).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        socket,
        "GET {} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n",
        parsed.path()
    )
    .unwrap();
    let mut raw = Vec::new();
    socket
        .take((max_bytes + 8192) as u64)
        .read_to_end(&mut raw)
        .unwrap();
    assert!(raw.starts_with(b"HTTP/1.1 200 "));
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    raw[split..].iter().copied().take(max_bytes).collect()
}

#[test]
#[ignore = "build official packages with scripts/test-plugin-v2-official.py"]
fn official_webdav_wasm_requires_host_consent_and_preserves_sources_across_reopen() {
    let server = Server::new();
    let fixture = Fixture::load("IRIS_V2_WEBDAV_PACKAGE");
    let mut one = fixture.open_window();
    let mut second = fixture.open_window();
    assert!(!second.is_connected());
    assert!(matches!(
        second.poll().notices.as_slice(),
        [Notice::Error {
            code: ErrorCode::Backpressure,
            ..
        }]
    ));
    drop(second);

    wait(|| {
        let ready = one.is_ready("iris.webdav");
        check_errors(&one.poll().notices);
        for plugin in one.list() {
            assert!(
                !matches!(plugin.status, PluginStatus::Failed(_)),
                "official load status: {:?}",
                plugin.status
            );
        }
        ready
            && one
                .list()
                .iter()
                .any(|p| p.id == "iris.webdav" && p.status == PluginStatus::Ready)
    });
    check_errors(
        &one.send_to(
            "iris.webdav",
            HostEvent::ModalSubmit {
                id: "webdav.add".into(),
                values: BTreeMap::from([
                    ("name".into(), "Fixture NAS".into()),
                    ("endpoint".into(), format!("{}/dav/", server.origin)),
                    ("auth".into(), "basic".into()),
                    ("username".into(), "user".into()),
                    ("secret".into(), "password".into()),
                ]),
            },
        )
        .notices,
    );
    wait(|| {
        check_errors(&one.poll().notices);
        one.media_sources().len() == 1
    });
    assert_eq!(fixture.host.control_stores_used(), 1);
    let (_, request) = one.browse("iris.webdav", target()).unwrap();
    let mut review = None;
    wait(|| {
        let notices = one.poll().notices;
        check_errors(&notices);
        for notice in notices {
            if let Notice::Origin(r) = notice {
                review = Some(r);
            }
        }
        review.is_some()
    });
    assert!(server.seen.lock().unwrap().is_empty());
    let review = review.take().unwrap();
    assert!(review.private && review.plaintext);
    one.confirm_origin(review.id, true, true).unwrap();
    drop(review);
    let page = await_page(&mut one, request);
    assert_eq!(
        page.folder_id, None,
        "the sidebar requests the implicit root"
    );
    assert_eq!(page.entries[0].id, "/movie.mp4");
    let mut explicit_root = target();
    explicit_root.folder = Some("/".into());
    let (_, request) = one.browse("iris.webdav", explicit_root).unwrap();
    let page = await_page(&mut one, request);
    assert_eq!(page.folder_id.as_deref(), Some("/"));
    assert_eq!(page.entries[0].id, "/movie.mp4");
    let request = one
        .resolve(
            "iris.webdav",
            "fixture-nas",
            Some("/".into()),
            "/movie.mp4",
            None,
        )
        .unwrap();
    let mut playback = None;
    wait(|| {
        let notices = one.poll().notices;
        check_errors(&notices);
        for notice in notices {
            if let Notice::Playback {
                ticket,
                playback: p,
                playlist,
                ..
            } = notice
            {
                assert_eq!(ticket.id, request);
                assert_eq!(playlist[0].entry_id, "/movie.mp4");
                playback = Some(p);
            }
        }
        playback.is_some()
    });
    let playback = playback.take().unwrap();
    assert!(playback.url.starts_with("http://127.0.0.1:"));
    let bytes = fetch_loopback_body(&playback.url, 4096);
    assert_eq!(bytes, b"fixture-video-content");
    assert_eq!(playback.attachments.len(), 1);
    drop(playback);
    // Native seek does not replace the ControlStore scope. Reopening a window
    // reloads persisted state and separately persisted host Origin consent.
    drop(one);
    wait(|| fixture.host.control_stores_used() == 0);
    let mut reopened = fixture.open_window();
    wait(|| {
        let ready = reopened.is_ready("iris.webdav");
        check_errors(&reopened.poll().notices);
        ready
            && reopened
                .list()
                .iter()
                .any(|p| p.id == "iris.webdav" && p.status == PluginStatus::Ready)
            && !reopened.media_sources().is_empty()
    });
    let (_, request) = reopened.browse("iris.webdav", target()).unwrap();
    assert_eq!(
        await_page(&mut reopened, request).entries[0].id,
        "/movie.mp4"
    );
    drop(reopened);
    fixture.shutdown();
}
