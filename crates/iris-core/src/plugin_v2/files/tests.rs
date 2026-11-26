use super::{FileExecutor, FileSetup, TempRoot, hash_media, identity::PinnedFile};
use crate::{
    plugin_v2::{
        actor::{Executor, IoOutcome, Operation, TaskContext},
        installation::random_identity,
        instance::Blob,
        ledger::{Ledger, Resource},
        network::NetworkGrant,
    },
    vfs::HttpCancel,
};
use iris_api::v2::{
    Capability, ErrorCode, Generation, Id, NetworkDeclaration, Role,
    task::{ArtifactFormat, FileHash, Task},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

fn directory() -> PathBuf {
    let path = std::env::temp_dir().join(format!("iris-files-{}", random_identity().unwrap()));
    std::fs::create_dir(&path).unwrap();
    path
}

/// `TempRoot` removes its empty parent on drop; a leftover parent must still be empty.
fn remove_released_parent(dir: PathBuf) {
    match std::fs::remove_dir(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!(
            "temporary parent {} was not released: {error}",
            dir.display()
        ),
    }
}

fn idle(root: &TempRoot) {
    let end = Instant::now() + Duration::from_secs(3);
    while !root.is_quiescent() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn execute(operation: Operation) -> Result<IoOutcome, ErrorCode> {
    if let Operation::Blocking(run) = operation {
        run()
    } else {
        panic!("blocking artifact job")
    }
}

#[test]
fn pinned_file_hashes_and_rejects_hardlink_aliases() {
    let dir = directory();
    let path = dir.join("input.bin");
    std::fs::write(&path, b"abc").unwrap();
    let pinned = PinnedFile::open(&path).unwrap();
    #[cfg(windows)]
    {
        assert!(std::fs::write(&path, b"tampered").is_err());
        assert!(std::fs::remove_file(&path).is_err());
    }
    assert_eq!(
        hash_media(
            &pinned,
            FileHash::FullMd5,
            &HttpCancel::default(),
            |_, _| {}
        )
        .unwrap()
        .digest,
        "900150983cd24fb0d6963f7d28e17f72"
    );
    assert_eq!(
        hash_media(
            &pinned,
            FileHash::FullSha256,
            &HttpCancel::default(),
            |_, _| {}
        )
        .unwrap()
        .digest,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let sparse = hash_media(
        &pinned,
        FileHash::SparseSha256V1,
        &HttpCancel::default(),
        |_, _| {},
    )
    .unwrap();
    let mut vector = b"iris-sparse-sha256-v1\0".to_vec();
    vector.extend_from_slice(&3u64.to_le_bytes());
    vector.extend_from_slice(&0u64.to_le_bytes());
    vector.extend_from_slice(&3u64.to_le_bytes());
    vector.extend_from_slice(b"abc");
    let expected: String = ring::digest::digest(&ring::digest::SHA256, &vector)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(sparse.digest, expected);
    let cancel = HttpCancel::default();
    cancel.cancel();
    assert!(matches!(
        hash_media(&pinned, FileHash::FullSha256, &cancel, |_, _| {}),
        Err(ErrorCode::Cancelled)
    ));
    drop(pinned);
    std::fs::hard_link(&path, dir.join("alias.bin")).unwrap();
    assert!(PinnedFile::open(&path).is_err());
    std::fs::remove_file(dir.join("alias.bin")).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[test]
fn artifacts_keep_disk_quota_until_cleanup() {
    let dir = directory();
    let ledger = Ledger::default();
    let root = TempRoot::new(&dir, "one".into(), ledger.clone()).unwrap();
    let cancel = HttpCancel::default();
    let mut writer = root.writer("txt").unwrap();
    writer.write(b"result", &cancel).unwrap();
    let artifact = writer.finish(&cancel).unwrap();
    assert_eq!(ledger.used(Some("one"), Resource::TempBytes), 6);
    let hold = ledger
        .reserve("one", &[(Resource::TempBytes, (1 << 30) - 6)])
        .unwrap();
    let mut rejected = root.writer("txt").unwrap();
    assert_eq!(
        rejected.write(b"over quota", &cancel),
        Err(ErrorCode::QuotaExceeded)
    );
    drop(rejected);
    drop(hold);
    drop(artifact);
    idle(&root);
    assert_eq!(ledger.used(None, Resource::TempBytes), 0);
    assert_eq!(ledger.used(None, Resource::Handle), 0);
    drop(root);
    remove_released_parent(dir);
}

#[test]
fn inert_artifacts_keep_writes_serial_and_cancellable() {
    let ledger = Ledger::default();
    let owner = "artifact-test";
    let dir = directory();
    let capabilities = BTreeSet::from([Capability::MediaEnhance]);
    let executor = FileExecutor::new(FileSetup {
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
    remove_released_parent(dir);
}

#[test]
fn lazy_temp_root_creates_zero_directories_across_window_open_close_cycles() {
    let ledger = Ledger::default();
    let owner = "lazy-temp";
    let dir = directory();
    let capabilities = BTreeSet::from([Capability::MediaEnhance]);
    let root = FileExecutor::new(FileSetup {
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
    })
    .unwrap();
    for _ in 0..50 {
        let window = root.for_window();
        window.stop();
        assert!(window.is_quiescent());
    }
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    root.stop();
    drop(root);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    std::fs::remove_dir(dir).unwrap();
}
