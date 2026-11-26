use super::*;
use iris_plugin_sdk::api::{SourceManageAction, is_valid_id};

#[test]
fn browse_pages_echo_implicit_explicit_and_normalized_folder_identities() {
    for folder in [None, Some("/"), Some("/folder/")] {
        let (mut plugin, mut host) = fixture("https://nas.example/dav/");
        let path = protocol::normalize_dav_path(folder.unwrap_or("/"));
        let prefix = if path == "/" {
            String::new()
        } else {
            path.clone()
        };
        plugin
            .event(
                scoped(
                    1,
                    HostEvent::SourceBrowse {
                        request_id: Id(100),
                        source_id: "nas".into(),
                        folder_id: folder.map(str::to_owned),
                        page: None,
                        query: None,
                        filters: BTreeMap::new(),
                    },
                ),
                &mut host,
            )
            .unwrap();
        let operation = authorize(&mut plugin, &mut host, 1, "https://nas.example");
        let url = format!("https://nas.example/dav{prefix}/");
        plugin.event(scoped(1, done(operation, json!({"ok":{
            "state":"response", "stream_handle":"990", "status":207, "url":url, "headers":[], "written":null
        }}))), &mut host).unwrap();
        let (operation, _) = host.pop();
        let mut xml = String::from("<d:multistatus xmlns:d='DAV:'>");
        for i in 0..130 {
            xml.push_str(&format!(
                "<d:response><d:href>/dav{prefix}/ep{i}.mp4</d:href><d:prop/></d:response>"
            ));
        }
        xml.push_str("</d:multistatus>");
        host.blobs.insert(Id(989), xml.into_bytes());
        let commands = plugin
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
        let [HostCommand::ProvideSourcePage { page, .. }] = commands.as_slice() else {
            panic!("expected first page")
        };
        assert_eq!(page.folder_id.as_deref(), folder);
        assert_eq!(page.entries.len(), 128);
        let commands = plugin
            .event(
                scoped(
                    1,
                    HostEvent::SourceBrowse {
                        request_id: Id(101),
                        source_id: "nas".into(),
                        folder_id: page.folder_id.clone(),
                        page: page.next_page.clone(),
                        query: None,
                        filters: BTreeMap::new(),
                    },
                ),
                &mut host,
            )
            .unwrap();
        let [HostCommand::ProvideSourcePage { page, .. }] = commands.as_slice() else {
            panic!("expected continuation")
        };
        assert_eq!(page.folder_id.as_deref(), folder);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.next_page, None);
        assert!(
            host.tasks.is_empty(),
            "continuation must use the cached listing"
        );
    }
}

#[test]
fn configure_uses_a_valid_modal_id_and_submits_to_the_same_connection() {
    let (mut plugin, mut host) = fixture("https://nas.example/dav/");
    let commands = plugin
        .event(
            scoped(
                1,
                HostEvent::SourceManage {
                    source_id: "nas".into(),
                    action: SourceManageAction::Configure,
                },
            ),
            &mut host,
        )
        .unwrap();
    let [HostCommand::OpenModal { modal }] = commands.as_slice() else {
        panic!("expected edit modal")
    };
    assert_eq!(modal.id, modal::EDIT_MODAL_ID);
    assert!(is_valid_id(&modal.id));
    let values = BTreeMap::from([
        ("name".into(), "Renamed NAS".into()),
        ("endpoint".into(), "https://nas.example/dav/".into()),
        ("auth".into(), "basic".into()),
        ("username".into(), "test".into()),
    ]);
    let validated = modal::validate_modal_submit(
        &plugin.servers,
        &modal.id,
        plugin.windows[&Id(1)].editing.as_deref(),
        &values,
    )
    .unwrap();
    assert_eq!(validated.conn.id, "nas");
    assert_eq!(validated.conn.name, "Renamed NAS");
    assert_eq!(
        validated.secret, None,
        "an unchanged credential must be retained"
    );
}

#[test]
fn max_length_64_byte_connection_id_opens_valid_modal_and_derives_valid_secret_key() {
    let (mut plugin, mut host) = fixture("https://nas.example/dav/");
    let id_64 = "a".repeat(64);
    assert!(is_valid_id(&id_64));
    plugin.servers[0].id = id_64.clone();
    plugin.servers[0].credential_key = None;

    let fallback_key = config::secret_key(&plugin.servers[0]);
    assert!(
        is_valid_id(&fallback_key),
        "fallback secret key `{fallback_key}` must stay within 64-byte is_valid_id"
    );

    let commands = plugin
        .event(
            scoped(
                1,
                HostEvent::SourceManage {
                    source_id: id_64.clone(),
                    action: SourceManageAction::Configure,
                },
            ),
            &mut host,
        )
        .unwrap();
    let [HostCommand::OpenModal { modal }] = commands.as_slice() else {
        panic!("expected configure modal")
    };
    assert_eq!(modal.id, modal::EDIT_MODAL_ID);
    assert!(is_valid_id(&modal.id));

    let submit_cmds = plugin
        .event(
            scoped(
                1,
                HostEvent::ModalSubmit {
                    id: modal.id.clone(),
                    values: BTreeMap::from([
                        ("name".into(), "64-Byte NAS".into()),
                        ("endpoint".into(), "https://nas.example/dav/".into()),
                        ("auth".into(), "basic".into()),
                        ("username".into(), "test".into()),
                        ("secret".into(), String::new()),
                    ]),
                },
            ),
            &mut host,
        )
        .unwrap();
    assert!(
        submit_cmds.is_empty(),
        "valid 64-byte ID edit should submit StateWrite, got {submit_cmds:?}"
    );
    let (write_op, task) = host.pop();
    let Task::StateWrite {
        value: Some(raw), ..
    } = task
    else {
        panic!("expected StateWrite for unchanged secret")
    };
    let decoded =
        config::decode_servers(Some(&raw)).expect("saved servers must pass decode_servers");
    assert_eq!(decoded[0].id, id_64);
    assert_eq!(
        decoded[0].credential_key.as_deref(),
        Some(fallback_key.as_str())
    );
    let finish = plugin
        .event(
            scoped(
                1,
                done(write_op, json!({"ok":{"stored":true,"revision":"2"}})),
            ),
            &mut host,
        )
        .unwrap();
    assert!(finish.iter().any(|c| matches!(
        c,
        HostCommand::CloseModal { id: Some(closed) } if closed == modal::EDIT_MODAL_ID
    )));
    assert_eq!(plugin.windows[&Id(1)].editing, None);
}

#[test]
fn long_names_and_duplicate_name_suffixes_allow_credentialed_connections() {
    for auth in ["basic", "bearer"] {
        let (mut plugin, mut host) = fixture("https://nas.example/dav/");
        let name = "abcdefghijklmnopqrstuvwxyz123456";
        // Force allocation of a suffix in addition to the longest generated slug.
        plugin.servers[0].id = name.into();
        let commands = plugin
            .event(
                scoped(
                    1,
                    HostEvent::ModalSubmit {
                        id: modal::ADD_MODAL_ID.into(),
                        values: BTreeMap::from([
                            ("name".into(), name.into()),
                            ("endpoint".into(), "https://nas.example/dav/".into()),
                            ("auth".into(), auth.into()),
                            ("username".into(), "test".into()),
                            ("secret".into(), "new-secret".into()),
                        ]),
                    },
                ),
                &mut host,
            )
            .unwrap();
        assert!(commands.is_empty());
        let (write_secret, task) = host.pop();
        let Task::SecretWrite {
            key,
            value: Some(secret),
            ..
        } = task
        else {
            panic!("expected secret write")
        };
        assert!(is_valid_id(&key));
        assert_eq!(secret, "new-secret");
        plugin
            .event(
                scoped(
                    1,
                    done(write_secret, json!({"ok":{"stored":true,"revision":null}})),
                ),
                &mut host,
            )
            .unwrap();
        let (write_state, task) = host.pop();
        let Task::StateWrite {
            value: Some(raw), ..
        } = task
        else {
            panic!("expected config commit")
        };
        let saved = config::decode_servers(Some(&raw)).unwrap();
        assert_eq!(saved[1].id, format!("{name}-2"));
        assert_eq!(saved[1].credential_key.as_deref(), Some(key.as_str()));
        assert!(!raw.contains("new-secret"));
        assert!(is_valid_id(&modal::build_modal(Some(&saved[1]), true).id));
        plugin
            .event(
                scoped(
                    1,
                    done(write_state, json!({"ok":{"stored":true,"revision":"2"}})),
                ),
                &mut host,
            )
            .unwrap();
        assert_eq!(config::secret_key(&plugin.servers[1]), key);
    }
}

#[test]
fn connection_omits_legacy_allow_insecure_auth_and_rejects_userinfo_endpoints() {
    let legacy = r#"[{"id":"nas","name":"NAS","endpoint":"https://nas.example/dav/","allow_insecure_auth":true}]"#;
    let decoded = config::decode_servers(Some(legacy)).unwrap();
    let encoded = serde_json::to_string(&decoded).unwrap();
    assert!(!encoded.contains("allow_insecure_auth"));

    let mut with_user = decoded[0].clone();
    with_user.endpoint = "https://user:pass@nas.example/dav/".into();
    assert!(with_user.root_segments().is_err());

    let mut long_conn = decoded[0].clone();
    long_conn.base_path = format!("/{}", "a".repeat(300));
    let descriptors = modal::source_descriptors(&[long_conn]);
    assert!(iris_plugin_sdk::api::v2::source::SourceDescriptor::validate_all(&descriptors).is_ok());
}

#[test]
fn removing_connection_clears_editing_target_across_all_open_windows() {
    let (mut plugin, mut host) = fixture("https://nas.example/dav/");

    // Window 1 opens Configure modal for `nas`.
    plugin
        .event(
            scoped(
                1,
                HostEvent::SourceManage {
                    source_id: "nas".into(),
                    action: SourceManageAction::Configure,
                },
            ),
            &mut host,
        )
        .unwrap();
    assert_eq!(plugin.windows[&Id(1)].editing.as_deref(), Some("nas"));

    // Window 2 removes `nas` and commits StateWrite.
    plugin
        .event(
            scoped(
                2,
                HostEvent::SourceManage {
                    source_id: "nas".into(),
                    action: SourceManageAction::Remove,
                },
            ),
            &mut host,
        )
        .unwrap();
    assert_eq!(plugin.windows[&Id(1)].editing, None);
    let (rm_state_op, _) = host.pop();
    plugin
        .event(
            scoped(
                2,
                done(rm_state_op, json!({"ok":{"stored":true,"revision":"2"}})),
            ),
            &mut host,
        )
        .unwrap();
    let _ = host.pop(); // old secret cleanup

    // Window 2 adds a fresh connection whose slug is also `nas`.
    plugin
        .event(
            scoped(
                2,
                HostEvent::ModalSubmit {
                    id: modal::ADD_MODAL_ID.into(),
                    values: BTreeMap::from([
                        ("name".into(), "NAS".into()),
                        ("endpoint".into(), "https://fresh.example/dav/".into()),
                        ("auth".into(), "none".into()),
                    ]),
                },
            ),
            &mut host,
        )
        .unwrap();
    let (add_state_op, _) = host.pop();
    plugin
        .event(
            scoped(
                2,
                done(add_state_op, json!({"ok":{"stored":true,"revision":"3"}})),
            ),
            &mut host,
        )
        .unwrap();
    assert_eq!(plugin.servers.len(), 1);
    assert_eq!(plugin.servers[0].id, "nas");
    assert_eq!(plugin.servers[0].endpoint, "https://fresh.example/dav/");

    // Window 1 submitting its stale edit modal must NOT overwrite Window 2's new `nas`.
    let stale_cmds = plugin
        .event(
            scoped(
                1,
                HostEvent::ModalSubmit {
                    id: modal::EDIT_MODAL_ID.into(),
                    values: BTreeMap::from([
                        ("name".into(), "Stale Overwrite".into()),
                        ("endpoint".into(), "https://stale.example/dav/".into()),
                        ("auth".into(), "none".into()),
                    ]),
                },
            ),
            &mut host,
        )
        .unwrap();
    assert!(matches!(
        stale_cmds.as_slice(),
        [HostCommand::ShowToast { message }] if message.contains("未找到正在编辑的连接")
    ));
    assert!(host.tasks.is_empty());
    assert_eq!(plugin.servers[0].endpoint, "https://fresh.example/dav/");
}
