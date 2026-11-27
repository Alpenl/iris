use super::*;
use iris_api::v2::{NetworkDeclaration, task::ArtifactFormat};
use iris_core::vault::CredentialVault;

const OWNER: &str = "0123456789abcdef0123456789abcdef";

struct Fixture {
    root: PathBuf,
    ledger: Ledger,
    services: ControlServices,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "iris-services-test-{}-{}",
            std::process::id(),
            crate::instance::next_id().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let ledger = Ledger::default();
        let caps = BTreeSet::from([Capability::MediaEnhance]);
        let grant = NetworkGrant::new(
            OWNER.into(),
            caps.clone(),
            NetworkDeclaration::default(),
            &ledger,
        )
        .unwrap();
        let storage = Arc::new(
            Storage::new(
                root.join("state"),
                OWNER.into(),
                CredentialVault::in_memory(),
                ledger.clone(),
            )
            .unwrap(),
        );
        let services = ControlServices::new(
            OWNER.into(),
            ledger.clone(),
            grant,
            storage,
            caps,
            root.join("temp"),
        )
        .unwrap();
        Self {
            root,
            ledger,
            services,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn single_window_isolates_generations_enforces_capacity_and_keeps_temp_root_lazy() {
    let f = Fixture::new();
    let gen0 = Generation::default();
    let browser: BrowserOpen = Arc::new(|_, _, _| Err(ErrorCode::Denied));

    f.services
        .open_window(Id(1), gen0, browser.clone())
        .unwrap();
    assert_eq!(
        f.services.open_window(Id(1), gen0, browser.clone()).err(),
        Some(ErrorCode::BadState)
    );
    assert_eq!(
        f.services.open_window(Id(2), gen0, browser.clone()).err(),
        Some(ErrorCode::Backpressure)
    );
    // Zero temp directories created until an artifact is actually opened.
    assert!(!f.root.join("temp").exists());

    // Closing the active window frees the slot and rejects stale calls.
    f.services.window_changed(Id(1), None);
    assert_eq!(
        f.services.close_in(Some(Id(1)), 99),
        Err(ErrorCode::StaleHandle)
    );
    f.services
        .open_window(Id(2), gen0, browser.clone())
        .unwrap();

    // Advancing window 2 rejects tasks tagged with window 2's old generation while accepting gen1.
    let gen1 = Generation {
        media: Id(2),
        ..gen0
    };
    f.services.window_changed(Id(2), Some(gen1));
    let stale_ctx = TaskContext {
        instance_id: Id(1),
        window: Some(Id(2)),
        owner: OWNER.into(),
        role: Role::Control,
        generation: gen0,
        cancel: iris_base::net::http_client::HttpCancel::default(),
        ledger: f.ledger.clone(),
    };
    assert!(matches!(
        f.services.prepare(
            Task::ArtifactBegin {
                format: ArtifactFormat::Text,
            },
            &BTreeMap::new(),
            stale_ctx,
        ),
        Err(ErrorCode::StaleHandle)
    ));
    let ok_ctx = TaskContext {
        instance_id: Id(1),
        window: Some(Id(2)),
        owner: OWNER.into(),
        role: Role::Control,
        generation: gen1,
        cancel: iris_base::net::http_client::HttpCancel::default(),
        ledger: f.ledger.clone(),
    };
    assert!(
        f.services
            .prepare(
                Task::ArtifactBegin {
                    format: ArtifactFormat::Text,
                },
                &BTreeMap::new(),
                ok_ctx,
            )
            .is_ok()
    );

    f.services.stop();
    assert!(f.services.is_quiescent());
    assert_eq!(
        f.services.open_window(Id(6), gen0, browser).err(),
        Some(ErrorCode::Cancelled)
    );
}

fn create_live_artifact(
    f: &Fixture,
    files: &FileExecutor,
    window: Id,
    generation: Generation,
) -> Arc<crate::files::Artifact> {
    let ctx = || TaskContext {
        instance_id: Id(1),
        window: Some(window),
        owner: OWNER.into(),
        role: Role::Control,
        generation,
        cancel: iris_base::net::http_client::HttpCancel::default(),
        ledger: f.ledger.clone(),
    };
    let Operation::Blocking(begin) = f
        .services
        .prepare(
            Task::ArtifactBegin {
                format: ArtifactFormat::Cube,
            },
            &BTreeMap::new(),
            ctx(),
        )
        .unwrap()
    else {
        panic!("expected blocking artifact begin");
    };
    let mut began = begin().unwrap();
    began.acknowledge();
    let writer_id = Id(began.value["artifact_writer"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap());
    let blobs = BTreeMap::from([(
        1,
        Arc::new(Blob::new(&f.ledger, OWNER, b"LUT_3D_SIZE 2\n".to_vec()).unwrap()),
    )]);
    let Operation::Blocking(write) = f
        .services
        .prepare(
            Task::ArtifactWrite {
                handle: writer_id,
                blob: Id(1),
                finish: true,
            },
            &blobs,
            ctx(),
        )
        .unwrap()
    else {
        panic!("expected blocking artifact write");
    };
    let mut finished = write().unwrap();
    finished.acknowledge();
    let artifact_id = Id(finished.value["artifact"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap());
    files
        .approved_artifact(OWNER, artifact_id, generation)
        .unwrap()
}

#[test]
fn retired_busy_windows_enforce_four_scope_cap_and_reap_when_quiescent() {
    let f = Fixture::new();
    let gen0 = Generation::default();
    let browser: BrowserOpen = Arc::new(|_, _, _| Err(ErrorCode::Denied));
    let mut held = Vec::new();

    for id in 1..=4 {
        let files = f
            .services
            .open_window(Id(id), gen0, browser.clone())
            .unwrap();
        let artifact = create_live_artifact(&f, &files, Id(id), gen0);
        f.services.window_changed(Id(id), None);
        held.push(artifact);
    }

    // All 4 retired scopes still hold uncleaned artifacts, so a 5th open_window hits Backpressure.
    assert_eq!(
        f.services.open_window(Id(5), gen0, browser.clone()).err(),
        Some(ErrorCode::Backpressure)
    );

    // Releasing one artifact allows cleanup to finish and reap that retired scope.
    drop(held.pop());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match f.services.open_window(Id(5), gen0, browser.clone()) {
            Ok(_) => break,
            Err(ErrorCode::Backpressure) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => panic!("unexpected open_window result: {:?}", other.err()),
        }
    }

    drop(held);
    f.services.stop();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !f.services.is_quiescent() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(f.ledger.used(None, Resource::TempBytes), 0);
    assert_eq!(f.ledger.used(None, Resource::Handle), 0);
}

#[test]
fn live_retired_artifact_never_blocks_reaping_subsequent_closed_windows() {
    let f = Fixture::new();
    let gen0 = Generation::default();
    let browser: BrowserOpen = Arc::new(|_, _, _| Err(ErrorCode::Denied));

    let w1_files = f
        .services
        .open_window(Id(1), gen0, browser.clone())
        .unwrap();
    let lut = create_live_artifact(&f, &w1_files, Id(1), gen0);
    f.services.window_changed(Id(1), None);
    assert!(!f.services.is_quiescent());

    // Open and close 16 sequential windows while retired Window 1 holds a live artifact; none may wedge reaping.
    for id in 2..=17 {
        f.services
            .open_window(Id(id), gen0, browser.clone())
            .unwrap();
        f.services.window_changed(Id(id), None);
    }

    assert!(!f.services.is_quiescent());
    drop((lut, w1_files));
    f.services.stop();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !f.services.is_quiescent() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(f.ledger.used(None, Resource::TempBytes), 0);
    assert_eq!(f.ledger.used(None, Resource::Handle), 0);
}
