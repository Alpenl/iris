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
        iris_plugin_sdk::json::decode_exact(&converted.bytes, manifest::INPUT_LIMIT).unwrap();
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
fn remote_dash_expands_number_time_and_finite_negative_repeat_without_exposing_baseurl() {
    let xml = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S" minBufferTime="PT1S"><BaseURL>video/</BaseURL><Period><AdaptationSet mimeType="video/mp4" codecs="avc1.640028"><SegmentTemplate timescale="1000" initialization="init-$RepresentationID$.mp4" media="s-$Number%05d$-$Time$.m4s" startNumber="5"><SegmentTimeline><S t="0" d="2000" r="-1"/></SegmentTimeline></SegmentTemplate><Representation id="v1" bandwidth="8000000" width="3840" height="2160"/></AdaptationSet><AdaptationSet mimeType="audio/mp4" codecs="mp4a.40.2" lang="en"><Representation id="a" bandwidth="128000"><BaseURL>audio.mp4</BaseURL><SegmentBase timescale="1000" indexRange="32-95"><Initialization range="0-31"/></SegmentBase></Representation></AdaptationSet></Period></MPD>"#;
    let (factory, grant, _) = factory();
    let (url, rx, thread) = server(vec![response(xml)]);
    let converted = convert(&factory, &resource(&grant, &url), &HttpCancel::default()).unwrap();
    let parsed: VirtualDash =
        iris_plugin_sdk::json::decode_exact(&converted.bytes, manifest::INPUT_LIMIT).unwrap();
    assert_eq!(parsed.tracks.len(), 2);
    assert!(parsed.resources.iter().any(|r|matches!(&r.source,ResourceSource::Remote {request} if request.url.ends_with("/video/s-00005-0.m4s"))));
    assert!(parsed.resources.iter().any(|r|matches!(&r.source,ResourceSource::Remote {request} if request.url.ends_with("/video/s-00006-2000.m4s"))));
    let output = render(&converted).unwrap();
    let text = std::str::from_utf8(&output.documents[0].bytes).unwrap();
    let xml = roxmltree::Document::parse(text).unwrap();
    for node in xml.descendants().filter(roxmltree::Node::is_element) {
        if node.has_tag_name("BaseURL") {
            assert!(
                node.text()
                    .unwrap()
                    .starts_with("http://127.0.0.1:50123/token/")
            );
        }
        for attr in node
            .attributes()
            .filter(|a| matches!(a.name(), "media" | "sourceURL"))
        {
            assert!(attr.value().starts_with("http://127.0.0.1:50123/token/"));
        }
    }
    assert!(!text.contains('$'));
    assert!(text.contains("indexRange=\"32-95\""));
    assert!(roxmltree::Document::parse(text).is_ok());
    rx.recv().unwrap();
    thread.join().unwrap();
}
#[test]
fn remote_parsers_reject_private_origin_escape_external_xml_and_unknown_semantics() {
    let bodies = [
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nhttp://127.0.0.1:9/internal\n#EXT-X-ENDLIST\n",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-CONTENT-STEERING:SERVER-URI=\"file:///secret\"\n#EXTINF:1,\nsegment.ts\n",
        "<!DOCTYPE MPD [<!ENTITY external SYSTEM 'file:///secret'>]><MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\">&external;</MPD>",
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"dynamic\" mediaPresentationDuration=\"PT1S\"><Period/></MPD>",
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" mediaPresentationDuration=\"PT1S\"><BaseURL>file:///secret</BaseURL><Period/></MPD>",
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" mediaPresentationDuration=\"PT1S\"><Period/><Period/></MPD>",
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
fn encoder_metadata_is_inert_but_links_service_extensions_and_fractional_precision_fail_closed() {
    let xml = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="urn:mpeg:dash:schema:mpd:2011 file:///must-not-read" type="static" mediaPresentationDuration="PT1S"><ProgramInformation><Title>Local fixture</Title></ProgramInformation><ServiceDescription id="0"/><Period><AdaptationSet mimeType="audio/mp4" codecs="mp4a.40.2" bitstreamSwitching="true"><Representation id="a" bandwidth="128000"><AudioChannelConfiguration schemeIdUri="urn:mpeg:dash:23003:3:audio_channel_configuration:2011" value="2"/><BaseURL>audio.mp4</BaseURL><SegmentBase timescale="1000" indexRange="32-95"><Initialization range="0-31"/></SegmentBase></Representation></AdaptationSet></Period></MPD>"#;
    let hls = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.000000,\nsegment.ts\n#EXT-X-ENDLIST\n";
    let cases = [(xml.to_string(), true), (hls.into(),true),
        (xml.replace("<ServiceDescription id=\"0\"/>","<ServiceDescription><Latency target=\"10\"/></ServiceDescription>"),false),
        (xml.replace("<Title>","<Title xmlns:xlink=\"http://www.w3.org/1999/xlink\" xlink:href=\"file:///secret\">"),false),
        (xml.replace("bitstreamSwitching=\"true\"","bitstreamSwitching=\"invalid\""),false),
        (xml.replace("value=\"2\"","value=\"256\""),false),
        (hls.replace("1.000000","0.999999"),false)];
    for (body, accepted) in cases {
        let (factory, grant, _) = factory();
        let (url, rx, thread) = server(vec![response(&body)]);
        let result = convert(&factory, &resource(&grant, &url), &HttpCancel::default());
        assert_eq!(result.is_ok(), accepted);
        if let Ok(converted) = result {
            let output = render(&converted).unwrap();
            for doc in output.documents {
                assert!(
                    !String::from_utf8(doc.bytes)
                        .unwrap()
                        .contains("must-not-read")
                );
            }
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
