use super::super::tests::{factory, resource, server};
use super::*;
use crate::plugin_v2::ledger::Resource;
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
        crate::plugin_v2::json::decode_exact(&converted.bytes, manifest::INPUT_LIMIT).unwrap();
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
fn live_refresh_keeps_root_and_attachments_but_revokes_evicted_segments() {
    use crate::plugin_v2::stream::{reader::MemoryReader, session::StreamSession};
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
