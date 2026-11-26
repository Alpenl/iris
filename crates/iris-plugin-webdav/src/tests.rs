use super::*;
use iris_plugin_sdk::api::v2::{Role, task::CredentialBucket};
use serde_json::json;
use std::collections::VecDeque;

mod regressions;

#[derive(Default)]
struct Mock {
    next: u64,
    tasks: VecDeque<(Id, Task)>,
    closed: Vec<Id>,
    blobs: BTreeMap<Id, Vec<u8>>,
}
impl Mock {
    fn pop(&mut self) -> (Id, Task) {
        self.tasks.pop_front().expect("expected asynchronous task")
    }
}
impl Host for Mock {
    fn submit(&mut self, task: &Task) -> Result<Id, ErrorCode> {
        self.next += 1;
        let id = Id(self.next);
        self.tasks.push_back((id, task.clone()));
        Ok(id)
    }
    fn blob(&mut self, bytes: &[u8]) -> Result<Id, ErrorCode> {
        self.next += 1;
        let id = Id(self.next);
        self.blobs.insert(id, bytes.to_vec());
        Ok(id)
    }
    fn take_blob(&mut self, id: Id) -> Result<Vec<u8>, ErrorCode> {
        self.blobs.remove(&id).ok_or(ErrorCode::StaleHandle)
    }
    fn close(&mut self, id: Id) {
        self.closed.push(id);
        self.blobs.remove(&id);
    }
    fn random(&mut self, bytes: &mut [u8]) -> Result<(), ErrorCode> {
        bytes.fill(42);
        Ok(())
    }
}
fn scoped(window: u64, body: HostEvent<Value>) -> HostEvent<Value> {
    HostEvent::Window {
        window: Id(window),
        generation: Generation::default(),
        body: Box::new(body),
    }
}
fn done(id: Id, result: Value) -> HostEvent<Value> {
    HostEvent::Completed {
        operation_id: id,
        generation: Generation::default(),
        result,
    }
}
fn fixture(endpoint: &str) -> (WebDav, Mock) {
    let mut plugin = WebDav::default();
    let mut host = Mock::default();
    plugin
        .event(
            HostEvent::Init {
                protocol: 2,
                abi: 2,
                host_version: "0.0.1".into(),
                role: Role::Control,
                generation: Generation::default(),
                capabilities: Default::default(),
                features: Default::default(),
                snapshot: None,
            },
            &mut host,
        )
        .unwrap();
    let (id, task) = host.pop();
    assert!(matches!(task, Task::StateRead { key } if key == "servers"));
    let servers = json!([{"id":"nas", "name":"NAS", "endpoint":endpoint,"auth":{"kind":"basic","username":"test"},"allow_insecure_auth":true}]).to_string();
    let commands = plugin
        .event(
            done(id, json!({"ok":{"store_value":servers,"revision":"1"}})),
            &mut host,
        )
        .unwrap();
    assert!(matches!(&commands[0], HostCommand::SetSources { sources } if sources[0].id == "nas"));
    for window in [1, 2] {
        plugin
            .event(
                scoped(window, HostEvent::WindowOpened { snapshot: None }),
                &mut host,
            )
            .unwrap();
    }
    (plugin, host)
}
fn browse(request: u64) -> HostEvent<Value> {
    HostEvent::SourceBrowse {
        request_id: Id(request),
        source_id: "nas".into(),
        folder_id: None,
        page: None,
        query: None,
        filters: BTreeMap::new(),
    }
}
fn authorize(plugin: &mut WebDav, host: &mut Mock, window: u64, origin: &str) -> Id {
    let (id, task) = host.pop();
    assert!(matches!(task, Task::OriginLookup { .. }));
    plugin
        .event(
            scoped(
                window,
                done(
                    id,
                    json!({"ok":{"origin":origin,"origin_grant":"991","insecure_auth":true}}),
                ),
            ),
            host,
        )
        .unwrap();
    let (id, task) = host.pop();
    assert!(
        matches!(task, Task::SecretRead { bucket: CredentialBucket::Plugin, key } if key == "secret.nas")
    );
    plugin
        .event(
            scoped(
                window,
                done(id, json!({"ok":{"store_value":"password","revision":null}})),
            ),
            host,
        )
        .unwrap();
    let (id, task) = host.pop();
    assert!(
        matches!(task, Task::HttpStart { origin_grant: Some(Id(991)), follow_redirects:false, headers, .. } if headers.iter().any(|(k,_)| k == "Authorization"))
    );
    id
}
#[test]
fn private_origin_and_plaintext_approval_precede_secrets_and_cancellation_is_scoped() {
    let (mut plugin, mut host) = fixture("http://192.168.1.50:5244/dav/");
    plugin.event(scoped(1, browse(100)), &mut host).unwrap();
    let (id, _) = host.pop();
    let commands = plugin.event(scoped(1, done(id, json!({"ok":{"origin":"http://192.168.1.50:5244", "origin_grant":"991", "insecure_auth":false}}))), &mut host).unwrap();
    assert!(matches!(
        &commands[0],
        HostCommand::RequestOrigin {
            request_id: Id(100),
            ..
        }
    ));
    assert!(
        host.tasks.is_empty(),
        "legacy checkbox must not authorize reading or sending secrets"
    );
    let commands = plugin
        .event(
            scoped(
                2,
                HostEvent::OriginGranted {
                    request_id: Id(100),
                    origin: "http://192.168.1.50:5244".into(),
                    grant: Some(Id(991)),
                    insecure_auth: true,
                },
            ),
            &mut host,
        )
        .unwrap();
    assert!(commands.is_empty());
    assert!(
        host.tasks.is_empty(),
        "another window cannot finish the consent"
    );
    plugin
        .event(
            scoped(
                1,
                HostEvent::RequestCancelled {
                    request_id: Id(100),
                },
            ),
            &mut host,
        )
        .unwrap();
    plugin
        .event(
            scoped(
                1,
                HostEvent::OriginGranted {
                    request_id: Id(100),
                    origin: "http://192.168.1.50:5244".into(),
                    grant: Some(Id(991)),
                    insecure_auth: true,
                },
            ),
            &mut host,
        )
        .unwrap();
    assert!(
        host.tasks.is_empty(),
        "a late approval cannot restart cancelled I/O"
    );
    plugin.event(scoped(1, browse(101)), &mut host).unwrap();
    let one = authorize(&mut plugin, &mut host, 1, "http://192.168.1.50:5244");
    plugin.event(scoped(2, browse(102)), &mut host).unwrap();
    let two = authorize(&mut plugin, &mut host, 2, "http://192.168.1.50:5244");
    let full = plugin.event(scoped(2, browse(103)), &mut host).unwrap();
    assert!(matches!(full.as_slice(), [HostCommand::SourceError { .. }]));
    plugin
        .event(
            scoped(
                1,
                HostEvent::RequestCancelled {
                    request_id: Id(101),
                },
            ),
            &mut host,
        )
        .unwrap();
    assert!(host.closed.contains(&one));
    assert!(!host.closed.contains(&two));
    assert_eq!(plugin.windows[&Id(2)].jobs.len(), 1);
}
#[test]
fn bounded_http_reads_produce_pages_and_cached_playback_without_extra_propfind() {
    let (mut plugin, mut host) = fixture("https://nas.example/dav/");
    plugin.event(scoped(1, browse(100)), &mut host).unwrap();
    let operation = authorize(&mut plugin, &mut host, 1, "https://nas.example");
    plugin.event(scoped(1, done(operation, json!({"ok":{"state":"response","stream_handle":"990","status":207,"url":"https://nas.example/dav/","headers":[],"written":null}}))), &mut host).unwrap();
    let (operation, task) = host.pop();
    assert!(matches!(
        task,
        Task::HttpRead {
            handle: Id(990),
            ..
        }
    ));
    let mut xml = String::from("<d:multistatus xmlns:d='DAV:'>");
    for index in 0..200 {
        xml.push_str(&format!("<d:response><d:href>/dav/ep{index}.mp4</d:href><d:propstat><d:status>HTTP/1.1 200 OK</d:status><d:prop><d:getcontentlength>123</d:getcontentlength></d:prop></d:propstat></d:response>"));
    }
    xml.push_str(
        "<d:response><d:href>/dav/ep2.zh.srt</d:href><d:prop/></d:response></d:multistatus>",
    );
    host.blobs.insert(Id(989), xml.into_bytes());
    let first = plugin
        .event(
            scoped(
                1,
                done(
                    operation,
                    json!({"ok":{"eof":true,"text":null,"blob":"989"}}),
                ),
            ),
            &mut host,
        )
        .unwrap();
    let HostCommand::ProvideSourcePage { page, .. } = &first[0] else {
        panic!("expected page")
    };
    assert_eq!(page.entries[2].name, "ep2.mp4");
    assert_eq!(page.entries.len(), 128);
    assert_eq!(page.folder_id, None);
    assert!(host.closed.contains(&Id(990)));
    let next = plugin
        .event(
            scoped(
                1,
                HostEvent::SourceBrowse {
                    request_id: Id(101),
                    source_id: "nas".into(),
                    folder_id: page.folder_id.clone(),
                    page: page.next_page.clone(),
                    query: None,
                    filters: Default::default(),
                },
            ),
            &mut host,
        )
        .unwrap();
    assert!(
        matches!(&next[0], HostCommand::ProvideSourcePage { page, .. } if page.entries.len() == 72 && page.next_page.is_none() && page.folder_id.is_none())
    );
    plugin
        .event(
            scoped(
                1,
                HostEvent::SourceResolve {
                    request_id: Id(102),
                    source_id: "nas".into(),
                    folder_id: Some("/".into()),
                    entry_id: "/ep2.mp4".into(),
                    variant: None,
                },
            ),
            &mut host,
        )
        .unwrap();
    let (id, _) = host.pop();
    plugin.event(scoped(1, done(id, json!({"ok":{"origin":"https://nas.example", "origin_grant":"991", "insecure_auth":false}}))), &mut host).unwrap();
    let (id, _) = host.pop();
    let resolved = plugin
        .event(
            scoped(
                1,
                done(id, json!({"ok":{"store_value":"password","revision":null}})),
            ),
            &mut host,
        )
        .unwrap();
    assert!(
        matches!(&resolved[0], HostCommand::ResolveStream { media, .. } if media.stream.entry_id.as_deref() == Some("/ep2.mp4") && media.stream.subtitles.len() == 1 && media.playlist.iter().any(|i| i.entry_id == "/ep2.mp4"))
    );
    assert!(host.tasks.is_empty());
    assert!(serde_json::to_vec(&resolved).unwrap().len() < 64 * 1024);
}
#[test]
fn credentials_are_versioned_before_config_commit_and_uncertain_writes_reload() {
    let (mut plugin, mut host) = fixture("https://nas.example/dav/");
    let fields = BTreeMap::from([
        ("name".into(), "Renamed".into()),
        ("endpoint".into(), "https://nas.example/dav/".into()),
        ("auth".into(), "basic".into()),
        ("username".into(), "test".into()),
        ("secret".into(), "new-password".into()),
    ]);
    for window in [1, 2] {
        plugin
            .event(
                scoped(
                    window,
                    HostEvent::SourceManage {
                        source_id: "nas".into(),
                        action: iris_plugin_sdk::api::SourceManageAction::Configure,
                    },
                ),
                &mut host,
            )
            .unwrap();
    }
    plugin
        .event(
            scoped(
                1,
                HostEvent::ModalSubmit {
                    id: "webdav.edit".into(),
                    values: fields.clone(),
                },
            ),
            &mut host,
        )
        .unwrap();
    let (id, task) = host.pop();
    assert!(
        matches!(task, Task::SecretWrite { key, value:Some(v), .. } if key.starts_with("cred.") && v == "new-password")
    );
    assert_eq!(plugin.servers[0].name, "NAS");
    let busy = plugin
        .event(
            scoped(
                2,
                HostEvent::ModalSubmit {
                    id: "webdav.edit".into(),
                    values: fields,
                },
            ),
            &mut host,
        )
        .unwrap();
    assert!(matches!(&busy[0], HostCommand::ShowToast { .. }));
    assert!(host.tasks.is_empty());
    plugin
        .event(
            scoped(1, done(id, json!({"ok":{"stored":true,"revision":null}}))),
            &mut host,
        )
        .unwrap();
    let (write, task) = host.pop();
    let Task::StateWrite {
        value: Some(saved), ..
    } = task
    else {
        panic!("configuration must follow successful secret write")
    };
    assert!(saved.contains("cred."));
    assert!(!saved.contains("new-password"));
    // A window can disappear after the atomic filesystem commit but before receipt.
    plugin
        .event(HostEvent::WindowClosed { window: Id(1) }, &mut host)
        .unwrap();
    assert!(!plugin.loaded);
    plugin
        .event(
            scoped(
                2,
                HostEvent::Command {
                    id: "add_server".into(),
                    snapshot: None,
                },
            ),
            &mut host,
        )
        .unwrap();
    let (id, task) = host.pop();
    assert!(matches!(task, Task::StateRead { .. }));
    plugin
        .event(
            scoped(
                2,
                done(id, json!({"ok":{"store_value":saved,"revision":"2"}})),
            ),
            &mut host,
        )
        .unwrap();
    assert_eq!(plugin.servers[0].name, "Renamed");
    let late = plugin
        .event(
            scoped(2, done(write, json!({"ok":{"stored":true,"revision":"2"}}))),
            &mut host,
        )
        .unwrap();
    assert!(late.is_empty());
    assert!(
        host.tasks.is_empty(),
        "unconfirmed old credential must not be deleted"
    );
}
#[test]
fn xml_and_modal_reject_scope_escape_and_dtd() {
    let (plugin, _) = fixture("https://nas.example/dav/");
    let conn = &plugin.servers[0];
    let (url, root) = conn.root_segments().unwrap();
    let xml = "<d:multistatus xmlns:d='DAV:'><d:response><d:href>https://evil.example/dav/steal.mp4</d:href><d:prop/></d:response><d:response><d:href>/dav/a%2fb.mp4</d:href><d:prop/></d:response><d:response><d:href>/other/hidden.mp4</d:href><d:prop/></d:response><d:response><d:href>/dav/ok.mp4</d:href><d:prop/></d:response></d:multistatus>";
    let listing = protocol::parse_propfind_xml(xml, &url, &root, "/").unwrap();
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].path, "/ok.mp4");
    assert!(
        protocol::parse_propfind_xml(
            "<!DOCTYPE a [<!ENTITY x SYSTEM 'file:///secret'>]><a>&x;</a>",
            &url,
            &root,
            "/"
        )
        .is_err()
    );
    let values = BTreeMap::from([
        ("name".into(), "NAS".into()),
        ("endpoint".into(), "https://nas.example/dav/".into()),
        ("auth".into(), "basic".into()),
        ("username".into(), "different".into()),
    ]);
    assert!(
        modal::validate_modal_submit(&plugin.servers, "webdav.edit", Some("nas"), &values).is_err()
    );
    assert!(config::decode_servers(Some("not JSON")).is_err());
}
