use super::super::tests::{factory, resource, server};
use super::*;
use crate::ledger::Resource;
fn response(text: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )
    .into_bytes()
}
fn render(converted: &Converted) -> Result<manifest::Manifest, ErrorCode> {
    manifest::compile(converted.format, &converted.bytes, &|id, _| {
        Ok(format!("http://127.0.0.1:50123/token/{id}"))
    })
}
#[test]
fn remote_hls_resolves_audio_groups_keys_relative_signed_urls_and_emits_only_local_references() {
    let (factory, grant, ledger) = factory();
    let master = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"main\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"a1.m3u8\"\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"main\",NAME=\"Chinese\",LANGUAGE=\"zh\",DEFAULT=NO,AUTOSELECT=YES,URI=\"a2.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=3840x2160,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"main\"\nv.m3u8\n";
    let media = |segment: &str| {
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-KEY:METHOD=AES-128,URI=\"key?id=x&sig=y\",IV=0x00000000000000000000000000000001\n#EXTINF:2,\n{segment}\n#EXT-X-ENDLIST\n"
        )
    };
    let (url, rx, thread) = server(vec![
        response(master),
        response(&media("a1.ts")),
        response(&media("a2.ts")),
        response(&media("video.ts")),
    ]);
    let converted = convert(&factory, &resource(&grant, &url), &HttpCancel::default()).unwrap();
    assert!(!converted.live);
    let input: VirtualHls =
        iris_core::bounded_json::decode_exact(&converted.bytes, manifest::INPUT_LIMIT).unwrap();
    assert_eq!(input.tracks.len(), 3);
    assert_eq!(input.resources.len(), 4);
    let audio: Vec<_> = input
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Audio)
        .collect();
    assert_eq!(audio[0].group, audio[1].group);
    assert!(audio[0].default);
    assert!(!audio[1].default);
    assert_eq!(input.variants[0].audio, audio[0].group);
    let output = render(&converted).unwrap();
    assert_eq!(output.documents.len(), 4);
    for doc in output.documents {
        let text = std::str::from_utf8(&doc.bytes).unwrap();
        assert!(!text.contains("sig=y"));
        assert!(!text.contains("video.ts"));
        assert!(text.contains("http://127.0.0.1:50123/token/"));
    }
    let requests: Vec<_> = (0..4).map(|_| rx.recv().unwrap()).collect();
    assert!(requests[1].starts_with("GET /a1.m3u8 "));
    assert!(requests[3].starts_with("GET /v.m3u8 "));
    thread.join().unwrap();
    drop(converted);
    drop(factory);
    drop(grant);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}
#[test]
fn remote_parsers_reject_private_origin_escape_xml_and_unknown_semantics() {
    let bodies = [
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nhttp://127.0.0.1:9/internal\n#EXT-X-ENDLIST\n",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-CONTENT-STEERING:SERVER-URI=\"file:///secret\"\n#EXTINF:1,\nsegment.ts\n",
        "<!DOCTYPE MPD [<!ENTITY external SYSTEM 'file:///secret'>]><MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\">&external;</MPD>",
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"static\" mediaPresentationDuration=\"PT1S\"><Period/></MPD>",
    ];
    for body in bodies {
        let (factory, grant, _) = factory();
        let (url, rx, thread) = server(vec![response(body)]);
        assert!(
            convert(&factory, &resource(&grant, &url), &HttpCancel::default()).is_err(),
            "{body}"
        );
        rx.recv().unwrap();
        thread.join().unwrap();
    }
}

#[test]
fn remote_hls_accepts_zero_padded_durations_and_rejects_submillisecond_precision() {
    let hls = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.000000,\nsegment.ts\n#EXT-X-ENDLIST\n";
    let cases = [
        (hls.to_string(), true),
        (hls.replace("1.000000", "0.999999"), false),
    ];
    for (body, accepted) in cases {
        let (factory, grant, _) = factory();
        let (url, rx, thread) = server(vec![response(&body)]);
        let result = convert(&factory, &resource(&grant, &url), &HttpCancel::default());
        assert_eq!(result.is_ok(), accepted);
        if let Ok(converted) = result {
            assert!(render(&converted).is_ok());
        }
        rx.recv().unwrap();
        thread.join().unwrap();
    }
}

#[test]
fn remote_hls_target_duration_rounds_to_the_nearest_second() {
    for ended in [false, true] {
        for (duration, accepted) in [
            (3999, true),
            (4000, true),
            (4040, true),
            (4499, true),
            (4500, false),
            (4501, false),
            (5000, false),
        ] {
            let body = format!(
                "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXTINF:{}.{:03},\nsegment.ts\n{}",
                duration / 1000,
                duration % 1000,
                if ended { "#EXT-X-ENDLIST\n" } else { "" },
            );
            let (factory, grant, _) = factory();
            let (url, rx, thread) = server(vec![response(&body)]);
            let result = convert(&factory, &resource(&grant, &url), &HttpCancel::default());
            if accepted {
                let converted = result.unwrap_or_else(|e| panic!("{body}: {e:?}"));
                assert_eq!(converted.live, !ended);
                let input: VirtualHls =
                    iris_core::bounded_json::decode_exact(&converted.bytes, manifest::INPUT_LIMIT)
                        .unwrap();
                assert_eq!(input.tracks[0].segments[0].duration_ms.0, duration);
                assert!(render(&converted).is_ok());
            } else {
                assert!(matches!(result, Err(ErrorCode::InvalidArgument)), "{body}");
            }
            rx.recv().unwrap();
            thread.join().unwrap();
        }
    }
}

#[test]
fn live_refresh_keeps_root_and_attachments_but_revokes_evicted_segments() {
    use crate::stream::{reader::MemoryReader, session::StreamSession};
    use iris_api::v2::{Generation, Id};
    use std::{sync::Arc, time::Duration};
    let (factory, grant, ledger) = factory();
    let playlist = |sequence, first, second| {
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{sequence}\n#EXTINF:1,\n{first}.ts\n#EXTINF:1,\n{second}.ts\n"
        )
    };
    let (url, rx, thread) = server(vec![
        response(&playlist(1, "a", "b")),
        response(&playlist(2, "b", "c")),
        response("#EXTM3U\n#EXT-X-UNKNOWN:bad\n"),
    ]);
    let source = resource(&grant, &url);
    let mut document = convert(&factory, &source, &HttpCancel::default()).unwrap();
    document.refresh_after = Duration::ZERO;
    let session = StreamSession::new(
        Id(71),
        Generation::default(),
        "127.0.0.1:45123".parse().unwrap(),
        factory,
        None,
    )
    .unwrap();
    session
        .commit(
            session
                .prepare_manifest(Id(0), document.format, &document.bytes)
                .unwrap(),
        )
        .unwrap();
    let root = session.root_url().unwrap();
    let path = |url: &str| url::Url::parse(url).unwrap().path().to_owned();
    let read = || {
        let access = session.open(&path(&root), &HttpCancel::default()).unwrap();
        let bytes = access.reader.read_at(0, 65536, &access.cancel).unwrap();
        String::from_utf8(bytes.blob.bytes.clone())
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("http:"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let old = read();
    let old_access = session
        .open(&path(&old[0]), &HttpCancel::default())
        .unwrap();
    let subtitle = session
        .add_attachment(
            "sub_en",
            "vtt",
            Arc::new(MemoryReader::new(b"WEBVTT\n".to_vec(), "stream-tests", &ledger).unwrap()),
        )
        .unwrap();
    live::attach(&session, source.clone(), &document).unwrap();
    let updated = read();
    assert_eq!(root, session.root_url().unwrap());
    assert_eq!(old[1], updated[0]);
    assert_ne!(old[0], updated[1]);
    assert!(old_access.cancel.is_cancelled());
    assert!(
        session
            .open(&path(&old[0]), &HttpCancel::default())
            .is_err()
    );
    assert!(
        session
            .open(&path(&subtitle), &HttpCancel::default())
            .is_ok()
    );
    let revision = session.revision();
    live::attach(&session, source, &document).unwrap();
    assert!(session.open(&path(&root), &HttpCancel::default()).is_err());
    assert_eq!(session.revision(), revision);
    assert!(
        session
            .open(&path(&updated[0]), &HttpCancel::default())
            .is_ok()
    );
    for _ in 0..3 {
        rx.recv().unwrap();
    }
    thread.join().unwrap();
    drop(old_access);
    drop(document);
    drop(session);
    drop(grant);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::PlaybackSession), 0);
}

#[test]
fn live_refresh_concurrent_opens_wait_for_fresh_snapshot_and_respect_cancel() {
    use crate::stream::session::StreamSession;
    use iris_api::v2::{Generation, Id};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Condvar, Mutex, mpsc},
        time::{Duration, Instant},
    };

    let master = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Main\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,AUDIO=\"aud\"\nvideo.m3u8\n";
    let playlist = |seq: u64, seg: &str| {
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{seq}\n#EXTINF:1,\n{seg}.ts\n"
        )
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/master.m3u8", listener.local_addr().unwrap());
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let release_server = Arc::clone(&release);
    let (refresh_started_tx, refresh_started_rx) = mpsc::channel();
    let (master_hits_tx, master_hits_rx) = mpsc::channel();

    let server_thread = std::thread::spawn(move || {
        let mut master_count = 0usize;
        // 3 requests for initial convert (master, audio, video) + 3 for single refresh.
        for _ in 0..6 {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut req = Vec::new();
            let mut b = [0u8; 1];
            while !req.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut b).unwrap();
                req.push(b[0]);
            }
            let head = String::from_utf8(req).unwrap();
            let line = head.lines().next().unwrap_or("");
            let body = if line.starts_with("GET /master.m3u8 ") {
                master_count += 1;
                master_hits_tx.send(master_count).unwrap();
                if master_count == 2 {
                    refresh_started_tx.send(()).unwrap();
                    let (lock, cv) = &*release_server;
                    let mut ready = lock.lock().unwrap();
                    while !*ready {
                        ready = cv.wait(ready).unwrap();
                    }
                }
                master.to_owned()
            } else if line.starts_with("GET /audio.m3u8 ") {
                if master_count == 1 {
                    playlist(1, "a_old")
                } else {
                    playlist(2, "a_new")
                }
            } else if line.starts_with("GET /video.m3u8 ") {
                if master_count == 1 {
                    playlist(1, "v_old")
                } else {
                    playlist(2, "v_new")
                }
            } else {
                panic!("unexpected path: {line}");
            };
            socket.write_all(&response(&body)).unwrap();
        }
    });

    let (factory, grant, ledger) = factory();
    let source = resource(&grant, &url);
    let mut document = convert(&factory, &source, &HttpCancel::default()).unwrap();
    document.refresh_after = Duration::ZERO;
    let session = StreamSession::new(
        Id(72),
        Generation::default(),
        "127.0.0.1:45124".parse().unwrap(),
        factory,
        None,
    )
    .unwrap();
    session
        .commit(
            session
                .prepare_manifest(Id(0), document.format, &document.bytes)
                .unwrap(),
        )
        .unwrap();
    let root_path = url::Url::parse(&session.root_url().unwrap())
        .unwrap()
        .path()
        .to_owned();
    let master_text = {
        let access = session.open(&root_path, &HttpCancel::default()).unwrap();
        String::from_utf8(
            access
                .reader
                .read_at(0, 65536, &access.cancel)
                .unwrap()
                .blob
                .bytes
                .clone(),
        )
        .unwrap()
    };
    let audio_url = master_text
        .lines()
        .find_map(|l| l.split("URI=\"").nth(1)?.split('"').next())
        .unwrap()
        .to_owned();
    let video_url = master_text
        .lines()
        .find(|l| l.starts_with("http:"))
        .unwrap()
        .to_owned();
    let audio_path = url::Url::parse(&audio_url).unwrap().path().to_owned();
    let video_path = url::Url::parse(&video_url).unwrap().path().to_owned();

    live::attach(&session, source, &document).unwrap();

    // Thread A triggers the refresh on the video playlist and blocks at the gated server.
    let sess_a = Arc::clone(&session);
    let vp = video_path.clone();
    let handle_a = std::thread::spawn(move || {
        let access = sess_a.open(&vp, &HttpCancel::default()).unwrap();
        String::from_utf8(
            access
                .reader
                .read_at(0, 65536, &access.cancel)
                .unwrap()
                .blob
                .bytes
                .clone(),
        )
        .unwrap()
    });
    refresh_started_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();

    // Thread B concurrently opens the audio playlist: it must WAIT for refresh (not skip it!).
    let sess_b = Arc::clone(&session);
    let ap = audio_path.clone();
    let handle_b = std::thread::spawn(move || {
        let access = sess_b.open(&ap, &HttpCancel::default()).unwrap();
        String::from_utf8(
            access
                .reader
                .read_at(0, 65536, &access.cancel)
                .unwrap()
                .blob
                .bytes
                .clone(),
        )
        .unwrap()
    });

    // Thread C also waits concurrently, then gets cancelled — it must wake immediately!
    let sess_c = Arc::clone(&session);
    let cancel_c = HttpCancel::default();
    let cancel_c_worker = cancel_c.clone();
    let ap_c = audio_path.clone();
    let handle_c = std::thread::spawn(move || sess_c.open(&ap_c, &cancel_c_worker).err());
    std::thread::sleep(Duration::from_millis(40));
    let cancel_start = Instant::now();
    cancel_c.cancel();
    assert_eq!(handle_c.join().unwrap(), Some(ErrorCode::Cancelled));
    assert!(
        cancel_start.elapsed() < Duration::from_millis(500),
        "cancelled waiter took {:?}",
        cancel_start.elapsed()
    );

    // Release the single in-flight refresh; both video and audio openers must see sequence 2.
    {
        let (lock, cv) = &*release;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    let video_body = handle_a.join().unwrap();
    let audio_body = handle_b.join().unwrap();
    assert!(
        video_body.contains("#EXT-X-MEDIA-SEQUENCE:2\n"),
        "{video_body}"
    );
    assert!(
        audio_body.contains("#EXT-X-MEDIA-SEQUENCE:2\n"),
        "{audio_body}"
    );
    assert_eq!(master_hits_rx.recv().unwrap(), 1);
    assert_eq!(master_hits_rx.recv().unwrap(), 2);
    assert!(master_hits_rx.try_recv().is_err());

    server_thread.join().unwrap();
    drop((document, session, grant));
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::PlaybackSession), 0);
}
