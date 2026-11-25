use super::*;
use iris_plugin_sdk::{api::OverlayNode, serde_json::json};
#[derive(Default)]
struct Mock {
    next: u64,
    tasks: Vec<Task>,
}
impl Host for Mock {
    fn submit(&mut self, task: &Task) -> Result<Id, ErrorCode> {
        self.next += 1;
        self.tasks.push(task.clone());
        Ok(Id(self.next))
    }
    fn blob(&mut self, _: &[u8]) -> Result<Id, ErrorCode> {
        Err(ErrorCode::Unsupported)
    }
    fn take_blob(&mut self, _: Id) -> Result<Vec<u8>, ErrorCode> {
        Err(ErrorCode::Unsupported)
    }
    fn close(&mut self, _: Id) {}
}
fn scoped(window: u64, seek: u64, body: HostEvent<Value>) -> HostEvent<Value> {
    HostEvent::Window {
        window: Id(window),
        generation: Generation {
            seek: Id(seek),
            ..Generation::default()
        },
        body: Box::new(body),
    }
}
fn completion(id: u64, mode: &str) -> HostEvent<Value> {
    HostEvent::Completed {
        operation_id: Id(id),
        generation: Generation::default(),
        result: json!({"ok":{"store_value":mode,"revision":"1"}}),
    }
}
#[test]
fn persisted_modes_are_window_scoped_and_late_reads_cannot_override_actions() {
    let mut plugin = Guides::default();
    let mut host = Mock::default();
    plugin
        .event(
            scoped(1, 0, HostEvent::WindowOpened { snapshot: None }),
            &mut host,
        )
        .unwrap();
    plugin
        .event(
            scoped(2, 0, HostEvent::WindowOpened { snapshot: None }),
            &mut host,
        )
        .unwrap();
    let first = plugin
        .event(scoped(1, 0, completion(1, "thirds")), &mut host)
        .unwrap();
    assert!(first.iter().any(|c| matches!(c, HostCommand::SetOverlay { nodes } if matches!(nodes.as_slice(), [OverlayNode::Grid { cols: 3, .. }]))));
    plugin
        .event(
            scoped(
                2,
                0,
                HostEvent::Command {
                    id: "cross".into(),
                    snapshot: None,
                },
            ),
            &mut host,
        )
        .unwrap();
    // The second window's slow initial read must not undo its user action.
    let late = plugin
        .event(scoped(2, 0, completion(2, "thirds")), &mut host)
        .unwrap();
    assert!(
        !late
            .iter()
            .any(|c| matches!(c, HostCommand::SetOverlay { .. }))
    );
    assert!(
        matches!(host.tasks.last(), Some(Task::StateWrite { value: Some(v), .. }) if v == "cross")
    );
    let first_cycle = plugin
        .event(
            scoped(
                1,
                0,
                HostEvent::Command {
                    id: "cycle".into(),
                    snapshot: None,
                },
            ),
            &mut host,
        )
        .unwrap();
    assert!(first_cycle.iter().any(
        |c| matches!(c, HostCommand::SetStatus { badge: Some(badge) } if badge.text == "黄金分割")
    ));
    let stale = plugin
        .event(scoped(2, 1, completion(2, "safe")), &mut host)
        .unwrap();
    assert!(stale.is_empty());
    plugin
        .event(HostEvent::WindowClosed { window: Id(2) }, &mut host)
        .unwrap();
    let reopened = plugin
        .event(
            scoped(2, 0, HostEvent::WindowOpened { snapshot: None }),
            &mut host,
        )
        .unwrap();
    assert!(
        reopened
            .iter()
            .any(|c| matches!(c, HostCommand::SetOverlay { nodes } if nodes.is_empty()))
    );
}
