use super::*;
use iris_plugin_sdk::api::v2::source::SourcePage;

fn browse(id: u64, cursor: Option<String>) -> HostEvent<Value> {
    win(HostEvent::SourceBrowse {
        request_id: Id(id),
        source_id: "files".into(),
        folder_id: None,
        page: cursor,
        query: None,
        filters: BTreeMap::new(),
    })
}

fn page(commands: Vec<HostCommand>) -> SourcePage {
    assert!(serde_json::to_vec(&commands).unwrap().len() < 64 * 1024);
    let [HostCommand::ProvideSourcePage { page, .. }]: [HostCommand; 1] =
        commands.try_into().unwrap()
    else {
        panic!("expected source page");
    };
    *page
}

fn first_page(count: u32) -> (BaiduPanPlugin, MockHost, SourcePage) {
    let mut plugin = BaiduPanPlugin::default();
    plugin.session.bduss = "fixture-session".into();
    let mut host = MockHost::default();
    plugin
        .event(browse(1, None), &mut host, 1_700_000_000_000)
        .unwrap();
    let (op, _) = host.pop();
    let items: Vec<_> = (1..=count)
        .map(|n| {
            json!({
                "path":format!("/video{n}.mp4"), "fs_id":n, "category":1, "isdir":0,
                "server_filename":format!("video{n}.mp4"),
                "thumbs":{"url3":format!("https://thumbnail.baidupcs.com/{}.jpg", "x".repeat(1800))}
            })
        })
        .collect();
    let result = page(feed_http(
        &mut plugin,
        &mut host,
        op,
        200,
        "https://pan.baidu.com/api/list",
        vec![],
        &serde_json::to_vec(&json!({"errno":0,"list":items})).unwrap(),
    ));
    (plugin, host, result)
}

#[test]
fn browse_byte_budget_preserves_all_entries_and_remote_continuation() {
    for count in [30, catalog::PAGE_SIZE] {
        let (mut plugin, mut host, mut current) = first_page(count);
        assert!(!current.entries.is_empty() && current.entries.len() < count as usize);
        let first_cursor = current
            .next_page
            .clone()
            .expect("local tail must be reachable");
        let mut ids = BTreeSet::new();
        let mut cursors = BTreeSet::new();
        loop {
            assert_eq!(current.folder_id, None);
            assert!(current.authenticated);
            for entry in current.entries {
                assert!(ids.insert(entry.id), "duplicate entry across local pages");
            }
            let Some(cursor) = current
                .next_page
                .as_deref()
                .filter(|c| c.starts_with("local:"))
            else {
                break;
            };
            assert!(cursors.insert(cursor.to_owned()), "pagination loop");
            current = page(
                plugin
                    .event(
                        browse(1 + cursors.len() as u64, Some(cursor.into())),
                        &mut host,
                        1_700_000_000_000,
                    )
                    .unwrap(),
            );
        }
        let expected: BTreeSet<_> = (1..=count)
            .map(|n| catalog::encode_entry_id(u64::from(n), &format!("/video{n}.mp4")))
            .collect();
        assert_eq!(ids, expected);
        assert!(
            host.tasks.is_empty(),
            "local continuation must not fetch upstream"
        );
        let replay = plugin
            .event(
                browse(100, Some(first_cursor)),
                &mut host,
                1_700_000_000_000,
            )
            .unwrap();
        assert!(matches!(
            replay.as_slice(),
            [HostCommand::SourceError { .. }]
        ));
        if count == catalog::PAGE_SIZE {
            assert_eq!(current.next_page.as_deref(), Some("2"));
            plugin
                .event(browse(101, current.next_page), &mut host, 1_700_000_000_000)
                .unwrap();
            let (_, task) = host.pop();
            assert!(matches!(task, Task::HttpStart { url, .. } if url.contains("page=2")));
        } else {
            assert!(current.next_page.is_none());
        }
    }
}

#[test]
fn local_pages_are_bound_to_the_listing_window_and_context() {
    for boundary in [
        "source", "folder", "query", "filters", "window", "context", "listing", "logout", "media",
    ] {
        let (mut plugin, mut host, current) = first_page(30);
        let cursor = current.next_page.unwrap();
        if boundary == "listing" {
            plugin
                .event(browse(2, None), &mut host, 1_700_000_000_000)
                .unwrap();
            host.pop();
        } else if boundary == "logout" {
            plugin
                .event(
                    win(HostEvent::Command {
                        id: "logout".into(),
                        snapshot: None,
                    }),
                    &mut host,
                    1_700_000_000_000,
                )
                .unwrap();
            host.tasks.clear();
        }
        let mut event = browse(3, Some(cursor));
        let HostEvent::Window {
            window,
            generation,
            body,
        } = &mut event
        else {
            unreachable!()
        };
        let HostEvent::SourceBrowse {
            source_id,
            folder_id,
            query,
            filters,
            ..
        } = body.as_mut()
        else {
            unreachable!()
        };
        match boundary {
            "source" => *source_id = "videos".into(),
            "folder" => *folder_id = Some("/".into()),
            "query" => *query = Some("video".into()),
            "filters" => {
                filters.insert("order".into(), "time".into());
            }
            "window" => *window = Id(2),
            "context" => generation.context = Id(1),
            "media" => generation.media = Id(1),
            _ => {}
        }
        let commands = plugin.event(event, &mut host, 1_700_000_000_000).unwrap();
        if boundary == "media" {
            assert_eq!(current.entries.len() + page(commands).entries.len(), 30);
        } else {
            assert!(
                matches!(commands.as_slice(), [HostCommand::SourceError { .. }]),
                "{boundary}"
            );
        }
        assert!(
            host.tasks.is_empty(),
            "invalid local cursor must not fall back to remote page 1"
        );
    }
}
