use super::{
    FileExecutor, FileSetup, TempRoot,
    artifact::{ArtifactCleaner, DrainPolicy},
    hash_media,
    identity::PinnedFile,
};
use crate::{
    actor::{Executor, IoOutcome, Operation, TaskContext},
    crypto::random_identity,
    hub::{Config as HubConfig, Hub},
    installation::Installer,
    instance::Blob,
    ledger::{Ledger, Resource},
    network::NetworkGrant,
    product::PluginHost,
    services::{Resources, Setup},
};
use iris_api::v2::{
    Capability, ErrorCode, Generation, Id, NetworkDeclaration, Role,
    task::{ArtifactFormat, FileHash, Task},
};
use iris_base::net::http_client::HttpCancel;
use iris_base::rt::{ThreadRole, live_threads};
use iris_core::vault::CredentialVault;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
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

struct WarnSink {
    entries: Mutex<Vec<String>>,
}

impl log::Log for WarnSink {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            self.entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{}", record.args()));
        }
    }

    fn flush(&self) {}
}

fn warn_sink() -> &'static WarnSink {
    static SINK: OnceLock<WarnSink> = OnceLock::new();
    let sink = SINK.get_or_init(|| WarnSink {
        entries: Mutex::new(Vec::new()),
    });
    let _ = log::set_logger(sink);
    if log::max_level() < log::LevelFilter::Warn {
        log::set_max_level(log::LevelFilter::Warn);
    }
    sink
}

fn has_warn_containing(sink: &WarnSink, start_idx: usize, needle: &str) -> bool {
    sink.entries.lock().unwrap_or_else(|e| e.into_inner())[start_idx..]
        .iter()
        .any(|line| line.contains(needle))
}

#[test]
fn multiple_plugin_hosts_run_isolated_artifact_cleanup_workers_without_residue() {
    let dir1 = directory();
    let dir2 = directory();
    let res1 = Resources::new().unwrap();
    let res2 = Resources::new().unwrap();

    let make_host = |res: &Resources, root: &PathBuf| {
        let hub = Hub::new(HubConfig {
            installer: Installer::new(root.join("pkgs"), res.ledger.clone()),
            bootstrap: None,
            runtime: Setup {
                resources: res.clone(),
                state_directory: root.join("state"),
                temporary_directory: root.join("temp"),
                vault: CredentialVault::in_memory(),
                wake: Arc::new(|| {}),
            },
            disabled: BTreeSet::new(),
        })
        .unwrap();
        PluginHost::from_parts(res.clone(), hub)
    };

    let host1 = make_host(&res1, &dir1);
    let host2 = make_host(&res2, &dir2);
    let cancel = HttpCancel::default();

    let temp1 = dir1.join("temp").join("owner-a");
    let temp2 = dir2.join("temp").join("owner-b");
    let root1 = TempRoot::with_cleaner(
        &temp1,
        "owner-a".into(),
        res1.ledger.clone(),
        res1.cleaner.queue(),
    )
    .unwrap();
    let root2 = TempRoot::with_cleaner(
        &temp2,
        "owner-b".into(),
        res2.ledger.clone(),
        res2.cleaner.queue(),
    )
    .unwrap();

    assert!(
        live_threads(ThreadRole::ArtifactCleanup) >= 2,
        "expected >= 2 concurrent per-host ArtifactCleanup workers"
    );

    let mut w1 = root1.writer("txt").unwrap();
    w1.write(b"alpha", &cancel).unwrap();
    let a1 = w1.finish(&cancel).unwrap();
    let p1 = a1.path();

    let mut w2 = root2.writer("txt").unwrap();
    w2.write(b"bravo-data", &cancel).unwrap();
    let a2 = w2.finish(&cancel).unwrap();
    let p2 = a2.path();

    assert!(p1.is_file() && p2.is_file());
    drop(a1);
    idle(&root1);
    assert!(!p1.exists());
    assert_eq!(res1.ledger.used(None, Resource::TempBytes), 0);
    assert_eq!(res1.ledger.used(None, Resource::Handle), 0);

    // Shutting down host1 joins host1's cleanup worker while host2's worker remains active.
    drop(root1);
    host1.shutdown();
    drop((host1, res1));
    assert!(
        live_threads(ThreadRole::ArtifactCleanup) >= 1,
        "host2 cleanup worker must remain active after host1 shutdown"
    );

    drop(a2);
    idle(&root2);
    assert!(!p2.exists());

    // Verify host2's cleanup worker still processes fresh artifacts after host1 shut down.
    let mut w2_next = root2.writer("json").unwrap();
    w2_next.write(b"{}", &cancel).unwrap();
    let a2_next = w2_next.finish(&cancel).unwrap();
    let p2_next = a2_next.path();
    drop(a2_next);
    idle(&root2);
    assert!(!p2_next.exists());
    assert_eq!(res2.ledger.used(None, Resource::TempBytes), 0);
    assert_eq!(res2.ledger.used(None, Resource::Handle), 0);

    drop(root2);
    host2.shutdown();
    drop((host2, res2));

    assert!(!temp1.exists());
    assert!(!temp2.exists());
    let _ = std::fs::remove_dir_all(&dir1);
    let _ = std::fs::remove_dir_all(&dir2);
}

#[test]
fn persistent_delete_failure_completes_bounded_drain_and_join_with_warning_and_fail_closed_quota() {
    let sink = warn_sink();
    let log_start = sink.entries.lock().unwrap_or_else(|e| e.into_inner()).len();

    // Part A: Persistent remove_file error exhausts bounded drain retries and retains quota.
    let dir_a = directory();
    let ledger_a = Ledger::default();
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_hook = attempts.clone();
    let cleaner_a = ArtifactCleaner::with_test_hooks(
        DrainPolicy {
            poll_interval: Duration::from_millis(200),
            drain_retry_limit: 3,
            drain_retry_delay: Duration::from_millis(10),
            drain_timeout: Duration::from_millis(500),
            join_timeout: Duration::from_secs(1),
        },
        Arc::new(move |_dir, _name| {
            attempts_hook.fetch_add(1, Ordering::AcqRel);
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "simulated locked artifact",
            ))
        }),
    );
    let root_a = TempRoot::with_cleaner(
        &dir_a.join("owner-fail"),
        "owner-fail".into(),
        ledger_a.clone(),
        cleaner_a.queue(),
    )
    .unwrap();
    let cancel = HttpCancel::default();
    let mut writer = root_a.writer("txt").unwrap();
    writer.write(b"locked-data", &cancel).unwrap();
    let artifact = writer.finish(&cancel).unwrap();
    let artifact_path = artifact.path();
    let artifact_name = artifact_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(ledger_a.used(Some("owner-fail"), Resource::TempBytes), 11);
    assert_eq!(ledger_a.used(Some("owner-fail"), Resource::Handle), 1);

    drop(artifact);
    let t0 = Instant::now();
    cleaner_a.shutdown();
    let elapsed_a = t0.elapsed();
    assert!(
        elapsed_a < Duration::from_secs(2),
        "bounded drain + join took too long: {elapsed_a:?}"
    );
    assert!(attempts.load(Ordering::Acquire) >= 3);
    assert!(
        artifact_path.is_file(),
        "undeletable file must stay on disk"
    );
    assert_eq!(ledger_a.used(Some("owner-fail"), Resource::TempBytes), 11);
    assert_eq!(ledger_a.used(Some("owner-fail"), Resource::Handle), 1);
    assert!(!root_a.is_quiescent());
    assert!(has_warn_containing(
        sink,
        log_start,
        "artifact cleanup drain exhausted"
    ));
    assert!(has_warn_containing(sink, log_start, &artifact_name));
    drop((root_a, cleaner_a));
    let _ = std::fs::remove_dir_all(&dir_a);

    // Part B: Blocked remove_file exceeding join_timeout detaches cleanly without hanging shutdown.
    let dir_b = directory();
    let ledger_b = Ledger::default();
    let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let gate_hook = gate.clone();
    let log_mid = sink.entries.lock().unwrap_or_else(|e| e.into_inner()).len();
    let cleaner_b = ArtifactCleaner::with_test_hooks(
        DrainPolicy {
            poll_interval: Duration::from_millis(200),
            drain_retry_limit: 1,
            drain_retry_delay: Duration::from_millis(5),
            drain_timeout: Duration::from_millis(20),
            join_timeout: Duration::from_millis(60),
        },
        Arc::new(move |dir, name| {
            let (lock, cv) = &*gate_hook;
            let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
            st.0 = true;
            cv.notify_all();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !st.1 && Instant::now() < deadline {
                let (next, _) = cv
                    .wait_timeout(st, Duration::from_millis(25))
                    .unwrap_or_else(|e| e.into_inner());
                st = next;
            }
            drop(st);
            dir.directory().remove_file(name)
        }),
    );
    let root_b = TempRoot::with_cleaner(
        &dir_b.join("owner-slow"),
        "owner-slow".into(),
        ledger_b.clone(),
        cleaner_b.queue(),
    )
    .unwrap();
    let mut w_b = root_b.writer("txt").unwrap();
    w_b.write(b"slow", &cancel).unwrap();
    let art_b = w_b.finish(&cancel).unwrap();
    drop(art_b);

    // Wait until worker enters the simulated blocking syscall.
    {
        let (lock, cv) = &*gate;
        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !st.0 {
            assert!(Instant::now() < deadline);
            st = cv
                .wait_timeout(st, Duration::from_millis(10))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    let t1 = Instant::now();
    cleaner_b.shutdown();
    let elapsed_b = t1.elapsed();
    assert!(
        elapsed_b < Duration::from_millis(450),
        "join_timeout did not bound shutdown: {elapsed_b:?}"
    );
    assert!(has_warn_containing(sink, log_mid, "did not exit within"));

    // Release the blocked test thread so no test worker stays parked.
    {
        let (lock, cv) = &*gate;
        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
        st.1 = true;
        cv.notify_all();
    }
    idle(&root_b);
    drop((root_b, cleaner_b));
    let _ = std::fs::remove_dir_all(&dir_b);
}

#[test]
fn post_shutdown_late_artifacts_delete_synchronously_and_retain_quota_on_failure() {
    let sink = warn_sink();
    let log_start = sink.entries.lock().unwrap_or_else(|e| e.into_inner()).len();
    let dir = directory();
    let ledger = Ledger::default();
    let fail_post_shutdown = Arc::new(AtomicBool::new(false));
    let fail_flag = fail_post_shutdown.clone();
    let cleaner = ArtifactCleaner::with_test_hooks(
        DrainPolicy::default(),
        Arc::new(move |dir, name| {
            if fail_flag.load(Ordering::Acquire) {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "simulated post-shutdown delete failure",
                ))
            } else {
                dir.directory().remove_file(name)
            }
        }),
    );
    let root = TempRoot::with_cleaner(
        &dir.join("owner-late"),
        "owner-late".into(),
        ledger.clone(),
        cleaner.queue(),
    )
    .unwrap();
    let cancel = HttpCancel::default();

    let mut w_ok = root.writer("txt").unwrap();
    w_ok.write(b"late-ok", &cancel).unwrap();
    let late_ok = w_ok.finish(&cancel).unwrap();
    let path_ok = late_ok.path();

    let mut late_writer = root.writer("txt").unwrap();
    late_writer.write(b"abort", &cancel).unwrap();
    let path_writer = root.path.join(&late_writer.name);

    let mut w_fail = root.writer("txt").unwrap();
    w_fail.write(b"late-fail", &cancel).unwrap();
    let late_fail = w_fail.finish(&cancel).unwrap();
    let path_fail = late_fail.path();

    assert_eq!(ledger.used(Some("owner-late"), Resource::TempBytes), 21);
    assert_eq!(ledger.used(Some("owner-late"), Resource::Handle), 3);

    // Shut down the background cleanup worker while all 3 handles remain live.
    cleaner.shutdown();

    // Late-dropped Artifact and ArtifactWriter succeed synchronously on the caller thread.
    drop(late_ok);
    drop(late_writer);
    assert!(!path_ok.exists());
    assert!(!path_writer.exists());
    assert_eq!(ledger.used(Some("owner-late"), Resource::TempBytes), 9);
    assert_eq!(ledger.used(Some("owner-late"), Resource::Handle), 1);

    // Late-dropped Artifact whose synchronous delete fails retains its Ledger quota fail-closed.
    fail_post_shutdown.store(true, Ordering::Release);
    drop(late_fail);
    assert!(path_fail.is_file());
    assert_eq!(ledger.used(Some("owner-late"), Resource::TempBytes), 9);
    assert_eq!(ledger.used(Some("owner-late"), Resource::Handle), 1);
    assert!(has_warn_containing(
        sink,
        log_start,
        "post-shutdown synchronous artifact cleanup failed"
    ));

    drop((root, cleaner));
    let _ = std::fs::remove_dir_all(&dir);
}
