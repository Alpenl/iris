use crate::plugin_v2::{
    actor::{Executor, IoOutcome, Operation, TaskContext},
    files::{ExportGrant, ExportState, random_name},
    instance::Blob,
    ledger::{Ledger, Resource},
    network::NetworkGrant,
    tools::{ToolExecutor, ToolSetup},
};
use crate::vfs::HttpCancel;
use iris_api::v2::{
    Capability, ErrorCode, Generation, Id, NetworkDeclaration, Role,
    task::{ArtifactFormat, Task},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};
fn execute(operation: Operation) -> Result<IoOutcome, ErrorCode> {
    if let Operation::Blocking(run) = operation {
        run()
    } else {
        panic!("blocking artifact job")
    }
}
#[test]
fn inert_artifacts_need_no_native_tool_and_keep_writes_serial_and_cancellable() {
    let ledger = Ledger::default();
    let owner = "artifact-test";
    let dir =
        std::env::temp_dir().join(format!("iris-artifact-service-{}", random_name().unwrap()));
    std::fs::create_dir(&dir).unwrap();
    let capabilities = BTreeSet::from([Capability::MediaEnhance, Capability::ExportFiles]);
    let executor = ToolExecutor::new(ToolSetup {
        network: NetworkGrant::new(
            owner.into(),
            capabilities.clone(),
            NetworkDeclaration::default(),
            &ledger,
        )
        .unwrap(),
        ledger: ledger.clone(),
        capabilities,
        temporary_directory: dir.clone(),
        tools: BTreeMap::new(),
        models: BTreeMap::new(),
        catalog: Default::default(),
    })
    .unwrap();
    let context = || TaskContext {
        instance_id: Id(4),
        window: None,
        owner: owner.into(),
        role: Role::Control,
        generation: Generation::default(),
        cancel: HttpCancel::default(),
        ledger: ledger.clone(),
    };
    let created = execute(
        executor
            .prepare(
                Task::ArtifactBegin {
                    format: ArtifactFormat::WebVtt,
                },
                &BTreeMap::new(),
                context(),
            )
            .unwrap(),
    )
    .unwrap();
    let id = Id(created.value["artifact_writer"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap());
    let blobs = BTreeMap::from([(
        9,
        Arc::new(Blob::new(&ledger, owner, b"WEBVTT\n\n".to_vec()).unwrap()),
    )]);
    let task = || Task::ArtifactWrite {
        handle: id,
        blob: Id(9),
        finish: false,
    };
    let pending = executor.prepare(task(), &blobs, context()).unwrap();
    assert!(matches!(
        executor.prepare(task(), &blobs, context()),
        Err(ErrorCode::Backpressure)
    ));
    let written = execute(pending).unwrap();
    assert_eq!(written.value["written"], "8");
    let finish = execute(
        executor
            .prepare(
                Task::ArtifactWrite {
                    handle: id,
                    blob: Id(9),
                    finish: true,
                },
                &blobs,
                context(),
            )
            .unwrap(),
    )
    .unwrap();
    let artifact = Id(finish.value["artifact"].as_str().unwrap().parse().unwrap());
    let output = executor
        .approved_artifact(owner, artifact, Generation::default())
        .unwrap();
    assert_eq!(output.size(), 16);
    assert!(
        executor
            .approved_artifact("another", artifact, Generation::default())
            .is_err()
    );
    assert_eq!(executor.close(id.0), Err(ErrorCode::StaleHandle));
    let destination = dir.join("saved.vtt");
    let grant = ExportGrant::approve(
        &destination,
        iris_api::v2::enhance::ExportFormat::WebVtt,
        owner.into(),
        Generation::default(),
        &ledger,
    )
    .unwrap();
    let audit = grant.audit();
    let export = executor.add_export(grant).unwrap();
    let task = Task::ExportArtifact { artifact, export };
    assert!(!task.authorized(&BTreeSet::from([Capability::MediaEnhance])));
    let exported = execute(executor.prepare(task, &BTreeMap::new(), context()).unwrap()).unwrap();
    // Losing the guest result or cancelling after commit must not hide the
    // completed host-side export or remove the user's saved file.
    drop(exported);
    assert_eq!(audit.state(), ExportState::Committed);
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        b"WEBVTT\n\nWEBVTT\n\n"
    );
    assert_eq!(executor.close(export.0), Err(ErrorCode::StaleHandle));
    drop(audit);
    drop((created, written, finish, output));
    let created = execute(
        executor
            .prepare(
                Task::ArtifactBegin {
                    format: ArtifactFormat::Text,
                },
                &BTreeMap::new(),
                context(),
            )
            .unwrap(),
    )
    .unwrap();
    let id = Id(created.value["artifact_writer"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap());
    let pending = executor
        .prepare(
            Task::ArtifactWrite {
                handle: id,
                blob: Id(9),
                finish: false,
            },
            &blobs,
            context(),
        )
        .unwrap();
    executor.close(id.0).unwrap();
    assert!(matches!(execute(pending), Err(ErrorCode::Cancelled)));
    drop((created, blobs));
    executor.stop();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !executor.is_quiescent() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(ledger.used(None, Resource::TempBytes), 0);
    assert_eq!(ledger.used(None, Resource::Handle), 0);
    drop(executor);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    std::fs::remove_file(destination).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
