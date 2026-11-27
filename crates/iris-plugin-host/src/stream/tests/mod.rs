mod bench;
mod relay;
mod remote;
mod session;
use super::{
    remote::RemoteFactory,
    session::{StreamSession, new_session_id},
};
use crate::ledger::Ledger;
use crate::network::{NetworkGrant, TransportKind};
use iris_api::v2::{Capability, Generation, Id, NetworkDeclaration, stream::RemoteResource};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

pub(super) fn factory() -> (RemoteFactory, NetworkGrant, Ledger) {
    let ledger = Ledger::default();
    let grant = NetworkGrant::new(
        "stream-tests".into(),
        BTreeSet::from([Capability::NetHttp]),
        NetworkDeclaration::default(),
        &ledger,
    )
    .unwrap();
    let factory = RemoteFactory::new(grant.clone(), ledger.clone()).unwrap();
    (factory, grant, ledger)
}
fn session(factory: RemoteFactory, authority: SocketAddr) -> Arc<StreamSession> {
    StreamSession::new(
        new_session_id().unwrap(),
        Generation::default(),
        authority,
        factory,
        None,
    )
    .unwrap()
}
pub(super) fn resource(grant: &NetworkGrant, url: &str) -> RemoteResource {
    let origin = grant
        .confirm_origin(url, TransportKind::Http, &["127.0.0.1".parse().unwrap()])
        .unwrap();
    RemoteResource {
        url: url.into(),
        headers: Vec::new(),
        origin_grant: Some(origin),
    }
}
fn headers(socket: &mut TcpStream) -> String {
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 65536);
    }
    String::from_utf8(bytes).unwrap()
}
pub(super) fn server(
    responses: Vec<Vec<u8>>,
) -> (String, mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/media", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        for response in responses {
            let end = Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                if let Ok((socket, _)) = listener.accept() {
                    socket.set_nonblocking(false).unwrap();
                    break socket;
                }
                assert!(Instant::now() < end, "fixture request never arrived");
                std::thread::sleep(Duration::from_millis(3));
            };
            tx.send(headers(&mut socket)).unwrap();
            socket.write_all(&response).unwrap();
        }
    });
    (url, rx, thread)
}
fn until(mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < end, "condition timed out");
        std::thread::sleep(Duration::from_millis(3));
    }
}
fn path(url: &str) -> String {
    url::Url::parse(url).unwrap().path().to_owned()
}
fn request(url: &str, method: &str, extra: &str) -> Vec<u8> {
    let url = url::Url::parse(url).unwrap();
    let authority = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    let mut socket = TcpStream::connect(&authority).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        socket,
        "{method} {} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n{extra}\r\n",
        url.path()
    )
    .unwrap();
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes).unwrap();
    bytes
}
