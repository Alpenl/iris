use super::*;
use iris_api::v2::{Capability, NetworkDeclaration};

struct Fixture {
    root: PathBuf,
    store: OriginStore,
    grant: NetworkGrant,
    ledger: Ledger,
}
impl Fixture {
    fn new() -> Self {
        let owner = format!("{:032x}", crate::plugin_v2::instance::next_id().unwrap());
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
