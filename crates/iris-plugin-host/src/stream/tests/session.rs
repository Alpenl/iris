use super::*;
use crate::{
    ledger::Resource,
    stream::{manifest::ROOT, reader::MemoryReader},
};
use iris_api::v2::{ErrorCode, stream::ManifestFormat};
use iris_net::http_client::HttpCancel;
use serde_json::json;
fn manifest(url: &RemoteResource) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "resources":[{"id":"media","kind":"media","extension":"ts","size":"4","source":{"transport":"remote","request":url}}],
        "tracks":[{
            "id":"video",
            "kind":"video",
            "label":"Video",
            "lang":null,
            "codec":null,
            "default":true,
            "sequence":"1",
            "segments":[{"resource":{"resource_id":"media","range":null},"duration_ms":"1000","init":null,"key":null}]
        }],
        "variants":[],
        "live":true
    }))
    .unwrap()
}
fn child(session: &StreamSession) -> String {
    let cancel = HttpCancel::default();
    let root = session
        .open(&path(&session.root_url().unwrap()), &cancel)
        .unwrap();
    let read = root.reader.read_at(0, 65536, &root.cancel).unwrap();
    String::from_utf8(read.blob.bytes.clone())
        .unwrap()
        .lines()
        .find(|l| l.starts_with("http:"))
        .unwrap()
        .into()
}
#[test]
fn refresh_preserves_unchanged_routes_and_revokes_rebound_or_removed_resources() {
    let (factory, grant, ledger) = factory();
    let session = session(factory, "127.0.0.1:45001".parse().unwrap());
    let resource = resource(&grant, "http://127.0.0.1:46001/a");
    let input = manifest(&resource);
    session
        .commit(
            session
                .prepare_manifest(Id(0), ManifestFormat::VirtualHls, &input)
                .unwrap(),
        )
        .unwrap();
    let root = session.root_url().unwrap();
    let old_child = child(&session);
    let prepared = session
        .prepare_manifest(Id(1), ManifestFormat::VirtualHls, &input)
        .unwrap();
    let racing = session
        .prepare_manifest(Id(1), ManifestFormat::VirtualHls, &input)
        .unwrap();
    session.commit(prepared).unwrap();
    assert_eq!(session.commit(racing).unwrap_err(), ErrorCode::StaleHandle);
    assert_eq!(session.root_url().unwrap(), root);
    assert_eq!(child(&session), old_child);
    let old_access = session
        .open(&path(&old_child), &HttpCancel::default())
        .unwrap();
    let mut updated = resource.clone();
    updated.url = "http://127.0.0.1:46001/b".into();
    let input = manifest(&updated);
    session
        .commit(
            session
                .prepare_manifest(Id(2), ManifestFormat::VirtualHls, &input)
                .unwrap(),
        )
        .unwrap();
    assert_ne!(child(&session), old_child);
    assert!(old_access.cancel.is_cancelled());
    assert!(
        session
            .open(&path(&old_child), &HttpCancel::default())
            .is_err()
    );
    assert!(
        session
            .prepare_manifest(Id(3), ManifestFormat::VirtualHls, b"<MPD/>")
            .is_err()
    );
    assert_eq!(session.root_url().unwrap(), root);
    assert_eq!(session.revision(), Id(3));
    drop(old_access);
    session.cancel();
    assert_eq!(ledger.used(None, Resource::StreamResource), 0);
    assert!(session.open(&path(&root), &HttpCancel::default()).is_err());
    drop(session);
    drop(grant);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(ledger.used(None, Resource::PlaybackSession), 0);
}
#[test]
fn cross_session_prepare_and_token_reuse_are_denied_and_ranges_are_repeatable() {
    let (factory, grant, ledger) = factory();
    let a = session(factory.clone(), "127.0.0.1:45002".parse().unwrap());
    let b = session(factory, "127.0.0.1:45003".parse().unwrap());
    let reader = Arc::new(MemoryReader::new(b"abcd".to_vec(), "stream-tests", &ledger).unwrap());
    let prepared = a.prepare_reader(reader.clone()).unwrap();
    assert_eq!(b.commit(prepared).unwrap_err(), ErrorCode::Denied);
    a.commit(a.prepare_reader(reader).unwrap()).unwrap();
    let root = path(&a.root_url().unwrap());
    assert!(b.open(&root, &HttpCancel::default()).is_err());
    let cross = root.replacen(&format!("/v2/{}/", a.id.0), &format!("/v2/{}/", b.id.0), 1);
    assert!(b.open(&cross, &HttpCancel::default()).is_err());
    for _ in 0..3 {
        let access = a.open(&root, &HttpCancel::default()).unwrap();
        let read = access.reader.read_at(1, 2, &access.cancel).unwrap();
        assert_eq!(read.blob.bytes, b"bc");
        assert!(!read.eof);
    }
    assert!(root.len() < 256);
    assert!(!root.contains(ROOT));
    drop(a);
    drop(b);
    drop(grant);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}

#[test]
fn advance_seek_advances_generation_without_cancelling_direct_streams() {
    let (factory, grant, ledger) = factory();
    let session = session(factory, "127.0.0.1:45004".parse().unwrap());
    let reader =
        Arc::new(MemoryReader::new(b"0123456789".to_vec(), "stream-tests", &ledger).unwrap());
    session
        .commit(session.prepare_reader(reader).unwrap())
        .unwrap();
    let root = path(&session.root_url().unwrap());
    let access = session.open(&root, &HttpCancel::default()).unwrap();
    let generation = session.advance_seek().unwrap();
    assert_eq!(generation.seek.0, 1);
    assert!(
        !access.cancel.is_cancelled(),
        "host-managed direct stream should stay alive across seeks until mpv closes its socket"
    );
    assert_eq!(
        access
            .reader
            .read_at(2, 4, &access.cancel)
            .unwrap()
            .blob
            .bytes,
        b"2345"
    );
    session.cancel();
    assert!(access.cancel.is_cancelled());
    drop(access);
    drop(session);
    drop(grant);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}
