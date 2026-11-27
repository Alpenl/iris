use super::*;
use iris_api::SourceManageAction;
use iris_plugin_host::{ActionKind, PluginStatus, PollReport};

fn poll(session: &mut WindowSession) -> PollReport {
    let report = session.poll();
    for plugin in session.list() {
        assert!(
            !matches!(plugin.status, PluginStatus::Failed(_)),
            "{:?}",
            plugin.status
        );
    }
    for notice in &report.notices {
        assert!(
            !matches!(notice, Notice::Error { .. }),
            "unexpected host error"
        );
    }
    report
}

#[test]
#[ignore = "build official packages with scripts/test-plugin-v2-official.py"]
fn official_webdav_long_named_connection_can_be_saved_and_edited() {
    let fixture = Fixture::load("IRIS_V2_WEBDAV_PACKAGE");
    let mut session = fixture.open_window();
    wait(|| {
        let ready = session.is_ready("iris.webdav");
        poll(&mut session);
        ready
            && session
                .list()
                .iter()
                .any(|p| p.id == "iris.webdav" && p.status == PluginStatus::Ready)
    });
    let name = "abcdefghijklmnopqrstuvwxyz123456";
    let mut values = BTreeMap::from([
        ("name".into(), name.into()),
        ("endpoint".into(), "https://nas.example/dav/".into()),
        ("auth".into(), "basic".into()),
        ("username".into(), "test".into()),
        ("secret".into(), "password".into()),
    ]);
    let sent = session.send_to(
        "iris.webdav",
        HostEvent::ModalSubmit {
            id: "webdav.add".into(),
            values: values.clone(),
        },
    );
    assert!(sent.notices.is_empty());

    let mut saw_toast = false;
    wait(|| {
        for action in poll(&mut session).actions {
            if let ActionKind::ShowToast { message } = action.kind {
                assert!(!message.trim().is_empty(), "expected non-empty save toast");
                saw_toast = true;
            }
        }
        saw_toast && session.media_sources().len() == 1
    });
    assert!(saw_toast, "saving a WebDAV connection should emit a toast");

    let sent = session.send_to(
        "iris.webdav",
        HostEvent::SourceManage {
            source_id: name.into(),
            action: SourceManageAction::Configure,
        },
    );
    assert!(sent.notices.is_empty());

    let mut modal_id = None;
    wait(|| {
        poll(&mut session);
        if let Some((pid, modal)) = session.active_modal()
            && pid == "iris.webdav"
        {
            modal_id = Some(modal.id.clone());
        }
        modal_id.is_some()
    });
    values.insert("name".into(), "Renamed connection".into());
    values.insert("secret".into(), String::new());
    let sent = session.send_to(
        "iris.webdav",
        HostEvent::ModalSubmit {
            id: modal_id.unwrap(),
            values,
        },
    );
    assert!(sent.notices.is_empty());

    wait(|| {
        poll(&mut session);
        session
            .media_sources()
            .iter()
            .any(|(_, _, s)| s.id == name && s.name == "Renamed connection")
    });
    drop(session);
    fixture.shutdown();
}
