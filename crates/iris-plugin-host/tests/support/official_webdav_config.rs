use super::*;
use iris_api::{SourceManageAction, v2::HostCommand};
use iris_plugin_host::hub::{Client, Notice, PluginStatus};

fn poll(client: &Client) -> Vec<Notice> {
    let notices = client.poll();
    for plugin in &client.catalog().plugins {
        assert!(
            !matches!(plugin.status, PluginStatus::Failed(_)),
            "{:?}",
            plugin.status
        );
    }
    for notice in &notices {
        assert!(
            !matches!(notice, Notice::Error { .. }),
            "unexpected host error"
        );
    }
    notices
}

#[test]
#[ignore = "build official packages with scripts/test-plugin-v2-official.py"]
fn official_webdav_long_named_connection_can_be_saved_and_edited() {
    let fixture = Fixture::load("IRIS_V2_WEBDAV_PACKAGE");
    let (client, _) = fixture
        .hub()
        .open_window(Generation::default(), None)
        .unwrap();
    wait(|| {
        poll(&client);
        client.is_ready("iris.webdav")
    });
    let name = "abcdefghijklmnopqrstuvwxyz123456";
    let mut values = BTreeMap::from([
        ("name".into(), name.into()),
        ("endpoint".into(), "https://nas.example/dav/".into()),
        ("auth".into(), "basic".into()),
        ("username".into(), "test".into()),
        ("secret".into(), "password".into()),
    ]);
    client
        .send(
            "iris.webdav",
            HostEvent::ModalSubmit {
                id: "webdav.add".into(),
                values: values.clone(),
            },
        )
        .unwrap();
    let mut saw_toast = false;
    wait(|| {
        for notice in poll(&client) {
            if let Notice::Commands(batch) = notice {
                for &i in &batch.indices {
                    if let HostCommand::ShowToast { message } = &batch.update.commands[i] {
                        assert!(!message.trim().is_empty(), "expected non-empty save toast");
                        saw_toast = true;
                    }
                }
            }
        }
        client
            .catalog()
            .plugins
            .iter()
            .any(|p| p.sources.as_ref().is_some_and(|s| s.sources.len() == 1))
    });
    assert!(saw_toast, "saving a WebDAV connection should emit a toast");
    client
        .send(
            "iris.webdav",
            HostEvent::SourceManage {
                source_id: name.into(),
                action: SourceManageAction::Configure,
            },
        )
        .unwrap();
    let mut modal_id = None;
    wait(|| {
        for notice in poll(&client) {
            if let Notice::Commands(batch) = notice {
                for &i in &batch.indices {
                    if let HostCommand::OpenModal { modal } = &batch.update.commands[i] {
                        modal_id = Some(modal.id.clone());
                    }
                }
            }
        }
        modal_id.is_some()
    });
    values.insert("name".into(), "Renamed connection".into());
    values.insert("secret".into(), String::new());
    client
        .send(
            "iris.webdav",
            HostEvent::ModalSubmit {
                id: modal_id.unwrap(),
                values,
            },
        )
        .unwrap();
    wait(|| {
        poll(&client);
        client.catalog().plugins.iter().any(|p| {
            p.sources.as_ref().is_some_and(|s| {
                s.sources
                    .iter()
                    .any(|s| s.id == name && s.name == "Renamed connection")
            })
        })
    });
    drop(client);
    fixture.shutdown();
}
