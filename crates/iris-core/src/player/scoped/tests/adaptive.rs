use super::*;
use crate::plugin_v2::stream::manifest::Profile;
mod transport;
use crate::{
    plugin_v2::{network::TransportKind, stream::prepare::Preparation},
    vfs::HttpCancel,
};
use iris_api::v2::{
    Capability,
    stream::{RemoteResource, StreamSource, StreamSpec},
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct Server {
    url: String,
    stopped: Arc<AtomicBool>,
    requests: Arc<Mutex<BTreeSet<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn start(directory: &Path) -> Self {
        let mut files = BTreeMap::new();
        let mut total = 0;
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file()
                && path.extension().is_some_and(|e| {
                    matches!(e.to_str(), Some("m3u8" | "mpd" | "ts" | "m4s" | "mp4"))
                })
            {
                let bytes = std::fs::read(&path).unwrap();
                total += bytes.len();
                assert!(total <= 64 << 20 && files.len() < 128);
                files.insert(
                    format!("/{}", path.file_name().unwrap().to_str().unwrap()),
                    bytes,
                );
            }
        }
        assert!(!files.is_empty());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let done = stopped.clone();
        let requests = Arc::new(Mutex::new(BTreeSet::new()));
        let recorded = requests.clone();
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                if let Ok((mut socket, _)) = listener.accept() {
                    if let Err(error) = respond(&mut socket, &files, &recorded) {
                        eprintln!("fixture transport: {error}");
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        });
        Self {
            url,
            stopped,
            requests,
            thread: Some(thread),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn respond(
    socket: &mut TcpStream,
    files: &BTreeMap<String, Vec<u8>>,
    requests: &Mutex<BTreeSet<String>>,
) -> std::io::Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte)?;
        header.push(byte[0]);
        if header.len() > 16384 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
    }
    let header = String::from_utf8(header).map_err(|_| std::io::ErrorKind::InvalidData)?;
    let mut fields = header.lines().next().unwrap().split_whitespace();
    let method = fields.next().unwrap();
    let path = fields.next().unwrap();
    let Some(bytes) = files.get(path) else {
        socket.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        return Ok(());
    };
    requests.lock().unwrap().insert(path.into());
    let range = header.lines().find_map(|line| {
        line.split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case("range"))
            .map(|(_, v)| v.trim())
    });
    let (start, end) = match range {
        Some(value) => {
            let (a, b) = value
                .strip_prefix("bytes=")
                .unwrap()
                .split_once('-')
                .unwrap();
            (
                a.parse::<usize>().unwrap(),
                if b.is_empty() {
                    bytes.len() - 1
                } else {
                    b.parse::<usize>().unwrap().min(bytes.len() - 1)
                },
            )
        }
        None => (0, bytes.len() - 1),
    };
    if start >= bytes.len() || start > end {
        write!(
            socket,
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            bytes.len()
        )?;
        return Ok(());
    }
    let range_header = if range.is_some() {
        format!("Content-Range: bytes {start}-{end}/{}\r\n", bytes.len())
    } else {
        String::new()
    };
    write!(
        socket,
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"fixture-v1\"\r\n{range_header}Connection: close\r\n\r\n",
        if range.is_some() {
            "206 Partial Content"
        } else {
            "200 OK"
        },
        end - start + 1
    )?;
    if method != "HEAD" {
        socket.write_all(&bytes[start..=end])?;
    }
    Ok(())
}

#[test]
#[ignore = "run scripts/test-plugin-v2-playback.py with a real FFmpeg fixture generator"]
fn real_hls_decode_through_scoped_rewritten_manifest() {
    let directory = std::env::var_os("IRIS_V2_ADAPTIVE_FIXTURE").expect("native fixture directory");
    let server = Server::start(Path::new(&directory));
    for (name, profile) in [("hls.m3u8", Profile::Hls)] {
        let ledger = Ledger::default();
        let grant = NetworkGrant::new(
            "adaptive-native".into(),
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
        let request = RemoteResource {
            url: format!("{}/{name}", server.url),
            headers: Vec::new(),
            origin_grant: Some(origin),
        };
        let spec = StreamSpec {
            source_id: Some("fixture".into()),
            entry_id: Some(name.into()),
            title: Some(name.into()),
            kind: Some(iris_api::SourceEntryKind::Video),
            is_live: false,
            source: StreamSource::DirectHttp {
                primary: request,
                fallback_urls: Vec::new(),
                identity: None,
            },
            variants: Vec::new(),
            active_variant: None,
            audio_tracks: Vec::new(),
            subtitles: Vec::new(),
        };
        let playback = Arc::new(
            Preparation {
                network: grant.clone(),
                ledger: ledger.clone(),
                generation: Generation::default(),
                guest: None,
            }
            .prepare(&spec, &BTreeMap::new(), &HttpCancel::default())
            .unwrap_or_else(|e| panic!("{name} preparation: {e:?}")),
        );
        assert_eq!(playback.profile, Some(profile));
        let mut player = Session::new().unwrap();
        println!(
            "{name}: mpv={} ffmpeg={}",
            player.mpv.get_property_string("mpv-version").unwrap(),
            player.mpv.get_property_string("ffmpeg-version").unwrap()
        );
        player.mpv.set_property_string("vo", "null").unwrap();
        player.mpv.set_property_string("ao", "null").unwrap();
        player.mpv.set_property_string("hwdec", "no").unwrap();
        let mut events = player.events().unwrap();
        player
            .open_scoped(
                playback.clone().into_scoped(),
                MediaUri::Plugin {
                    plugin_id: "fixture".into(),
                    source_id: "adaptive".into(),
                    entry_id: name.into(),
                },
                &Settings::default(),
                0.,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut samples = Vec::<f64>::new();
        let mut sought = false;
        loop {
            while let Ok(update) = events.try_recv() {
                player.apply_update(&update);
            }
            if player.state.loaded && player.state.time_pos > 0.5 {
                let sync = player.mpv.get_property_f64("avsync").unwrap();
                assert!(sync.is_finite());
                samples.push(sync.abs());
                assert_eq!(
                    player
                        .mpv
                        .get_property_string("video-out-params/w")
                        .unwrap(),
                    "160"
                );
                assert_eq!(
                    player
                        .mpv
                        .get_property_string("audio-params/samplerate")
                        .unwrap(),
                    "48000"
                );
                if !sought && player.state.time_pos > 1.0 {
                    player.execute(Command::SeekAbsolute(3.)).unwrap();
                    sought = true;
                }
                if sought && player.state.time_pos > 4.5 {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{name} stalled: loaded={} position={} error={:?}",
                player.state.loaded,
                player.state.time_pos,
                player.mpv.get_property_string("file-format")
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        samples.sort_by(f64::total_cmp);
        let p95 = samples[samples.len() * 95 / 100];
        assert!(
            samples.len() >= 20 && p95 < 0.15,
            "{name} avsync p95 {p95}, n={}",
            samples.len()
        );
        println!(
            "{name}: native decode/seek passed; null output |avsync| p95={p95:.6}s n={}",
            samples.len()
        );
        assert_eq!(
            player.mpv.get_property_string("access-references").unwrap(),
            "no"
        );
        player.stop().unwrap();
        player.shutdown();
        drop(player);
        drop(events);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !playback.relay.is_quiescent() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(playback);
        drop(grant);
        assert_eq!(ledger.used(None, Resource::HostBytes), 0);
        assert_eq!(ledger.used(None, Resource::PlaybackSession), 0);
    }
    let requests = server.requests.lock().unwrap();
    assert!(requests.iter().any(|p| p.starts_with("/hls-")));
}
