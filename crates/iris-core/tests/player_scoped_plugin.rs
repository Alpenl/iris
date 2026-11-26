//! Cross-layer integration tests exercising `iris_core::player` together with
//! `iris_core::plugin_v2` and `iris_api` (kept outside `src/player/` to preserve
//! strict core-domain dependency isolation).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use iris_api::v2::{
    Capability, ErrorCode, Generation, Id, NetworkDeclaration, Role,
    enhance::{EnhancementResource, SubtitleFormat},
    stream::{RemoteResource, StreamSource, StreamSpec},
    task::{ArtifactFormat, Task},
};
use iris_core::{
    library::Kind,
    media_uri::MediaUri,
    player::{Command, Session, SessionEvent, SessionUpdate, TrackKind, prepare},
    plugin_v2::{
        actor::{Executor, Operation, TaskContext},
        enhancement::{AttachmentRegistry, AttachmentSpec},
        files::{FileExecutor, FileSetup, TempRoot},
        hub::{Config, Hub, Notice},
        installation::{Approval, Installer},
        instance::Blob,
        ledger::{Ledger, Resource},
        lut::MonitorLut,
        network::{NetworkGrant, TransportKind},
        package::{Package, Provenance},
        requests::BrowseTarget,
        runtime::{Resources, Setup, WindowAccess},
        stream::{
            manifest::Profile,
            prepare::{AttachmentView, Playback, Preparation},
            reader::MemoryReader,
            relay::BoundRelay,
            remote::RemoteFactory,
            session::{StreamSession, new_session_id},
        },
        ui::PluginManager,
    },
    settings::{Hwdec, Settings},
    vault::CredentialVault,
    vfs::HttpCancel,
};
use ring::digest::{self, Context, SHA256};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("iris-{label}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        Self(dir)
    }
}

impl Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wait_until(timeout: Duration, label: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn drain(player: &mut Session, events: &mut UnboundedReceiver<SessionUpdate>) {
    while let Ok(update) = events.try_recv() {
        player.apply_update(&update);
    }
}

fn plugin_uri(plugin_id: &str, source_id: &str, entry_id: &str) -> MediaUri {
    MediaUri::Plugin {
        plugin_id: plugin_id.into(),
        source_id: source_id.into(),
        entry_id: entry_id.into(),
    }
}

fn read_http_head(socket: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let (mut header, mut byte) = (Vec::new(), [0]);
    while !header.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte)?;
        header.push(byte[0]);
        if header.len() > 16384 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
    }
    Ok(header)
}

fn y4m_video(w: usize, h: usize, frames: usize) -> Vec<u8> {
    let frame_bytes = w * h * 3 / 2;
    format!(
        "YUV4MPEG2 W{w} H{h} F10:1 Ip A1:1 C420jpeg\n{}",
        format!("FRAME\n{}", "x".repeat(frame_bytes)).repeat(frames)
    )
    .into_bytes()
}

fn silent_wav() -> Vec<u8> {
    let data_bytes = 48_000u32 * 2 * 2;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    for chunk in [
        b"RIFF".as_slice(),
        &(36 + data_bytes).to_le_bytes(),
        b"WAVEfmt ",
        &16u32.to_le_bytes(),
        &1u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &48_000u32.to_le_bytes(),
        &96_000u32.to_le_bytes(),
        &2u16.to_le_bytes(),
        &16u16.to_le_bytes(),
        b"data",
        &data_bytes.to_le_bytes(),
    ] {
        out.extend_from_slice(chunk);
    }
    out.resize(out.len() + data_bytes as usize, 0);
    out
}

fn prepare_direct_http(
    server_url: &str,
    owner: &str,
    entry_name: &str,
    title: Option<String>,
) -> (Ledger, NetworkGrant, Playback) {
    let ledger = Ledger::default();
    let caps = BTreeSet::from([Capability::NetHttp]);
    let grant =
        NetworkGrant::new(owner.into(), caps, NetworkDeclaration::default(), &ledger).unwrap();
    let ips = ["127.0.0.1".parse().unwrap()];
    let origin = grant
        .confirm_origin(server_url, TransportKind::Http, &ips)
        .unwrap();
    let primary = RemoteResource {
        url: format!("{server_url}/{entry_name}"),
        headers: Vec::new(),
        origin_grant: Some(origin),
    };
    let spec = StreamSpec {
        source_id: Some("fixture".into()),
        entry_id: Some(entry_name.into()),
        title,
        kind: Some(iris_api::SourceEntryKind::Video),
        is_live: false,
        source: StreamSource::DirectHttp {
            primary,
            fallback_urls: Vec::new(),
            identity: None,
        },
        variants: Vec::new(),
        active_variant: None,
        audio_tracks: Vec::new(),
        subtitles: Vec::new(),
    };
    let prep = Preparation {
        network: grant.clone(),
        ledger: ledger.clone(),
        generation: Generation::default(),
        guest: None,
    };
    let playback = prep
        .prepare(&spec, &BTreeMap::new(), &HttpCancel::default())
        .unwrap_or_else(|e| panic!("{entry_name} preparation: {e:?}"));
    (ledger, grant, playback)
}

#[test]
fn native_receipts_hold_revoked_attachments_and_release_accounting() {
    let root = TempDir::new("enhance");
    let resources = Resources::new().unwrap();
    let ledger = resources.ledger.clone();
    let caps = BTreeSet::from([Capability::MediaEnhance]);
    let network = NetworkGrant::new(
        "enhance-test".into(),
        caps.clone(),
        NetworkDeclaration::default(),
        &ledger,
    )
    .unwrap();
    let files = FileExecutor::new(FileSetup {
        network: network.clone(),
        ledger: ledger.clone(),
        capabilities: caps.clone(),
        temporary_directory: root.to_path_buf(),
    })
    .unwrap();
    let access = WindowAccess {
        instance: Id(1),
        window: Id(2),
        generation: Generation::default(),
        owner: "enhance-test".into(),
        capabilities: caps,
        network,
        files,
        resources,
        cancel: HttpCancel::default(),
        attachments: Arc::new(AttachmentRegistry::default()),
    };
    let run = |task, blobs: &BTreeMap<u64, Arc<Blob>>| {
        let context = TaskContext {
            instance_id: access.instance,
            window: Some(access.window),
            owner: access.owner.clone(),
            role: Role::Control,
            generation: access.generation,
            cancel: access.cancel.clone(),
            ledger: ledger.clone(),
        };
        match access.files.prepare(task, blobs, context).unwrap() {
            Operation::Blocking(job) => job().unwrap(),
            _ => panic!("artifact worker"),
        }
    };
    let begin = Task::ArtifactBegin {
        format: ArtifactFormat::WebVtt,
    };
    let created = run(begin, &BTreeMap::new());
    let writer = Id(created.value["artifact_writer"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap());
    let vtt = b"WEBVTT\n\n00:00.000 --> 00:03.000\nReceipt\n".to_vec();
    let blobs = BTreeMap::from([(9, Arc::new(Blob::new(&ledger, &access.owner, vtt).unwrap()))]);
    let write_task = Task::ArtifactWrite {
        handle: writer,
        blob: Id(9),
        finish: true,
    };
    let written = run(write_task, &blobs);
    let artifact = Id(written.value["artifact"].as_str().unwrap().parse().unwrap());
    let spec = || AttachmentSpec {
        id: "subtitle".into(),
        label: "Receipt".into(),
        lang: None,
        subtitle: Some(SubtitleFormat::WebVtt),
        resource: EnhancementResource::Artifact { handle: artifact },
    };
    let mut wrong = spec();
    wrong.subtitle = Some(SubtitleFormat::Ass);
    assert!(matches!(
        access.prepare_attachment(wrong),
        Err(ErrorCode::InvalidArgument)
    ));
    let mut unauthorized = access.clone();
    unauthorized.owner = "another-installation".into();
    assert!(matches!(
        unauthorized.prepare_attachment(spec()),
        Err(ErrorCode::Denied)
    ));
    drop(unauthorized);
    let track = access.prepare_attachment(spec()).unwrap();
    let extra: Vec<_> = (0..7)
        .map(|_| access.prepare_attachment(spec()).unwrap())
        .collect();
    assert!(matches!(
        access.prepare_attachment(spec()),
        Err(ErrorCode::QuotaExceeded)
    ));
    drop(extra);
    assert_eq!(
        ledger.used(Some(&access.owner), Resource::EnhancementTrack),
        1
    );

    let path = root.join("video.y4m");
    std::fs::write(&path, y4m_video(2, 2, 60)).unwrap();
    let mut player = Session::new_headless(true).unwrap();
    let mut events = player.events().unwrap();
    player.open(prepare(&path, None).unwrap(), 0.).unwrap();
    wait_until(Duration::from_secs(8), "media load", || {
        drain(&mut player, &mut events);
        player.state().loaded
    });

    player.attach_plugin_track(track.clone()).unwrap();
    wait_until(Duration::from_secs(8), "first plugin subtitle", || {
        drain(&mut player, &mut events);
        player.flush_deferred();
        player
            .selected_track(TrackKind::Sub)
            .is_some_and(|sub| sub.title == "Receipt")
    });
    let first_id = player.selected_track(TrackKind::Sub).unwrap().id;

    // Attaching a secondary plugin subtitle must not steal selection from the first.
    let mut second_spec = spec();
    second_spec.id = "subtitle-en".into();
    second_spec.label = "English".into();
    second_spec.lang = Some("en".into());
    let second = access.prepare_attachment(second_spec).unwrap();
    player.attach_plugin_track(second.clone()).unwrap();
    wait_until(Duration::from_secs(8), "second plugin subtitle", || {
        drain(&mut player, &mut events);
        player.flush_deferred();
        player.tracks_of(TrackKind::Sub).len() == 2
    });
    assert_eq!(
        player.selected_track(TrackKind::Sub).map(|s| s.id),
        Some(first_id)
    );
    drop(second);
    player.revoke_plugin_tracks(&access.owner);
    drop(track);
    assert!(ledger.used(None, Resource::EnhancementTrack) >= 1);
    wait_until(Duration::from_secs(8), "revoked track release", || {
        drain(&mut player, &mut events);
        ledger.used(None, Resource::EnhancementTrack) == 0
    });

    // Revoke an addition before the asynchronous native acknowledgement arrives.
    let track = access.prepare_attachment(spec()).unwrap();
    player.attach_plugin_track(track).unwrap();
    player.revoke_plugin_tracks(&access.owner);
    assert_eq!(ledger.used(None, Resource::EnhancementTrack), 1);
    wait_until(Duration::from_secs(8), "pending add revocation", || {
        drain(&mut player, &mut events);
        ledger.used(None, Resource::EnhancementTrack) == 0
    });
    player.shutdown();
    drop((player, events));
    access.cancel.cancel();
    access.files.stop();
    drop((created, written, blobs));
    wait_until(Duration::from_secs(8), "file executor quiescence", || {
        access.files.is_quiescent()
    });
    drop(access);
    wait_until(Duration::from_secs(8), "ledger cleanup", || {
        ledger.used(None, Resource::HostBytes) == 0 && ledger.used(None, Resource::TempBytes) == 0
    });
}

#[test]
fn monitor_lut_artifact_loads_through_session_and_releases_ledger() {
    let parent = TempDir::new("lut-[,;' space");
    let ledger = Ledger::default();
    let root = TempRoot::new(&parent, "lut-test".into(), ledger.clone()).unwrap();
    let cancel = HttpCancel::default();
    let mut writer = root.writer("cube").unwrap();
    let cube = format!("LUT_3D_SIZE 2\n{}", "1 0 0\n".repeat(8));
    writer.write(cube.as_bytes(), &cancel).unwrap();
    let lut = MonitorLut::prepare(writer.finish(&cancel).unwrap(), &ledger, &cancel).unwrap();

    let clip_path = parent.join("clip.y4m");
    std::fs::write(&clip_path, y4m_video(16, 16, 30)).unwrap();
    let mut session = Session::new_headless(true).unwrap();
    let mut events = session.events().unwrap();
    session
        .open(prepare(&clip_path, None).unwrap(), 0.)
        .unwrap();
    wait_until(Duration::from_secs(5), "lut video load", || {
        drain(&mut session, &mut events);
        session.state().loaded
    });

    session.set_monitor_lut(Some(lut.clone())).unwrap();
    wait_until(Duration::from_secs(5), "native monitor LUT filter", || {
        let d = session.backend_diagnostics();
        d.vf.contains("iris-monitor-lut") && d.out_pixel_format.starts_with("rgb")
    });
    drop(lut);
    session.stop().unwrap();
    session.shutdown();
    drop((session, events));
    wait_until(Duration::from_secs(5), "temp root quiescence", || {
        root.is_quiescent()
    });
    drop(root);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::TempBytes), 0);
}

#[test]
fn rapid_lut_replacements_and_stop_hold_artifacts_until_native_receipts() {
    let parent = TempDir::new("lut-rapid");
    let ledger = Ledger::default();
    let root = TempRoot::new(&parent, "lut-rapid".into(), ledger.clone()).unwrap();
    let cancel = HttpCancel::default();
    let make_lut = |rgb: &str| {
        let mut writer = root.writer("cube").unwrap();
        let cube = format!("LUT_3D_SIZE 2\n{}", format!("{rgb}\n").repeat(8));
        writer.write(cube.as_bytes(), &cancel).unwrap();
        MonitorLut::prepare(writer.finish(&cancel).unwrap(), &ledger, &cancel).unwrap()
    };
    let lut1 = make_lut("1 0 0");
    let lut2 = make_lut("0 1 0");
    let lut3 = make_lut("0 0 1");

    let clip_path = parent.join("clip.y4m");
    std::fs::write(&clip_path, y4m_video(16, 16, 30)).unwrap();
    let mut session = Session::new_headless(true).unwrap();
    let mut events = session.events().unwrap();
    session
        .open(prepare(&clip_path, None).unwrap(), 0.)
        .unwrap();
    wait_until(Duration::from_secs(5), "rapid lut video load", || {
        drain(&mut session, &mut events);
        session.state().loaded
    });

    session.set_monitor_lut(Some(lut1)).unwrap();
    session.set_monitor_lut(Some(lut2)).unwrap();
    session.set_monitor_lut(Some(lut3)).unwrap();
    session.stop().unwrap();

    // Even while `session` remains alive, draining native CommandReply receipts
    // must release all three retired/stopped LUT artifacts and zero the ledger.
    wait_until(
        Duration::from_secs(5),
        "retired LUT native receipt cleanup",
        || {
            drain(&mut session, &mut events);
            root.is_quiescent()
                && ledger.used(None, Resource::HostBytes) == 0
                && ledger.used(None, Resource::TempBytes) == 0
        },
    );
    session.shutdown();
}

#[test]
fn actual_mpv_opens_scoped_video_audio_and_subtitles_then_restores_reference_policy() {
    let ledger = Ledger::default();
    let grant = NetworkGrant::new(
        "mpv-fixture".into(),
        BTreeSet::new(),
        NetworkDeclaration::default(),
        &ledger,
    )
    .unwrap();
    let factory = RemoteFactory::new(grant.clone(), ledger.clone()).unwrap();
    let bound = BoundRelay::bind().unwrap();
    let session = StreamSession::new(
        new_session_id().unwrap(),
        Generation::default(),
        bound.authority().unwrap(),
        factory,
        None,
    )
    .unwrap();
    let mem = |bytes: Vec<u8>| Arc::new(MemoryReader::new(bytes, "mpv-fixture", &ledger).unwrap());
    session
        .commit(session.prepare_reader(mem(y4m_video(2, 2, 60))).unwrap())
        .unwrap();
    let audio = session
        .add_attachment("audio", "bin", mem(silent_wav()))
        .unwrap();
    let vtt = b"WEBVTT\n\n00:00.000 --> 00:02.000\nScoped subtitle\n".to_vec();
    let sub = session.add_attachment("sub", "vtt", mem(vtt)).unwrap();
    let att = |id: &str, url: String, label: &str, audio: bool| AttachmentView {
        id: id.into(),
        url,
        label: label.into(),
        lang: Some("en".into()),
        default: true,
        audio,
    };
    let playback = Arc::new(Playback {
        relay: bound.start(session.clone()).unwrap(),
        url: session.root_url().unwrap(),
        profile: None,
        kind: Kind::Video,
        title: "Fixture".into(),
        source_id: None,
        entry_id: None,
        is_live: false,
        attachments: vec![
            att("audio", audio, "Audio", true),
            att("subtitle", sub, "Subtitle", false),
        ],
        variants: Vec::new(),
        active_variant: None,
    });
    let mut player = Session::new_headless(true).unwrap();
    let mut events = player.events().unwrap();
    let uri = plugin_uri("fixture", "source", "media");
    player
        .open_scoped(
            playback.clone().into_scoped(),
            uri,
            &Settings::default(),
            0.,
        )
        .unwrap();
    wait_until(
        Duration::from_secs(8),
        "scoped video/audio/sub load",
        || {
            drain(&mut player, &mut events);
            player.state().loaded && player.backend_diagnostics().track_count >= 3
        },
    );
    let diag = player.backend_diagnostics();
    assert_eq!(diag.access_references, "no");
    assert_eq!(diag.demuxer, "lavf");
    assert!(diag.paused);
    assert!(
        player
            .execute(Command::SetAbLoop {
                start: f64::NAN,
                end: 1.
            })
            .is_err()
    );
    player
        .execute(Command::SetAbLoop {
            start: 0.2,
            end: 0.6,
        })
        .unwrap();
    player.execute(Command::TogglePlayback).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut previous, mut loops) = (0., 0);
    while loops < 3 {
        drain(&mut player, &mut events);
        let position = player.state().time_pos;
        if previous > position + 0.15 {
            loops += 1;
        }
        previous = position;
        assert!(
            Instant::now() < deadline,
            "native A-B did not repeat: loops={loops}, position={position}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(player.state().ab_start, Some(0.2));
    assert_eq!(player.state().ab_end, Some(0.6));
    player.execute(Command::ClearAbLoop).unwrap();
    let mut cleared = false;
    wait_until(Duration::from_secs(2), "A-B loop clear", || {
        while let Ok(update) = events.try_recv() {
            if matches!(
                player.apply_update(&update),
                Some(SessionEvent::AbEnd(None))
            ) {
                cleared = true;
            }
        }
        cleared
    });
    player.execute(Command::SeekAbsolute(1.)).unwrap();
    assert!(session.generation().seek.0 >= 1);
    player.stop().unwrap();
    wait_until(Duration::from_secs(3), "relay quiescence", || {
        playback.relay.is_quiescent()
    });
    player.shutdown();
    drop((player, events, playback, session, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::RelayWorker), 0);
}

struct FixtureServer {
    url: String,
    stopped: Arc<AtomicBool>,
    requests: Arc<Mutex<BTreeSet<String>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FixtureServer {
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
                let name = path.file_name().unwrap().to_str().unwrap();
                files.insert(format!("/{name}"), bytes);
            }
        }
        assert!(!files.is_empty());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(BTreeSet::new()));
        let (done, recorded) = (stopped.clone(), requests.clone());
        let thread = thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                if let Ok((mut socket, _)) = listener.accept() {
                    if let Err(error) = respond_fixture(&mut socket, &files, &recorded) {
                        eprintln!("fixture transport: {error}");
                    }
                } else {
                    thread::sleep(Duration::from_millis(2));
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

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

fn respond_fixture(
    socket: &mut TcpStream,
    files: &BTreeMap<String, Vec<u8>>,
    requests: &Mutex<BTreeSet<String>>,
) -> std::io::Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let header =
        String::from_utf8(read_http_head(socket)?).map_err(|_| std::io::ErrorKind::InvalidData)?;
    let mut fields = header.lines().next().unwrap().split_whitespace();
    let (method, path) = (fields.next().unwrap(), fields.next().unwrap());
    let Some(bytes) = files.get(path) else {
        return socket.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
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
            let end = if b.is_empty() {
                bytes.len() - 1
            } else {
                b.parse::<usize>().unwrap().min(bytes.len() - 1)
            };
            (a.parse::<usize>().unwrap(), end)
        }
        None => (0, bytes.len() - 1),
    };
    if start >= bytes.len() || start > end {
        return write!(
            socket,
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            bytes.len()
        );
    }
    let (status, range_hdr) = if range.is_some() {
        (
            "206 Partial Content",
            format!("Content-Range: bytes {start}-{end}/{}\r\n", bytes.len()),
        )
    } else {
        ("200 OK", String::new())
    };
    write!(
        socket,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"fixture-v1\"\r\n{range_hdr}Connection: close\r\n\r\n",
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
    let server = FixtureServer::start(Path::new(&directory));
    let name = "hls.m3u8";
    {
        let (ledger, grant, playback) =
            prepare_direct_http(&server.url, "adaptive-native", name, Some(name.into()));
        let playback = Arc::new(playback);
        assert_eq!(playback.profile, Some(Profile::Hls));
        let mut player = Session::new_headless(false).unwrap();
        player.set_hwdec(Hwdec::Off);
        let init_diag = player.backend_diagnostics();
        println!(
            "{name}: mpv={} ffmpeg={}",
            init_diag.mpv_version, init_diag.ffmpeg_version
        );
        let mut events = player.events().unwrap();
        let uri = plugin_uri("fixture", "adaptive", name);
        player
            .open_scoped(
                playback.clone().into_scoped(),
                uri,
                &Settings::default(),
                0.,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let (mut samples, mut sought) = (Vec::<f64>::new(), false);
        loop {
            drain(&mut player, &mut events);
            if player.state().loaded && player.state().time_pos > 0.5 {
                let sync = player.backend_diagnostics().avsync.expect("finite avsync");
                samples.push(sync.abs());
                assert_eq!(player.state().video_size.0, 160);
                assert_eq!(player.media_stats().audio_sample_rate, Some(48_000));
                if !sought && player.state().time_pos > 1.0 {
                    player.execute(Command::SeekAbsolute(3.)).unwrap();
                    sought = true;
                }
                if sought && player.state().time_pos > 4.5 {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{name} stalled: loaded={} position={} format={:?}",
                player.state().loaded,
                player.state().time_pos,
                player.backend_diagnostics().file_format
            );
            thread::sleep(Duration::from_millis(20));
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
        assert_eq!(player.backend_diagnostics().access_references, "no");
        player.stop().unwrap();
        player.shutdown();
        drop((player, events));
        wait_until(Duration::from_secs(5), "adaptive quiescence", || {
            playback.relay.is_quiescent()
        });
        drop((playback, grant));
        assert_eq!(ledger.used(None, Resource::HostBytes), 0);
        assert_eq!(ledger.used(None, Resource::PlaybackSession), 0);
    }
    let requests = server.requests.lock().unwrap();
    assert!(requests.iter().any(|p| p.starts_with("/hls-")));
}

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
    let server = FixtureServer::start(directory);
    let (ledger, grant, playback) =
        prepare_direct_http(&server.url, "transport-benchmark", "reference.mp4", None);
    let url = url::Url::parse(&playback.url).unwrap();
    let authority = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    let started = Instant::now();
    let (mut samples, mut total, mut misses, mut peak_host) = (Vec::new(), 0_u64, 0, 0);
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
        assert!(
            read_http_head(&mut socket)
                .unwrap()
                .starts_with(b"HTTP/1.1 200")
        );
        let (mut hash, mut bytes, mut buffer) = (Context::new(&SHA256), 0_u64, [0_u8; 65536]);
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
        samples.push(serde_json::json!({
            "bytes": bytes, "fetch_ms": elapsed.as_secs_f64() * 1000.,
            "bytes_per_second": bytes as f64 / elapsed.as_secs_f64(),
            "deadline_missed": elapsed > budget
        }));
        if started.elapsed() >= next_progress {
            println!(
                "transport: {:.0}s, {} reads, {misses} deadline misses",
                started.elapsed().as_secs_f64(),
                samples.len()
            );
            next_progress += Duration::from_secs(60);
        }
        thread::sleep(
            budget
                .saturating_sub(start.elapsed())
                .min(Duration::from_secs(600).saturating_sub(started.elapsed())),
        );
    }
    let elapsed = started.elapsed().as_secs_f64();
    let cleanup = Instant::now();
    playback.relay.stop();
    wait_until(Duration::from_secs(5), "transport quiescence", || {
        playback.relay.is_quiescent()
    });
    drop((playback, grant));
    for resource in [
        Resource::HostBytes,
        Resource::PlaybackSession,
        Resource::RelayWorker,
        Resource::Handle,
    ] {
        assert_eq!(ledger.used(None, resource), 0);
    }
    let receipt = serde_json::json!({
        "elapsed_seconds": elapsed, "total_bytes": total,
        "target_bytes_per_second": peak as f64 * 1.25, "paced_bytes_per_second": total as f64 / elapsed,
        "deadline_misses": misses, "samples": samples, "sampled_host_ledger_bytes": peak_host,
        "cleanup_ms": cleanup.elapsed().as_secs_f64() * 1000., "debug": cfg!(debug_assertions),
        "scope": "DirectHttp local RAM origin + authorized relay; repeated complete 4K sample; no decoder, display, Wasm transformation or WAN; deadline misses are transport-budget misses, not player underruns"
    });
    println!("IRIS_V2_TRANSPORT_RECEIPT={receipt}");
    assert_eq!(misses, 0, "transport failed 1.25x peak-one-second budget");
}

#[path = "player_scoped_plugin/live.rs"]
mod live;
