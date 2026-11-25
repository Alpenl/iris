use super::super::*;
use crate::plugin_v2::ledger::Ledger;
use iris_api::v2::{Capability, HostRule, NetworkDeclaration};
use std::{collections::BTreeSet, net::IpAddr};

fn public() -> NetworkGrant {
    NetworkGrant::new(
        "network-tests".into(),
        BTreeSet::from([Capability::NetHttp, Capability::NetWs, Capability::NetTcp]),
        NetworkDeclaration {
            net_public: true,
            ..NetworkDeclaration::default()
        },
        &Ledger::default(),
    )
    .unwrap()
}
#[test]
fn public_rejects_special_addresses_and_requires_port_and_transport() {
    let grant = public();
    let request = grant
        .request("https://cdn.example/video", TransportKind::Http, None)
        .unwrap();
    let url = normalize_url("https://cdn.example/video", TransportKind::Http).unwrap();
    for ip in [
        "127.0.0.1",
        "10.0.0.1",
        "169.254.169.254",
        "100.64.1.2",
        "192.0.2.1",
        "198.18.0.1",
        "198.51.100.5",
        "203.0.113.2",
        "224.1.2.3",
        "240.0.0.1",
        "::1",
        "::ffff:127.0.0.1",
        "fe80::1",
        "64:ff9b::c0a8:1",
        "2001:db8::1",
        "2002::1",
        "3fff::1",
        "fd00:ec2::254",
    ] {
        assert!(
            request
                .check_address(&url, (ip.parse::<IpAddr>().unwrap(), 443).into())
                .is_err(),
            "{ip}"
        );
    }
    for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
        assert!(
            request
                .check_address(&url, (ip.parse::<IpAddr>().unwrap(), 443).into())
                .is_ok(),
            "{ip}"
        );
    }
    assert!(
        grant
            .request("https://cdn.example:8443/", TransportKind::Http, None)
            .is_err()
    );
    assert!(
        grant
            .request(
                "https://cdn.example/",
                TransportKind::Tcp { tls: true },
                None
            )
            .is_err()
    );
    let http_only = NetworkGrant::new(
        "http-only".into(),
        BTreeSet::from([Capability::NetHttp]),
        NetworkDeclaration {
            net_public: true,
            ..NetworkDeclaration::default()
        },
        &Ledger::default(),
    )
    .unwrap();
    assert!(
        http_only
            .request("wss://cdn.example/", TransportKind::WebSocket, None)
            .is_err()
    );
}

#[test]
fn private_authority_is_exact_pinned_and_cannot_be_borrowed_by_public_redirect() {
    let grant = public();
    let private = "http://nas.example:5244/dav";
    let id = grant
        .confirm_origin(
            private,
            TransportKind::Http,
            &["192.168.1.50".parse().unwrap()],
        )
        .unwrap();
    let request = grant
        .request(private, TransportKind::Http, Some(id))
        .unwrap();
    let url = normalize_url(private, TransportKind::Http).unwrap();
    assert!(
        request
            .check_address(&url, "192.168.1.50:5244".parse().unwrap())
            .is_ok()
    );
    assert!(
        request
            .check_address(&url, "192.168.1.51:5244".parse().unwrap())
            .is_err()
    );
    assert!(
        request
            .check_address(&url, "192.168.1.50:5245".parse().unwrap())
            .is_err()
    );
    let public = grant
        .request("http://cdn.example/", TransportKind::Http, None)
        .unwrap();
    assert!(
        public
            .check_address(&url, "192.168.1.50:5244".parse().unwrap())
            .is_err()
    );
    assert!(
        grant
            .request("http://cdn.example/", TransportKind::Http, Some(id))
            .is_err()
    );
    assert!(
        grant
            .confirm_origin(
                "http://169.254.169.254/",
                TransportKind::Http,
                &["169.254.169.254".parse().unwrap()]
            )
            .is_err()
    );
    assert!(!request.cancel().is_cancelled());
    grant.revoke_origin(id).unwrap();
    assert!(request.cancel().is_cancelled());
    assert!(
        request
            .check_address(&url, "192.168.1.50:5244".parse().unwrap())
            .is_err()
    );
}

#[test]
fn host_rules_normalize_idna_and_keep_dns_label_boundaries() {
    let grant = NetworkGrant::new(
        "hosts".into(),
        BTreeSet::from([Capability::NetHttp]),
        NetworkDeclaration {
            net_hosts: vec![HostRule {
                host: "*.BÜCHER.example.".into(),
                schemes: vec!["https".into()],
                ports: vec![443],
            }],
            ..NetworkDeclaration::default()
        },
        &Ledger::default(),
    )
    .unwrap();
    assert!(
        grant
            .request(
                "https://a.xn--bcher-kva.example./file",
                TransportKind::Http,
                None
            )
            .is_ok()
    );
    for url in [
        "https://xn--bcher-kva.example/",
        "https://evil-xn--bcher-kva.example/",
        "https://a.xn--bcher-kva.example.evil/",
        "http://a.xn--bcher-kva.example/",
        "https://a.xn--bcher-kva.example:444/",
    ] {
        assert!(
            grant.request(url, TransportKind::Http, None).is_err(),
            "{url}"
        );
    }
}

#[test]
fn unsafe_urls_downgrades_and_revocations_fail_closed() {
    let grant = public();
    for raw in [
        "https://user:pass@example.com/",
        " https://example.com/",
        "https://example.com/\r\nX:x",
        "file:///tmp/data",
        "https://example.com\\@evil.test/",
        "http://127.0.0.1/",
        "http://[::ffff:127.0.0.1]/",
    ] {
        assert!(
            grant.request(raw, TransportKind::Http, None).is_err(),
            "{raw}"
        );
    }
    let request = grant
        .request("https://example.com/", TransportKind::Http, None)
        .unwrap();
    assert!(
        request
            .check_redirect(
                &url::Url::parse("https://example.com/").unwrap(),
                &url::Url::parse("http://example.com/").unwrap()
            )
            .is_err()
    );
    grant.revoke();
    assert!(request.cancel().is_cancelled());
    assert!(request.check_live().is_err());
}
