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
