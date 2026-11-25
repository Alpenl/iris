mod artifacts;
mod native;
use super::*;
use crate::{
    plugin_v2::{
        ledger::{Activity, Ledger, Resource},
        network::NetworkGrant,
    },
    vfs::HttpCancel,
};
use iris_api::v2::{
    Capability, NetworkDeclaration,
    task::{MediaContainer, SampleRate, Task},
};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

#[test]
fn typed_arguments_and_output_parsers_cannot_expand_operations() {
    assert!(crate::plugin_v2::json::decode_exact::<Task>(br#"{"operation":"invoke_tool","task":{"kind":"resolve_metadata","url":"https://example.com","origin_grant":null,"extra_args":"--exec whoami"}}"#, 65536).is_err());
    assert!(
        arguments::whisper(
            std::path::Path::new("audio.wav"),
            std::path::Path::new("model.bin"),
            Some("en --exec bad")
        )
        .is_err()
    );
    let args = arguments::audio("http://127.0.0.1:45001/token", 2, SampleRate::Hz16000).unwrap();
    assert!(args.iter().any(|a| a == "0:a:2"));
    assert_eq!(args.last().unwrap(), "pipe:1");
    assert!(arguments::slice("url", 2000, 1000, MediaContainer::Mp4).is_err());
    assert!(
        arguments::metadata("https://example.com/--exec", "http://127.0.0.1:1")
            .windows(2)
            .any(|a| a[0] == "--" && a[1] == "https://example.com/--exec")
    );
    let result =
        output::transcription(b"\n[00:00:01.000 --> 00:00:02.500]  <script>&hello\n").unwrap();
    assert!(
        String::from_utf8(result)
            .unwrap()
            .contains("&lt;script&gt;&amp;hello")
    );
    assert!(output::transcription(b"[00:99:00.000 --> 00:00:01.000] nope").is_err());
    let mut extent = output::Extent::default();
    extent.push(b"packet|pts_time=-0.020000|duration_time=0.040000|flags=K_\npacket|pts_time=1.000000|duration_time=0.021333|flags=K_\n").unwrap();
    assert_eq!(extent.finish().unwrap(), (-20, 1021));
}
#[test]
fn metadata_proxy_requires_task_credentials_and_denies_private_connect() {
    let ledger = Ledger::default();
    let activity = Activity::default();
    let network = NetworkGrant::new(
        "proxy-test".into(),
        BTreeSet::from([Capability::NetHttp]),
        NetworkDeclaration {
            net_public: true,
            ..Default::default()
        },
        &ledger,
    )
    .unwrap();
    let proxy = proxy::Proxy::start(
        network,
        ledger.clone(),
        &HttpCancel::default(),
        activity.clone(),
        "https://example.com/",
    )
    .unwrap();
    let url = url::Url::parse(&proxy.url).unwrap();
    let address = (url.host_str().unwrap(), url.port().unwrap());
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .write_all(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1:443\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 407"));
    use base64::{Engine, engine::general_purpose::STANDARD};
    let auth = STANDARD.encode(format!("{}:{}", url.username(), url.password().unwrap()));
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(socket, "CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1:443\r\nProxy-Authorization: Basic {auth}\r\n\r\n").unwrap();
    let mut response = String::new();
    let _ = socket.read_to_string(&mut response);
    assert!(!response.contains("200 Connection Established"));
    drop(proxy);
    let end = Instant::now() + Duration::from_secs(3);
    while !activity.is_idle() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(ledger.used(None, Resource::Http), 0);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}
