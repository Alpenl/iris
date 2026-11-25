use super::*;
use serde_json::{Value, json};
fn binding(id: &str, kind: &str, extension: &str) -> Value {
    json!({"id":id,"kind":kind,"extension":extension,"size":"4096","source":{"transport":"guest","resource_id":id}})
}
fn local(id: &str, ext: ResourceExtension) -> Result<String, ErrorCode> {
    let suffix = serde_json::to_value(ext).unwrap();
    Ok(format!(
        "http://127.0.0.1:50123/session-token/{id}.{}",
        suffix.as_str().unwrap()
    ))
}
fn compile_value(format: ManifestFormat, value: &Value) -> Result<Manifest, ErrorCode> {
    compile(format, &serde_json::to_vec(value).unwrap(), &local)
}
fn hls_value() -> Value {
    json!({"resources":[binding("segment", "media", "m4s"),binding("init", "init", "mp4")], "tracks":[{
        "id":"video", "kind":"video", "label":"Video", "lang":null, "codec":"avc1.640028", "sequence":"9007199254740993", "default":true,
        "segments":[{"resource":{"resource_id":"segment","range":{"offset":"5","len":"10"}},"duration_ms":"1500","init":{"resource_id":"init","range":null},"key":null}]
    }], "variants":[], "live":false})
}
#[test]
fn hls_generates_sealed_init_range_sequence_and_live_master_audio() {
    let mut value = hls_value();
    let manifest = compile_value(ManifestFormat::VirtualHls, &value).unwrap();
    let text = std::str::from_utf8(&manifest.documents[0].bytes).unwrap();
    assert!(text.contains("MEDIA-SEQUENCE:9007199254740993\n"));
    assert!(text.contains("#EXT-X-BYTERANGE:10@5\n"));
    assert!(text.contains("#EXT-X-MAP:URI=\"http://127.0.0.1:50123/session-token/init.mp4\""));
    assert!(text.ends_with("#EXT-X-ENDLIST\n"));
    let mut audio = value["tracks"][0].clone();
    audio["id"] = json!("audio");
    audio["kind"] = json!("audio");
    audio["codec"] = json!("mp4a.40.2");
    value["tracks"].as_array_mut().unwrap().push(audio);
    value["variants"] =
        json!([{"video":"video","audio":"audio","bandwidth":"8000000","width":3840,"height":2160}]);
    value["live"] = json!(true);
    let manifest = compile_value(ManifestFormat::VirtualHls, &value).unwrap();
    assert_eq!(manifest.documents.len(), 3);
    assert!(manifest.documents.iter().all(|d| {
        !std::str::from_utf8(&d.bytes)
            .unwrap()
            .contains("#EXT-X-ENDLIST")
    }));
    let master = std::str::from_utf8(&manifest.documents.last().unwrap().bytes).unwrap();
    assert!(master.contains("TYPE=AUDIO,GROUP-ID=\"audio\""));
    assert!(master.contains("CODECS=\"avc1.640028,mp4a.40.2\""));
    assert!(master.contains("RESOLUTION=3840x2160"));
}
#[test]
fn hls_rejects_type_confusion_overflow_injection_and_unbounded_resources() {
    for mutation in 0..8 {
        let mut value = hls_value();
        match mutation {
            0 => {
                value["tracks"][0]["segments"][0]["resource"]["resource_id"] =
                    json!("file:///secret")
            }
            1 => value["tracks"][0]["label"] = json!("bad\"\n#EXT-X-KEY:URI=\"http://evil"),
            2 => value["resources"][0]["kind"] = json!("key"),
            3 => {
                value["tracks"][0]["segments"][0]["resource"]["range"]["offset"] =
                    json!("18446744073709551615")
            }
            4 => value["tracks"][0]["segments"][0]["resource"]["range"]["len"] = json!("4096"),
            5 => value["resources"][1]["id"] = json!("segment"),
            6 => value["resources"][0]["source"]["resource_id"] = json!("%2e%2e%2fsecret"),
            _ => value["tracks"][0]["sequence"] = json!("18446744073709551615"),
        }
        assert!(
            compile_value(ManifestFormat::VirtualHls, &value).is_err(),
            "mutation {mutation}"
        );
    }
    let mut value = hls_value();
    let segment = value["tracks"][0]["segments"][0].clone();
    value["tracks"][0]["segments"] = json!(vec![segment; 10001]);
    assert!(compile_value(ManifestFormat::VirtualHls, &value).is_err());
}
#[test]
fn restricted_hls_rebuilds_ast_and_rejects_all_indirection() {
    let mut value = json!({"resources":[binding("segment", "media", "ts")], "text":"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:1.500,example\n#EXT-X-BYTERANGE:20@0\nsegment\n#EXTINF:1.000,\n#EXT-X-BYTERANGE:20\nsegment\n#EXT-X-ENDLIST\n"});
    let manifest = compile_value(ManifestFormat::RestrictedHls, &value).unwrap();
    let text = std::str::from_utf8(&manifest.documents[0].bytes).unwrap();
    assert!(text.contains("#EXT-X-BYTERANGE:20@20"));
    assert!(!text.contains("example"));
    for reference in [
        "http://evil/seg",
        "file:///tmp/seg",
        "//evil/seg",
        "../seg",
        "%2e%2e%2fseg",
        "segment?x=1",
        "segment\\x",
        "segment\tx",
    ] {
        value["text"] = json!(format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:1,\n{reference}\n#EXT-X-ENDLIST\n"
        ));
        assert!(
            compile_value(ManifestFormat::RestrictedHls, &value).is_err(),
            "{reference}"
        );
    }
    for tag in [
        "#EXT-X-DEFINE:NAME=\"x\",VALUE=\"http://evil\"",
        "#EXT-X-UNKNOWN:URI=\"segment\"",
        "#EXT-X-KEY:METHOD=AES-128,URI=\"segment\",URI=\"file:///x\"",
        "#EXT-X-MAP:URI=\"http://evil\"",
        "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"segment\"",
    ] {
        value["text"] = json!(format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n{tag}\n#EXTINF:1,\nsegment\n#EXT-X-ENDLIST\n"
        ));
        assert!(
            compile_value(ManifestFormat::RestrictedHls, &value).is_err(),
            "{tag}"
        );
    }
}
fn dash_value() -> Value {
    json!({"duration_ms":"2000", "resources":[binding("s1", "media", "m4s"),binding("s2", "media", "m4s"),binding("init", "init", "mp4")],"tracks":[{
        "id":"v", "kind":"video", "codec":"avc1.640028", "mime_type":"video/mp4", "bandwidth":"8000000", "timescale":1000,
        "width":3840,"height":2160,"sample_rate":null,"lang":null,"init":{"resource_id":"init","range":{"offset":"0","len":"32"}},
        "segments":{"addressing":"segment_template","resource_template":"s$Number$","start_number":"1","timeline":[{"time":"0","duration":"1000","repeat":1}]}
    }]})
}
#[test]
fn dash_expands_finite_templates_and_generates_only_session_urls() {
    let value = dash_value();
    let result = compile_value(ManifestFormat::VirtualDash, &value).unwrap();
    let text = std::str::from_utf8(&result.documents[0].bytes).unwrap();
    let doc = roxmltree::Document::parse(text).unwrap();
    let urls: Vec<_> = doc
        .descendants()
        .filter(|n| n.has_tag_name("SegmentURL"))
        .map(|n| n.attribute("media").unwrap())
        .collect();
    assert_eq!(urls.len(), 2);
    assert!(urls[0].ends_with("/s1.m4s"));
    assert!(urls[1].ends_with("/s2.m4s"));
    assert!(text.contains("<S t=\"1000\" d=\"1000\"/>"));
    assert!(!text.contains('$'));
    for bad in [
        "../../$Number$",
        "https://evil/$Time$",
        "s$RepresentationID$",
        "s$Number$$Time$",
        "s%2f$Number$",
    ] {
        let mut value = value.clone();
        value["tracks"][0]["segments"]["resource_template"] = json!(bad);
        assert!(compile_value(ManifestFormat::VirtualDash, &value).is_err());
    }
    let mut value = value.clone();
    value["tracks"][0]["segments"]["timeline"][0]["repeat"] = json!(u32::MAX);
    assert!(compile_value(ManifestFormat::VirtualDash, &value).is_err());
}
#[test]
fn dash_segment_base_checks_index_and_rejects_xml_passthrough() {
    let mut value = dash_value();
    value["tracks"][0]["segments"] = json!({"addressing":"segment_base","media":{"resource_id":"s1","range":null},"index":{"offset":"32","len":"64"}});
    let manifest = compile_value(ManifestFormat::VirtualDash, &value).unwrap();
    let text = std::str::from_utf8(&manifest.documents[0].bytes).unwrap();
    assert!(text.contains("indexRange=\"32-95\""));
    assert!(text.contains("range=\"0-31\""));
    value["tracks"][0]["segments"]["index"]["len"] = json!("4096");
    assert!(compile_value(ManifestFormat::VirtualDash, &value).is_err());
    assert!(
        compile(
            ManifestFormat::VirtualDash,
            b"<MPD><BaseURL>file:///etc</BaseURL></MPD>",
            &local
        )
        .is_err()
    );
}
