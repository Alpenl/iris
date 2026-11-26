use super::{Storage, VALUE_BYTES, check, validate_entry};
use crate::{
    plugin_v2::{
        actor::{Executor, IoOutcome, Operation, TaskContext},
        instance::Blob,
        ledger::{Activity, Resource},
        network::NetworkGrant,
    },
    vfs::HttpCancel,
};
use iris_api::v2::{
    Capability, ErrorCode, Generation, Id, Role,
    task::{CredentialBucket, Task},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

struct BindingState {
    instance: Option<Id>,
    global_generation: Option<Generation>,
    windows: BTreeMap<Id, (Generation, HttpCancel)>,
}

struct Inner {
    store: Arc<Storage>,
    network: NetworkGrant,
    capabilities: BTreeSet<Capability>,
    activity: Activity,
    cancel: HttpCancel,
    binding: Mutex<BindingState>,
}
#[derive(Clone)]
pub struct StorageExecutor(Arc<Inner>);
impl StorageExecutor {
    pub fn new(
        store: Arc<Storage>,
        network: NetworkGrant,
        capabilities: BTreeSet<Capability>,
    ) -> Result<Self, ErrorCode> {
        if store.owner() != network.owner() {
            return Err(ErrorCode::Denied);
        }
        Ok(Self(Arc::new(Inner {
            store,
            network,
            capabilities,
            activity: Activity::default(),
            cancel: HttpCancel::default(),
            binding: Mutex::new(BindingState {
                instance: None,
                global_generation: None,
                windows: BTreeMap::new(),
            }),
        })))
    }
    fn credential_key(&self, bucket: &CredentialBucket, key: &str) -> Result<String, ErrorCode> {
        validate_entry(key, None, 2048)?;
        let prefix = match bucket {
            CredentialBucket::Plugin => "p".into(),
            CredentialBucket::Origin { origin_grant } => {
                let origin = self.0.network.confirmed_origin(*origin_grant)?.display();
                let digest = ring::digest::digest(&ring::digest::SHA256, origin.as_bytes());
                format!(
                    "o.{}",
                    digest
                        .as_ref()
                        .iter()
                        .map(|v| format!("{v:02x}"))
                        .collect::<String>()
                )
            }
        };
        Ok(format!("{prefix}.{key}"))
    }
    fn execute(&self, task: Task, context: &TaskContext) -> Result<IoOutcome, ErrorCode> {
        check(&context.cancel)?;
        let store = &self.0.store;
        let value = match task {
            Task::StateRead { key } => {
                let (value, revision) = store.read_state(&key, &context.cancel)?;
                serde_json::json!({"store_value":value,"revision":revision})
            }
            Task::StateWrite { key, value } => {
                let revision = store.write_state(&key, value.as_deref(), &context.cancel)?;
                serde_json::json!({"stored":true,"revision":revision})
            }
            Task::SecretRead { bucket, key } => {
                let key = self.credential_key(&bucket, &key)?;
                let value = store
                    .vault
                    .try_get_secret(&store.scope(), &key)
                    .map_err(|e| vault_error_code(&e))?;
                // Revocation while waiting on the OS vault must not deliver data.
                if let CredentialBucket::Origin { origin_grant } = &bucket {
                    self.0.network.confirmed_origin(*origin_grant)?;
                }
                check(&context.cancel)?;
                if value.as_ref().is_some_and(|v| v.len() > 2048) {
                    return Err(ErrorCode::QuotaExceeded);
                }
                serde_json::json!({"store_value":value,"revision":null})
            }
            Task::SecretWrite { bucket, key, value } => {
                let key = self.credential_key(&bucket, &key)?;
                check(&context.cancel)?;
                store
                    .vault
                    .set_secret(&store.scope(), &key, value.as_deref())
                    .map_err(|e| vault_error_code(&e))?;
                serde_json::json!({"stored":true,"revision":null})
            }
            _ => return Err(ErrorCode::Unsupported),
        };
        Ok(IoOutcome::new(value, None))
    }
}

fn vault_error_code(error: &anyhow::Error) -> ErrorCode {
    if let Some(typed) = error.downcast_ref::<crate::vault::VaultError>() {
        return match typed.kind {
            crate::vault::VaultErrorKind::QuotaExceeded => ErrorCode::QuotaExceeded,
            crate::vault::VaultErrorKind::Busy => ErrorCode::Backpressure,
            crate::vault::VaultErrorKind::InvalidArgument => ErrorCode::InvalidArgument,
            crate::vault::VaultErrorKind::Backend => ErrorCode::IoError,
        };
    }
    if error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io_err| io_err.kind() == std::io::ErrorKind::WouldBlock)
    {
        return ErrorCode::Backpressure;
    }
    ErrorCode::IoError
}
impl Executor for StorageExecutor {
    fn prepare(
        &self,
        task: Task,
        _: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        if context.role != Role::Control
            || context.owner != self.0.store.owner
            || !task.authorized(&self.0.capabilities)
        {
            return Err(ErrorCode::Denied);
        }
        match &task {
            Task::StateRead { key } => validate_entry(key, None, VALUE_BYTES)?,
            Task::StateWrite { key, value } => validate_entry(key, value.as_deref(), VALUE_BYTES)?,
            Task::SecretRead { bucket, key } => {
                self.credential_key(bucket, key)?;
            }
            Task::SecretWrite { bucket, key, value } => {
                validate_entry(key, value.as_deref(), 2048)?;
                self.credential_key(bucket, key)?;
            }
            _ => return Err(ErrorCode::Unsupported),
        }
        check(&context.cancel)?;
        self.0
            .cancel
            .link(&context.cancel)
            .map_err(|_| ErrorCode::Cancelled)?;
        let mut binding = self.0.binding.lock().unwrap_or_else(|e| e.into_inner());
        if binding.instance.is_some_and(|id| id != context.instance_id) {
            return Err(ErrorCode::Denied);
        }
        binding.instance = Some(context.instance_id);
        match context.window {
            None => {
                if binding
                    .global_generation
                    .is_some_and(|g| g != context.generation)
                {
                    return Err(ErrorCode::StaleHandle);
                }
                binding.global_generation = Some(context.generation);
            }
            Some(win) => {
                let entry = binding
                    .windows
                    .entry(win)
                    .or_insert_with(|| (context.generation, HttpCancel::default()));
                if entry.0 != context.generation {
                    return Err(ErrorCode::StaleHandle);
                }
                entry
                    .1
                    .link(&context.cancel)
                    .map_err(|_| ErrorCode::Cancelled)?;
            }
        }
        drop(binding);
        let ticket = self.0.store.serial.enter()?;
        let lease = self
            .0
            .store
            .ledger
            .reserve(&self.0.store.owner, &[(Resource::HostBytes, 2 << 20)])?;
        let activity = self.0.activity.start();
        let this = self.clone();
        Ok(Operation::blocking(move || {
            let _lifetime = (lease, activity);
            ticket.wait(&context.cancel)?;
            let _ticket = ticket;
            this.execute(task, &context)
        }))
    }
    fn close(&self, _: u64) -> Result<(), ErrorCode> {
        Err(ErrorCode::StaleHandle)
    }
    fn stop(&self) {
        self.0.cancel.cancel();
        let windows = std::mem::take(
            &mut self
                .0
                .binding
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .windows,
        );
        for (_, (_, cancel)) in windows {
            cancel.cancel();
        }
    }
    fn generation_changed(&self, generation: Generation) {
        self.0
            .binding
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .global_generation = Some(generation);
    }
    fn window_changed(&self, window: Id, generation: Option<Generation>) {
        let cancel = {
            let mut binding = self.0.binding.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(generation) = generation {
                binding
                    .windows
                    .insert(window, (generation, HttpCancel::default()))
                    .map(|(_, c)| c)
            } else {
                binding.windows.remove(&window).map(|(_, c)| c)
            }
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }
    fn is_quiescent(&self) -> bool {
        self.0.activity.is_idle()
    }
}
