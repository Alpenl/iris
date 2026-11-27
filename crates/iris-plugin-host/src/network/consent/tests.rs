use super::*;
use crate::url_resolver::normalize_url;
use iris_api::v2::{Capability, NetworkDeclaration};

impl OriginStore {
    pub(crate) fn revoke(
        &self,
        grant: &NetworkGrant,
        origin: &str,
        cancel: &HttpCancel,
    ) -> Result<(), ErrorCode> {
        self.check(grant, cancel)?;
        let origin = parse_consent_origin_url(origin)?.0.display();
        let _lock = fs_util::lock_file_with_timeout(
            &self.path.with_extension("lock"),
            Duration::from_millis(250),
        )
        .map_err(|_| ErrorCode::Backpressure)?;
        let mut record = self.read()?;
        record.entries.remove(&origin);
        self.check(grant, cancel)?;
        let encoded = bounded_json::encode(&record, MAX_RECORD).map_err(json_error)?;
        fs_util::atomic_write_private(&self.path, &encoded).map_err(|_| ErrorCode::IoError)?;
        if let Ok(kind) = consent_transport(grant, &origin)
            && let Ok(found) = grant.lookup_origin(&origin, kind)
        {
            grant.revoke_origin(found.origin_grant)?;
        }
        Ok(())
    }
}

struct Fixture {
    root: PathBuf,
    store: OriginStore,
    grant: NetworkGrant,
    ledger: Ledger,
}
impl Fixture {
    fn new() -> Self {
        let owner = format!("{:032x}", crate::instance::next_id().unwrap());
        let root = std::env::temp_dir().join(format!(
            "iris-origin-consent-{}-{owner}",
            std::process::id()
        ));
        let ledger = Ledger::default();
        let store = OriginStore::new(root.clone(), owner.clone(), ledger.clone()).unwrap();
        let grant = NetworkGrant::new(
            owner,
            BTreeSet::from([Capability::NetHttp]),
            NetworkDeclaration::default(),
            &ledger,
        )
        .unwrap();
        Self {
            root,
            store,
            grant,
            ledger,
        }
    }
    fn reload(&self) -> NetworkGrant {
        let grant = NetworkGrant::new(
            self.store.owner.clone(),
            BTreeSet::from([Capability::NetHttp]),
            NetworkDeclaration::default(),
            &self.ledger,
        )
        .unwrap();
        self.store.restore(&grant, &HttpCancel::default()).unwrap();
        grant
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert!(self.root.starts_with(std::env::temp_dir()));
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
#[test]
fn review_does_not_authorize_and_persisted_pins_remain_exact() {
    let f = Fixture::new();
    let cancel = HttpCancel::default();
    let raw = "http://192.168.1.50:5244/dav/movies";
    let review = f.store.review(&f.grant, raw, &cancel).unwrap();
    assert!(review.private() && review.plaintext());
    assert_eq!(review.origin(), "http://192.168.1.50:5244");
    assert!(f.grant.lookup_origin(raw, TransportKind::Http).is_err());
    assert!(!f.store.path.exists());
    let approved = f.store.approve(&f.grant, review, false, &cancel).unwrap();
    let scoped = f
        .grant
        .request(raw, TransportKind::Http, Some(approved.origin_grant))
        .unwrap();
    let url = normalize_url(raw, TransportKind::Http).unwrap();
    assert!(
        scoped
            .check_address(&url, "192.168.1.50:5244".parse().unwrap())
            .is_ok()
    );
    assert!(
        scoped
            .check_address(&url, "192.168.1.51:5244".parse().unwrap())
            .is_err()
    );
    assert!(
        scoped
            .check_credentials(&url, &[("Authorization".into(), "Basic xxx".into())])
            .is_err()
    );
    assert!(f.grant.request(raw, TransportKind::Http, None).is_err());
    let reloaded = f.reload();
    let restored = reloaded.lookup_origin(raw, TransportKind::Http).unwrap();
    assert_ne!(
        restored.origin_grant, approved.origin_grant,
        "ephemeral handles are not persisted"
    );
    assert!(!restored.insecure_auth);
    f.store.revoke(&f.grant, raw, &cancel).unwrap();
    assert!(scoped.cancel().is_cancelled());
    assert!(f.reload().lookup_origin(raw, TransportKind::Http).is_err());
}
#[test]
fn concurrent_reviews_cannot_replace_newer_pins_and_plaintext_requires_confirmation() {
    let f = Fixture::new();
    let cancel = HttpCancel::default();
    let raw = "http://127.0.0.1:5244/";
    let first = f.store.review(&f.grant, raw, &cancel).unwrap();
    let stale = f.store.review(&f.grant, raw, &cancel).unwrap();
    let approved = f.store.approve(&f.grant, first, false, &cancel).unwrap();
    assert!(matches!(
        f.store.approve(&f.grant, stale, true, &cancel),
        Err(ErrorCode::StaleHandle)
    ));
    let prior = f
        .grant
        .request(raw, TransportKind::Http, Some(approved.origin_grant))
        .unwrap();
    let next = f.store.review(&f.grant, raw, &cancel).unwrap();
    assert!(next.previous().is_some());
    let approved = f.store.approve(&f.grant, next, true, &cancel).unwrap();
    assert!(
        prior.cancel().is_cancelled(),
        "reconfirmation retires all previous handles for this Origin"
    );
    let request = f
        .grant
        .request(raw, TransportKind::Http, Some(approved.origin_grant))
        .unwrap();
    assert!(
        request
            .check_credentials(
                &normalize_url(raw, TransportKind::Http).unwrap(),
                &[("Authorization".into(), "Basic xxx".into())]
            )
            .is_ok()
    );
    assert!(
        f.reload()
            .lookup_origin(raw, TransportKind::Http)
            .unwrap()
            .insecure_auth
    );
}
#[test]
fn corrupt_records_and_forbidden_targets_never_become_empty_authority() {
    let f = Fixture::new();
    let cancel = HttpCancel::default();
    for raw in [
        "http://169.254.169.254/",
        "http://198.18.0.1/",
        "http://[fd00:ec2::254]/",
    ] {
        assert!(f.store.review(&f.grant, raw, &cancel).is_err());
    }
    let review = f
        .store
        .review(&f.grant, "https://192.168.1.50/", &cancel)
        .unwrap();
    assert!(matches!(
        f.store.approve(&f.grant, review, true, &cancel),
        Err(ErrorCode::InvalidArgument)
    ));
    std::fs::create_dir_all(f.store.path.parent().unwrap()).unwrap();
    std::fs::write(&f.store.path, b"broken record").unwrap();
    assert!(f.store.restore(&f.grant, &cancel).is_err());
    assert!(
        f.store
            .review(&f.grant, "http://192.168.1.50/", &cancel)
            .is_err()
    );
    assert_eq!(std::fs::read(&f.store.path).unwrap(), b"broken record");
}

#[test]
fn parse_consent_origin_url_supports_http_and_websocket_schemes() {
    let (http_origin, http_kind) =
        parse_consent_origin_url("http://192.168.1.50:8080/dav").unwrap();
    assert_eq!(http_origin.display(), "http://192.168.1.50:8080");
    assert_eq!(http_kind, TransportKind::Http);

    let (ws_origin, ws_kind) = parse_consent_origin_url("ws://192.168.1.50:8080/socket").unwrap();
    assert_eq!(ws_origin.display(), "http://192.168.1.50:8080");
    assert_eq!(ws_kind, TransportKind::WebSocket);

    let (wss_origin, wss_kind) = parse_consent_origin_url("wss://example.com/live").unwrap();
    assert_eq!(wss_origin.display(), "https://example.com:443");
    assert_eq!(wss_kind, TransportKind::WebSocket);

    let (tcp_origin, tcp_kind) = parse_consent_origin_url("tcp://192.168.1.50:9000").unwrap();
    assert_eq!(tcp_origin.display(), "http://192.168.1.50:9000");
    assert_eq!(tcp_kind, TransportKind::Tcp { tls: false });

    let (tls_origin, tls_kind) = parse_consent_origin_url("tls://example.com:9443").unwrap();
    assert_eq!(tls_origin.display(), "https://example.com:9443");
    assert_eq!(tls_kind, TransportKind::Tcp { tls: true });

    for bad in [
        "ftp://example.com/",
        "file:///etc/passwd",
        "ws://user:pass@192.168.1.50:8080/",
        "tcp://192.168.1.50",
        "tls://example.com",
        "not a url",
    ] {
        assert!(parse_consent_origin_url(bad).is_err(), "{bad}");
    }
}

#[test]
fn origin_store_persists_and_restores_ws_only_and_tcp_only_grants() {
    let cancel = HttpCancel::default();
    for (cap, raw, check_kind, url_str) in [
        (
            Capability::NetWs,
            "ws://192.168.1.60:8080/chat",
            TransportKind::WebSocket,
            "ws://192.168.1.60:8080/chat",
        ),
        (
            Capability::NetTcp,
            "http://192.168.1.61:9000/",
            TransportKind::Tcp { tls: false },
            "http://192.168.1.61:9000/",
        ),
        (
            Capability::NetTcp,
            "https://192.168.1.62:9443/",
            TransportKind::Tcp { tls: true },
            "https://192.168.1.62:9443/",
        ),
    ] {
        let owner = format!("{:032x}", crate::instance::next_id().unwrap());
        struct DirGuard(PathBuf);
        impl Drop for DirGuard {
            fn drop(&mut self) {
                assert!(self.0.starts_with(std::env::temp_dir()));
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = DirGuard(std::env::temp_dir().join(format!(
            "iris-origin-consent-cap-{}-{owner}",
            std::process::id()
        )));
        let ledger = Ledger::default();
        let store = OriginStore::new(root.0.clone(), owner.clone(), ledger.clone()).unwrap();
        let make_grant = || {
            NetworkGrant::new(
                owner.clone(),
                BTreeSet::from([cap]),
                NetworkDeclaration::default(),
                &ledger,
            )
            .unwrap()
        };
        let grant = make_grant();
        let review = store.review(&grant, raw, &cancel).unwrap();
        let approved = store.approve(&grant, review, false, &cancel).unwrap();
        assert_eq!(
            grant
                .lookup_origin(url_str, check_kind)
                .unwrap()
                .origin_grant,
            approved.origin_grant
        );

        let reloaded = make_grant();
        store.restore(&reloaded, &cancel).unwrap();
        let restored = reloaded.lookup_origin(url_str, check_kind).unwrap();
        assert_ne!(restored.origin_grant, approved.origin_grant);

        store.revoke(&grant, raw, &cancel).unwrap();
        assert!(grant.lookup_origin(url_str, check_kind).is_err());

        drop((reloaded, grant, store));
    }
}

#[test]
fn mixed_net_http_and_net_tcp_plugin_shares_persisted_origin_grants_across_transports() {
    let cancel = HttpCancel::default();
    let owner = format!("{:032x}", crate::instance::next_id().unwrap());
    let root = std::env::temp_dir().join(format!(
        "iris-origin-consent-mixed-{}-{owner}",
        std::process::id()
    ));
    let ledger = Ledger::default();
    let store = OriginStore::new(root.clone(), owner.clone(), ledger.clone()).unwrap();
    let make_grant = || {
        NetworkGrant::new(
            owner.clone(),
            BTreeSet::from([Capability::NetHttp, Capability::NetTcp]),
            NetworkDeclaration::default(),
            &ledger,
        )
        .unwrap()
    };

    let grant = make_grant();
    let raw = "http://192.168.1.77:9000/";
    let review = store.review(&grant, raw, &cancel).unwrap();
    let approved = store.approve(&grant, review, false, &cancel).unwrap();

    for kind in [TransportKind::Http, TransportKind::Tcp { tls: false }] {
        assert_eq!(
            grant.lookup_origin(raw, kind).unwrap().origin_grant,
            approved.origin_grant
        );
        assert!(
            grant
                .request(raw, kind, Some(approved.origin_grant))
                .is_ok()
        );
    }
    assert!(
        grant
            .request(
                "https://192.168.1.77:9000/",
                TransportKind::Tcp { tls: true },
                Some(approved.origin_grant),
            )
            .is_err()
    );

    let reloaded = make_grant();
    store.restore(&reloaded, &cancel).unwrap();
    let restored = reloaded
        .lookup_origin(raw, TransportKind::Tcp { tls: false })
        .unwrap();
    assert!(
        reloaded
            .request(
                raw,
                TransportKind::Tcp { tls: false },
                Some(restored.origin_grant),
            )
            .is_ok()
    );

    store.revoke(&grant, raw, &cancel).unwrap();
    assert!(
        grant
            .lookup_origin(raw, TransportKind::Tcp { tls: false })
            .is_err()
    );
    drop((reloaded, grant, store));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn review_and_approve_reject_grants_without_network_capabilities_before_dns_or_disk_io() {
    let cancel = HttpCancel::default();
    let owner = format!("{:032x}", crate::instance::next_id().unwrap());
    let root = std::env::temp_dir().join(format!(
        "iris-origin-consent-nocap-{}-{owner}",
        std::process::id()
    ));
    let ledger = Ledger::default();
    let store = OriginStore::new(root.clone(), owner.clone(), ledger.clone()).unwrap();
    let no_net_grant = NetworkGrant::new(
        owner.clone(),
        BTreeSet::new(),
        NetworkDeclaration::default(),
        &ledger,
    )
    .unwrap();

    // `review` fails with Denied before spawning DNS resolution.
    assert_eq!(
        store
            .review(&no_net_grant, "http://unresolvable.invalid:8080/", &cancel)
            .err(),
        Some(ErrorCode::Denied)
    );
    assert!(!store.path.exists());

    // `approve` fails with Denied before writing `origins/<owner>.json`.
    let synthetic_review = OriginReview {
        owner: owner.clone(),
        version: no_net_grant.version(),
        origin: "http://192.168.1.99:8080".into(),
        ips: vec!["192.168.1.99".parse().unwrap()],
        previous: None,
        expires: Deadline::after(Duration::from_secs(60)),
        _memory: ledger
            .reserve(&owner, &[(Resource::HostBytes, 1024)])
            .unwrap(),
    };
    assert_eq!(
        store
            .approve(&no_net_grant, synthetic_review, false, &cancel)
            .err(),
        Some(ErrorCode::Denied)
    );
    assert!(!store.path.exists());
    let _ = std::fs::remove_dir_all(&root);
}
