use super::*;
use crate::actor::{Executor, Operation, TaskContext};
use crate::instance::next_id;
use crate::network::{NetworkGrant, TransportKind};
use iris_api::v2::{
    Capability, Generation, NetworkDeclaration, Role,
    task::{CredentialBucket, Task},
};
use std::{collections::BTreeSet, sync::Arc};
const OWNER: &str = "0123456789abcdef0123456789abcdef";
struct Fixture {
    root: PathBuf,
    storage: Arc<Storage>,
    grant: NetworkGrant,
    executor: StorageExecutor,
    ledger: Ledger,
    vault: CredentialVault,
}
impl Fixture {
    fn new(capabilities: BTreeSet<Capability>) -> Self {
        let root = std::env::temp_dir().join(format!(
            "iris-v2-store-{}-{}",
            std::process::id(),
            next_id().unwrap()
        ));
        let ledger = Ledger::default();
        let vault = CredentialVault::in_memory();
        let storage = Arc::new(
            Storage::new(root.clone(), OWNER.into(), vault.clone(), ledger.clone()).unwrap(),
        );
        let grant = NetworkGrant::new(
            OWNER.into(),
            capabilities.clone(),
            NetworkDeclaration::default(),
            &ledger,
        )
        .unwrap();
        let executor = StorageExecutor::new(storage.clone(), grant.clone(), capabilities).unwrap();
        Self {
            root,
            storage,
            grant,
            executor,
            ledger,
            vault,
        }
    }
    fn context(&self) -> TaskContext {
        TaskContext {
            instance_id: Id(1),
            window: Some(Id(1)),
            owner: OWNER.into(),
            role: Role::Control,
            generation: Generation::default(),
            cancel: HttpCancel::default(),
            ledger: self.ledger.clone(),
        }
    }
    fn run(&self, task: Task) -> Result<serde_json::Value, ErrorCode> {
        let Operation::Blocking(work) =
            self.executor
                .prepare(task, &BTreeMap::new(), self.context())?
        else {
            panic!("storage must run on worker");
        };
        work().map(|value| value.value)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.executor.stop();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn state_updates_merge_latest_revision_and_reject_corruption_without_overwrite() {
    let f = Fixture::new(BTreeSet::new());
    let baseline = f.ledger.used(None, Resource::HostBytes);
    let peer = Arc::new(
        Storage::new(
            f.root.clone(),
            OWNER.into(),
            f.vault.clone(),
            f.ledger.clone(),
        )
        .unwrap(),
    );
    let barrier = Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        for (store, key) in [(f.storage.clone(), "left"), (peer.clone(), "right")] {
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                store
                    .write_state(key, Some(key), &HttpCancel::default())
                    .unwrap();
            });
        }
    });
    let (value, revision) = peer.read_state("left", &HttpCancel::default()).unwrap();
    assert_eq!(value.as_deref(), Some("left"));
    assert_eq!(revision, Id(2));
    assert_eq!(
        peer.read_state("right", &HttpCancel::default())
            .unwrap()
            .0
            .as_deref(),
        Some("right")
    );
    let bytes = std::fs::read(&f.storage.path).unwrap();
    let cancel = HttpCancel::default();
    cancel.cancel();
    assert_eq!(
        peer.write_state("right", None, &cancel),
        Err(ErrorCode::Cancelled)
    );
    assert_eq!(std::fs::read(&f.storage.path).unwrap(), bytes);
    std::fs::write(&f.storage.path, b"corrupt").unwrap();
    assert!(
        peer.write_state("left", Some("lost"), &HttpCancel::default())
            .is_err()
    );
    assert_eq!(std::fs::read(&f.storage.path).unwrap(), b"corrupt");
    let mut wrong: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    wrong["installation"] = serde_json::json!("ffffffffffffffffffffffffffffffff");
    std::fs::write(&f.storage.path, serde_json::to_vec(&wrong).unwrap()).unwrap();
    assert!(peer.read_state("left", &HttpCancel::default()).is_err());
    assert_eq!(f.ledger.used(None, Resource::HostBytes), baseline);
}

#[test]
fn state_limit_failures_preserve_prior_record_and_serialized_values_are_readable() {
    let f = Fixture::new(BTreeSet::new());
    let text = "x".repeat(VALUE_BYTES);
    for i in 0..7 {
        f.storage
            .write_state(&format!("key.{i}"), Some(&text), &HttpCancel::default())
            .unwrap();
    }
    let bytes = std::fs::read(&f.storage.path).unwrap();
    assert_eq!(
        f.storage
            .write_state("overflow", Some(&text), &HttpCancel::default()),
        Err(ErrorCode::QuotaExceeded)
    );
    assert_eq!(std::fs::read(&f.storage.path).unwrap(), bytes);
    assert_eq!(
        f.storage.write_state(
            "escaped",
            Some(&"\u{0001}".repeat(20000)),
            &HttpCancel::default()
        ),
        Err(ErrorCode::QuotaExceeded)
    );
    let result = f
        .run(Task::StateRead {
            key: "key.0".into(),
        })
        .unwrap();
    assert_eq!(result["store_value"], text);
    assert!(iris_core::bounded_json::encode(&result, iris_api::v2::MESSAGE_LIMIT - 1024).is_ok());
}

#[test]
fn credential_buckets_and_installation_identity_never_alias() {
    let f = Fixture::new(BTreeSet::from([
        Capability::MediaSource,
        Capability::NetHttp,
    ]));
    let origin = f
        .grant
        .confirm_origin(
            "https://example.com",
            TransportKind::Http,
            &["93.184.216.34".parse().unwrap()],
        )
        .unwrap();
    let other = f
        .grant
        .confirm_origin(
            "https://example.org",
            TransportKind::Http,
            &["93.184.216.34".parse().unwrap()],
        )
        .unwrap();
    let bucket = CredentialBucket::Origin {
        origin_grant: origin,
    };
    f.run(Task::SecretWrite {
        bucket: bucket.clone(),
        key: "token".into(),
        value: Some("private-token".into()),
    })
    .unwrap();
    assert!(
        !f.storage.path.exists(),
        "secrets must never enter the state database"
    );
    for empty in [
        CredentialBucket::Plugin,
        CredentialBucket::Origin {
            origin_grant: other,
        },
    ] {
        assert!(
            f.run(Task::SecretRead {
                bucket: empty,
                key: "token".into()
            })
            .unwrap()["store_value"]
                .is_null()
        );
    }
    assert_eq!(
        f.run(Task::SecretRead {
            bucket: bucket.clone(),
            key: "token".into()
        })
        .unwrap()["store_value"],
        "private-token"
    );
    // A different host grant for the same canonical Origin reaches the same
    // bucket; a different installation never reaches this vault scope.
    let same = f
        .grant
        .confirm_origin(
            "https://EXAMPLE.com:443",
            TransportKind::Http,
            &["93.184.216.34".parse().unwrap()],
        )
        .unwrap();
    assert_eq!(
        f.run(Task::SecretRead {
            bucket: CredentialBucket::Origin { origin_grant: same },
            key: "token".into()
        })
        .unwrap()["store_value"],
        "private-token"
    );
    let saved = f.vault.list_scope(&f.storage.scope());
    assert_eq!(saved.len(), 1);
    let replacement = Storage::new(
        f.root.clone(),
        "ffffffffffffffffffffffffffffffff".into(),
        f.vault.clone(),
        f.ledger.clone(),
    )
    .unwrap();
    assert!(
        f.vault
            .try_get_secret(&replacement.scope(), saved.keys().next().unwrap())
            .unwrap()
            .is_none()
    );
    // Reconfirmation already retired the old handle, while retaining the
    // canonical credential bucket behind the new live grant.
    assert_eq!(f.grant.revoke_origin(origin), Err(ErrorCode::StaleHandle));
    f.grant.revoke_origin(same).unwrap();
    assert_eq!(
        f.run(Task::SecretRead {
            bucket,
            key: "token".into()
        }),
        Err(ErrorCode::StaleHandle)
    );
    assert_eq!(
        f.run(Task::SecretRead {
            bucket: CredentialBucket::Origin { origin_grant: same },
            key: "token".into(),
        }),
        Err(ErrorCode::StaleHandle)
    );
    let denied = Fixture::new(BTreeSet::new());
    assert_eq!(
        denied.run(Task::SecretRead {
            bucket: CredentialBucket::Plugin,
            key: "token".into()
        }),
        Err(ErrorCode::Denied)
    );
}

#[test]
fn queued_storage_work_holds_capacity_and_cancel_does_not_write() {
    let f = Fixture::new(BTreeSet::new());
    let baseline = f.ledger.used(None, Resource::HostBytes);
    let task = Task::StateWrite {
        key: "pending".into(),
        value: Some("must-not-commit".into()),
    };
    let Operation::Blocking(work) = f
        .executor
        .prepare(task.clone(), &BTreeMap::new(), f.context())
        .unwrap()
    else {
        panic!();
    };
    assert!(!f.executor.is_quiescent());
    assert_eq!(
        f.ledger.used(None, Resource::HostBytes),
        baseline + (2 << 20)
    );
    let queued: Vec<_> = (0..7)
        .map(|_| {
            f.executor
                .prepare(task.clone(), &BTreeMap::new(), f.context())
                .unwrap()
        })
        .collect();
    assert!(matches!(
        f.executor
            .prepare(task.clone(), &BTreeMap::new(), f.context()),
        Err(ErrorCode::Backpressure)
    ));
    f.executor.stop();
    assert!(!f.executor.is_quiescent());
    assert!(matches!(work(), Err(ErrorCode::Cancelled)));
    for operation in queued {
        let Operation::Blocking(work) = operation else {
            panic!()
        };
        assert!(matches!(work(), Err(ErrorCode::Cancelled)));
    }
    assert!(!f.storage.path.exists());
    assert!(f.executor.is_quiescent());
    assert_eq!(f.ledger.used(None, Resource::HostBytes), baseline);
    let mut context = f.context();
    context.role = Role::Stream;
    assert!(matches!(
        f.executor.prepare(task, &BTreeMap::new(), context),
        Err(ErrorCode::Denied)
    ));
}
