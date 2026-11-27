use super::*;
use crate::plugin_v2::pool::IoPool;
use iris_api::v2::{Capability, NetworkDeclaration};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::TcpStream,
    sync::mpsc,
    time::Instant,
};

fn context(ledger: &Ledger) -> TaskContext {
    TaskContext {
        instance_id: Id(20),
        window: None,
        owner: "auth-tests".into(),
        role: Role::Control,
        generation: Generation::default(),
        cancel: HttpCancel::default(),
        ledger: ledger.clone(),
    }
}
fn setup() -> (AuthExecutor, NetworkGrant, Ledger, mpsc::Receiver<String>) {
    let ledger = Ledger::default();
    let network = NetworkGrant::new(
        "auth-tests".into(),
        BTreeSet::from([Capability::NetHttp]),
        NetworkDeclaration {
            net_public: true,
            ..Default::default()
        },
        &ledger,
    )
    .unwrap();
    let (tx, rx) = mpsc::sync_channel(4);
    let auth = AuthExecutor::new(
        network.clone(),
        ledger.clone(),
        Arc::new(move |_, _, url| {
            tx.try_send(url.to_owned())
                .map_err(|_| ErrorCode::Backpressure)
        }),
    );
    (auth, network, ledger, rx)
}
fn start(pool: &IoPool, operation: Operation) -> mpsc::Receiver<Result<IoOutcome, ErrorCode>> {
    let (tx, rx) = mpsc::channel();
    operation
        .dispatch(
            pool,
            Role::Control,
            "auth-tests",
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        )
        .unwrap();
    rx
}
fn begin(auth: &AuthExecutor, pool: &IoPool, ledger: &Ledger) -> serde_json::Value {
    start(
        pool,
        auth.prepare(
            Task::BeginAuth {
                callback_path: "/oauth/callback".into(),
                issuer: Some("https://issuer.example".into()),
            },
            &BTreeMap::new(),
            context(ledger),
        )
        .unwrap(),
    )
    .recv_timeout(Duration::from_secs(2))
    .unwrap()
    .unwrap()
    .value
}
fn open_task(value: &serde_json::Value) -> Task {
    let mut url = Url::parse("https://login.example/authorize").unwrap();
    url.query_pairs_mut()
        .append_pair("state", value["state"].as_str().unwrap())
        .append_pair("redirect_uri", value["redirect_uri"].as_str().unwrap())
        .append_pair("code_challenge", value["code_challenge"].as_str().unwrap())
        .append_pair("code_challenge_method", "S256")
        .append_pair("response_type", "code")
        .append_pair("client_id", "iris");
    Task::OpenAuth {
        transaction: serde_json::from_value(value["transaction"].clone()).unwrap(),
        url: url.to_string(),
        origin_grant: None,
    }
}
fn callback(value: &serde_json::Value, query: &str) -> String {
    let redirect = Url::parse(value["redirect_uri"].as_str().unwrap()).unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        stream,
        "GET {}?{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        redirect.path(),
        query,
        redirect.port().unwrap()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}
fn released(ledger: &Ledger) {
    let end = Instant::now() + Duration::from_secs(2);
    while ledger.used(None, Resource::Auth) > 0 {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn oauth_binds_state_pkce_issuer_and_consumes_only_one_valid_callback() {
    let (auth, _network, ledger, browser) = setup();
    let pool = IoPool::new().unwrap();
    let value = begin(&auth, &pool, &ledger);
    assert!(!auth.is_quiescent());
    let pending = start(
        &pool,
        auth.prepare(open_task(&value), &BTreeMap::new(), context(&ledger))
            .unwrap(),
    );
    assert!(
        browser
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .starts_with("https://login.example/")
    );
    assert!(
        callback(
            &value,
            "state=wrong&code=secret&iss=https%3A%2F%2Fissuer.example"
        )
        .starts_with("HTTP/1.1 400")
    );
    let state = value["state"].as_str().unwrap();
    assert!(
        callback(&value, &format!("state={state}&state={state}&code=secret"))
            .starts_with("HTTP/1.1 400")
    );
    assert!(
        callback(
            &value,
            &format!("state={state}&code=secret&iss=https%3A%2F%2Fwrong.example")
        )
        .starts_with("HTTP/1.1 400")
    );
    assert!(pending.try_recv().is_err());
    let response = callback(
        &value,
        &format!("state={state}&code=secret&iss=https%3A%2F%2Fissuer.example"),
    );
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(!response.contains("secret"));
    assert!(!response.contains(state));
    let result = pending
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap();
    assert_eq!(result.value["code"], "secret");
    let verifier = result.value["code_verifier"].as_str().unwrap();
    assert_eq!(
        URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes())),
        value["code_challenge"]
    );
    released(&ledger);
    let deadline = Instant::now() + Duration::from_secs(1);
    while !auth.is_quiescent() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(matches!(
        auth.prepare(open_task(&value), &BTreeMap::new(), context(&ledger)),
        Err(ErrorCode::StaleHandle)
    ));
}

#[test]
fn auth_rejects_redirect_mutation_cross_instance_and_releases_on_revocation() {
    let (auth, network, ledger, browser) = setup();
    let pool = IoPool::new().unwrap();
    let value = begin(&auth, &pool, &ledger);
    let mut altered = open_task(&value);
    if let Task::OpenAuth { url, .. } = &mut altered {
        *url = url.replace(
            "redirect_uri=",
            "redirect_uri=https%3A%2F%2Fevil.example%2F",
        );
    }
    assert!(matches!(
        auth.prepare(altered, &BTreeMap::new(), context(&ledger)),
        Err(ErrorCode::InvalidArgument)
    ));
    let mut other = context(&ledger);
    other.instance_id = Id(21);
    assert!(matches!(
        auth.prepare(open_task(&value), &BTreeMap::new(), other),
        Err(ErrorCode::Denied)
    ));
    let pending = start(
        &pool,
        auth.prepare(open_task(&value), &BTreeMap::new(), context(&ledger))
            .unwrap(),
    );
    browser.recv_timeout(Duration::from_secs(1)).unwrap();
    network.revoke();
    assert!(
        pending
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_err()
    );
    released(&ledger);
}

#[test]
fn expired_auth_is_not_opened_and_query_rejects_ambiguous_encoding() {
    for raw in [
        "https://a/?x=1&x=2",
        "https://a/?x=%GG",
        "https://a/?x=%ff",
        "https://a/?x=%",
    ] {
        assert!(super::callback::query(&Url::parse(raw).unwrap()).is_err());
    }
    let (mut auth, _network, ledger, _browser) = setup();
    Arc::get_mut(&mut auth.0).unwrap().ttl = Duration::from_millis(200);
    let pool = IoPool::new().unwrap();
    let value = begin(&auth, &pool, &ledger);
    let pending = start(
        &pool,
        auth.prepare(open_task(&value), &BTreeMap::new(), context(&ledger))
            .unwrap(),
    );
    assert!(matches!(
        pending.recv_timeout(Duration::from_secs(2)).unwrap(),
        Err(ErrorCode::Timeout)
    ));
    released(&ledger);
    assert!(matches!(
        auth.prepare(open_task(&value), &BTreeMap::new(), context(&ledger)),
        Err(ErrorCode::StaleHandle)
    ));
}
