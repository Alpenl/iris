//! Installation-owned state and exact credential buckets. All disk/vault work
//! runs in bounded tasks; a failed read is never treated as a missing database.
mod executor;
mod serial;
use super::{
    installation::valid_identity,
    ledger::{Ledger, Resource},
};
use crate::plugin_v2::json;
use crate::{fs_util, vault::CredentialVault, vfs::HttpCancel};
pub use executor::StorageExecutor;
use iris_api::v2::{ErrorCode, Id};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{ErrorKind, Read},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

const STATE_BYTES: usize = 256 << 10;
const VALUE_BYTES: usize = 32 << 10;
const STATE_KEYS: usize = 128;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    installation: String,
    revision: Id,
    entries: BTreeMap<String, String>,
}
pub struct Storage {
    owner: String,
    path: PathBuf,
    vault: CredentialVault,
    ledger: Ledger,
    serial: Arc<serial::Serial>,
}
impl Storage {
    pub fn new(
        root: PathBuf,
        installation: String,
        vault: CredentialVault,
        ledger: Ledger,
    ) -> Result<Self, ErrorCode> {
        if !valid_identity(&installation) {
            return Err(ErrorCode::InvalidArgument);
        }
        Ok(Self {
            path: root.join(format!("{installation}.json")),
            owner: installation,
            vault,
            ledger,
            serial: Arc::new(serial::Serial::default()),
        })
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    fn scope(&self) -> String {
        format!("plugin-v2.{}", self.owner)
    }
    fn read(&self) -> Result<Record, ErrorCode> {
        let file = match fs_util::retry_sharing_violation(|| std::fs::File::open(&self.path)) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(Record {
                    schema: 1,
                    installation: self.owner.clone(),
                    revision: Id(0),
                    entries: BTreeMap::new(),
                });
            }
            Err(_) => return Err(ErrorCode::IoError),
        };
        let mut bytes = Vec::new();
        file.take(STATE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ErrorCode::IoError)?;
        let record: Record = json::decode(&bytes, STATE_BYTES)?;
        if record.schema != 1
            || record.installation != self.owner
            || record.entries.len() > STATE_KEYS
        {
            return Err(ErrorCode::InvalidArgument);
        }
        for (key, value) in &record.entries {
            validate_entry(key, Some(value), VALUE_BYTES)?;
        }
        Ok(record)
    }
    /// Host migration/startup workers may use the same typed operations as the
    /// guest executor. No UI or Wasm import may call these blocking methods.
    pub fn read_state(
        &self,
        key: &str,
        cancel: &HttpCancel,
    ) -> Result<(Option<String>, Id), ErrorCode> {
        validate_entry(key, None, VALUE_BYTES)?;
        let _memory = self
            .ledger
            .reserve(&self.owner, &[(Resource::HostBytes, 4 << 20)])?;
        check(cancel)?;
        let record = self.read()?;
        check(cancel)?;
        Ok((record.entries.get(key).cloned(), record.revision))
    }
    pub fn write_state(
        &self,
        key: &str,
        value: Option<&str>,
        cancel: &HttpCancel,
    ) -> Result<Id, ErrorCode> {
        validate_entry(key, value, VALUE_BYTES)?;
        let _memory = self
            .ledger
            .reserve(&self.owner, &[(Resource::HostBytes, 4 << 20)])?;
        check(cancel)?;
        let _lock = fs_util::lock_file_with_timeout(
            &self.path.with_extension("lock"),
            Duration::from_millis(250),
        )
        .map_err(|e| {
            if e.kind() == ErrorKind::WouldBlock {
                ErrorCode::Backpressure
            } else {
                ErrorCode::IoError
            }
        })?;
        let mut record = self.read()?;
        match value {
            Some(value) => {
                record.entries.insert(key.into(), value.into());
            }
            None => {
                record.entries.remove(key);
            }
        }
        if record.entries.len() > STATE_KEYS {
            return Err(ErrorCode::QuotaExceeded);
        }
        record.revision = Id(record
            .revision
            .0
            .checked_add(1)
            .ok_or(ErrorCode::QuotaExceeded)?);
        let bytes = json::encode(&record, STATE_BYTES)?;
        check(cancel)?;
        fs_util::atomic_write_private(&self.path, bytes).map_err(|_| ErrorCode::IoError)?;
        // A cancel racing the atomic commit cannot undo a committed write.
        // The caller can read the current revision before retrying an edit.
        Ok(record.revision)
    }
}
fn validate_entry(key: &str, value: Option<&str>, max: usize) -> Result<(), ErrorCode> {
    if !iris_api::is_valid_id(key) || key.len() > 128 {
        return Err(ErrorCode::InvalidArgument);
    }
    if value.is_some_and(|v| v.len() > max || v.contains('\0')) {
        return Err(ErrorCode::QuotaExceeded);
    }
    if let Some(value) = value {
        json::encode(&value, iris_api::v2::MESSAGE_LIMIT - 4096)?;
    }
    Ok(())
}
fn check(cancel: &HttpCancel) -> Result<(), ErrorCode> {
    if cancel.is_cancelled() {
        Err(ErrorCode::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
