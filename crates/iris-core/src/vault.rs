//! Cross-platform credential vault backed by macOS Keychain and Windows
//! Credential Manager (fail-closed, zero plaintext disk fallback), plus a
//! thread-safe in-memory backend for deterministic unit and integration tests.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use crate::fs_util;

/// Maximum secret payload size in bytes (strictly below Windows `CRED_MAX_CREDENTIAL_BLOB_SIZE = 2560`).
pub const MAX_SECRET_BYTES: usize = 2048;
/// Maximum stored credential keys per plugin/source scope.
pub const MAX_SECRETS_PER_SCOPE: usize = 64;

const MAX_KEY_BYTES: usize = 256;
const VAULT_WRITE_LOCK_TIMEOUT: Duration = Duration::from_millis(1200);
const VAULT_READ_LOCK_TIMEOUT: Duration = Duration::from_millis(250);

type ScopedSecrets = BTreeMap<String, BTreeMap<String, String>>;
type CredentialIndex = BTreeMap<String, BTreeSet<String>>;

#[derive(Clone, Debug)]
enum VaultBackend {
    System,
    #[cfg(any(debug_assertions, feature = "devtools"))]
    TestFile(PathBuf),
    InMemory(Arc<Mutex<ScopedSecrets>>),
}

/// Cross-platform secret store for `MediaSource` and authenticated plugins.
#[derive(Clone, Debug)]
pub struct CredentialVault {
    backend: VaultBackend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultErrorKind {
    QuotaExceeded,
    Busy,
    InvalidArgument,
    Backend,
}

#[derive(Debug)]
pub struct VaultError {
    pub kind: VaultErrorKind,
    pub message: String,
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for VaultError {}

fn vault_err(kind: VaultErrorKind, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(VaultError {
        kind,
        message: message.into(),
    })
}

fn validate_id_part<'a>(label: &str, val: &'a str) -> Result<&'a str> {
    let trimmed = val.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_KEY_BYTES {
        return Err(vault_err(
            VaultErrorKind::InvalidArgument,
            format!("invalid credential {label}"),
        ));
    }
    if trimmed.bytes().any(|b| matches!(b, b'\0' | b'\r' | b'\n')) {
        return Err(vault_err(
            VaultErrorKind::InvalidArgument,
            format!("credential {label} contains control characters"),
        ));
    }
    Ok(trimmed)
}

fn read_json_map<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs_util::retry_sharing_violation(|| std::fs::read_to_string(path)) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("invalid vault file {}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("failed reading vault file {}", path.display())),
    }
}

fn vault_lock(path: &Path) -> Result<std::fs::File> {
    fs_util::lock_file_with_timeout(path, VAULT_WRITE_LOCK_TIMEOUT)
        .context("could not lock credential vault")
}

fn try_vault_read_lock(path: &Path) -> Result<Option<std::fs::File>> {
    match fs_util::lock_file_shared_with_timeout(path, VAULT_READ_LOCK_TIMEOUT) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e).context("could not lock credential vault"),
    }
}

#[cfg(any(debug_assertions, feature = "devtools"))]
fn test_file_lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

#[cfg(any(debug_assertions, feature = "devtools"))]
fn test_file_lock(path: &Path) -> Result<std::fs::File> {
    vault_lock(&test_file_lock_path(path))
}

fn log_read_error(error: anyhow::Error) {
    log::warn!("reading credential vault failed: {error:#}");
}

fn system_dir() -> Result<PathBuf> {
    fs_util::data_dir().ok_or_else(|| anyhow!("application data dir unavailable"))
}

fn write_json_private<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value).context("failed serializing vault JSON")?;
    fs_util::atomic_write_private(path, json.as_bytes())
        .with_context(|| format!("failed writing vault file {}", path.display()))
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join("credential_index.json")
}

fn update_index(index: &mut CredentialIndex, scope: &str, key: &str, present: bool) -> bool {
    if present {
        if index.get(scope).is_some_and(|keys| keys.contains(key)) {
            return false;
        }
        index.entry(scope.into()).or_default().insert(key.into());
    } else if let Some(keys) = index.get_mut(scope) {
        if !keys.remove(key) {
            return false;
        }
        if keys.is_empty() {
            index.remove(scope);
        }
    } else {
        return false;
    }
    true
}

#[cfg(any(windows, test))]
fn windows_target_name(scope: &str, key: &str) -> Vec<u16> {
    // Credential Manager compares TargetName case-insensitively. Encode the
    // original UTF-8 bytes in lowercase hex in a namespace disjoint from v1.
    const PREFIX: &str = "app.iris.vault.v2:";
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut target = Vec::with_capacity(PREFIX.len() + 2 * (scope.len() + key.len()) + 2);
    target.extend(PREFIX.encode_utf16());
    for (part, delimiter) in [(scope, u16::from(b'/')), (key, 0)] {
        for byte in part.bytes() {
            target.push(u16::from(HEX[usize::from(byte >> 4)]));
            target.push(u16::from(HEX[usize::from(byte & 15)]));
        }
        target.push(delimiter);
    }
    target
}

#[cfg(target_os = "macos")]
mod os_store {
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
    fn service_name(scope: &str) -> String {
        format!("app.iris.player.{scope}")
    }

    pub fn set(scope: &str, key: &str, secret: &str) -> Result<(), String> {
        set_generic_password(&service_name(scope), key, secret.as_bytes())
            .map_err(|e| e.to_string())
    }

    pub fn get(scope: &str, key: &str) -> Result<Option<String>, String> {
        match get_generic_password(&service_name(scope), key) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|e| e.to_string()),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn delete(scope: &str, key: &str) -> Result<(), String> {
        match delete_generic_password(&service_name(scope), key) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(target_os = "windows")]
mod os_store {
    use core::ffi::c_void;
    use std::io;
    use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
    use windows_sys::Win32::Security::Credentials::{
        CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW,
        CredDeleteW, CredFree, CredReadW, CredWriteW,
    };

    fn legacy_target(scope: &str, key: &str) -> Vec<u16> {
        format!("app.iris.player.{scope}/{key}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn set(scope: &str, key: &str, secret: &str) -> Result<(), String> {
        let bytes = secret.as_bytes();
        if bytes.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
            return Err("secret exceeds Windows CredentialBlob size limit".into());
        }
        let mut target = super::windows_target_name(scope, key);
        let mut user: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
        let cred = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: std::ptr::null_mut(),
            LastWritten: Default::default(),
            CredentialBlobSize: bytes.len() as u32,
            CredentialBlob: bytes.as_ptr().cast_mut(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: user.as_mut_ptr(),
        };
        let ok = unsafe { CredWriteW(&cred, 0) };
        if ok == 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        // New writes retire the exact legacy target, preventing a later fallback
        // from resurrecting the previous value. Other legacy owners are untouched.
        delete_legacy(scope, key)
    }

    fn read<T>(
        target: &[u16],
        inspect: impl FnOnce(&CREDENTIALW) -> T,
    ) -> Result<Option<T>, String> {
        let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
        let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if ok == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
                Ok(None)
            } else {
                Err(error.to_string())
            };
        }
        if credential.is_null() {
            return Err("CredReadW returned a null credential".into());
        }
        let value = inspect(unsafe { &*credential });
        unsafe { CredFree(credential as *const c_void) };
        Ok(Some(value))
    }

    fn utf16_matches(actual: *const u16, expected: impl Iterator<Item = u16>) -> bool {
        if actual.is_null() {
            return false;
        }
        // Both expected strings include their terminator. A shorter actual
        // string first differs at its terminator, before advancing past it.
        for (index, unit) in expected.enumerate() {
            if unsafe { *actual.add(index) } != unit {
                return false;
            }
        }
        true
    }

    fn owned_legacy(cred: &CREDENTIALW, target: &[u16], key: &str) -> bool {
        utf16_matches(cred.TargetName, target.iter().copied())
            && utf16_matches(cred.UserName, key.encode_utf16().chain(std::iter::once(0)))
    }

    fn secret(cred: &CREDENTIALW) -> Result<String, String> {
        let bytes = if cred.CredentialBlobSize == 0 {
            &[][..]
        } else if cred.CredentialBlob.is_null() {
            return Err("CredReadW returned an invalid credential blob".into());
        } else {
            unsafe {
                std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize)
            }
        };
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|e| e.to_string())
    }

    pub fn get(scope: &str, key: &str) -> Result<Option<String>, String> {
        if let Some(value) = read(&super::windows_target_name(scope, key), secret)? {
            return value.map(Some);
        }
        let target = legacy_target(scope, key);
        read(&target, |cred| {
            owned_legacy(cred, &target, key).then(|| secret(cred))
        })?
        .flatten()
        .transpose()
    }

    fn delete_target(target: &[u16]) -> Result<(), String> {
        let ok = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
            Ok(())
        } else {
            Err(error.to_string())
        }
    }

    fn delete_legacy(scope: &str, key: &str) -> Result<(), String> {
        let target = legacy_target(scope, key);
        // Both stored fields must match exactly: the target verifies casing,
        // and the username verifies the boundary between scope and key.
        if read(&target, |cred| owned_legacy(cred, &target, key))? == Some(true) {
            delete_target(&target)?;
        }
        Ok(())
    }

    pub fn delete(scope: &str, key: &str) -> Result<(), String> {
        delete_legacy(scope, key)?;
        delete_target(&super::windows_target_name(scope, key))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod os_store {
    pub fn set(_scope: &str, _key: &str, _secret: &str) -> Result<(), String> {
        Err("native OS credential store unavailable on this platform".into())
    }

    pub fn get(_scope: &str, _key: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    pub fn delete(_scope: &str, _key: &str) -> Result<(), String> {
        Err("native OS credential store unavailable on this platform".into())
    }
}

impl CredentialVault {
    /// Creates a credential vault backed by the OS credential store (macOS Keychain or
    /// Windows Credential Manager). Fails closed on error and never writes plaintext
    /// fallback files.
    ///
    /// In debug or `devtools` builds only, setting `IRIS_TEST_VAULT=/path/to/file.json`
    /// redirects storage to a `0600` test-isolated file for external automation scripts.
    pub fn system() -> Self {
        #[cfg(any(debug_assertions, feature = "devtools"))]
        if let Some(p) = std::env::var_os("IRIS_TEST_VAULT")
            && !p.is_empty()
        {
            return Self {
                backend: VaultBackend::TestFile(PathBuf::from(p)),
            };
        }
        Self {
            backend: VaultBackend::System,
        }
    }

    /// Creates a pure in-memory, thread-safe credential vault for unit and integration tests.
    pub fn in_memory() -> Self {
        Self {
            backend: VaultBackend::InMemory(Arc::new(Mutex::new(BTreeMap::new()))),
        }
    }

    /// Retrieves a stored secret for `(scope, key)`.
    pub fn get_secret(&self, scope: &str, key: &str) -> Option<String> {
        let scope = validate_id_part("scope", scope).ok()?;
        let key = validate_id_part("key", key).ok()?;
        let found = match &self.backend {
            VaultBackend::InMemory(store) => {
                let guard = store.lock().unwrap_or_else(|e| e.into_inner());
                guard.get(scope)?.get(key).cloned()
            }
            #[cfg(any(debug_assertions, feature = "devtools"))]
            VaultBackend::TestFile(path) => {
                let read = || -> Result<Option<String>> {
                    let _lock = try_vault_read_lock(&test_file_lock_path(path))?;
                    let secrets: ScopedSecrets = read_json_map(path)?;
                    Ok(secrets.get(scope).and_then(|keys| keys.get(key)).cloned())
                };
                read().unwrap_or_else(|e| {
                    log_read_error(e);
                    None
                })
            }
            VaultBackend::System => {
                let read = || -> Result<Option<String>> {
                    let _lock = try_vault_read_lock(&system_dir()?.join("vault.lock"))?;
                    os_store::get(scope, key).map_err(anyhow::Error::msg)
                };
                read().unwrap_or_else(|e| {
                    log_read_error(e);
                    None
                })
            }
        };
        if let Some(secret) = &found {
            crate::logging::record_secret_value(secret);
        }
        found
    }

    /// Strict reads distinguish an absent key from a locked or unavailable
    /// vault. Callers must run this on a background worker.
    pub fn try_get_secret(&self, scope: &str, key: &str) -> Result<Option<String>> {
        let scope = validate_id_part("scope", scope)?;
        let key = validate_id_part("key", key)?;
        let found = match &self.backend {
            VaultBackend::InMemory(store) => Ok(store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(scope)
                .and_then(|entries| entries.get(key))
                .cloned()),
            #[cfg(any(debug_assertions, feature = "devtools"))]
            VaultBackend::TestFile(path) => {
                let _lock = try_vault_read_lock(&test_file_lock_path(path))?
                    .ok_or_else(|| vault_err(VaultErrorKind::Busy, "credential vault is busy"))?;
                let secrets: ScopedSecrets = read_json_map(path)?;
                Ok(secrets
                    .get(scope)
                    .and_then(|entries| entries.get(key))
                    .cloned())
            }
            VaultBackend::System => {
                let _lock = try_vault_read_lock(&system_dir()?.join("vault.lock"))?
                    .ok_or_else(|| vault_err(VaultErrorKind::Busy, "credential vault is busy"))?;
                os_store::get(scope, key).map_err(|e| vault_err(VaultErrorKind::Backend, e))
            }
        }?;
        if let Some(secret) = &found {
            crate::logging::record_secret_value(secret);
        }
        Ok(found)
    }

    /// Stores or deletes (`secret = None`) a secret under `(scope, key)`.
    pub fn set_secret(&self, scope: &str, key: &str, secret: Option<&str>) -> Result<()> {
        let scope = validate_id_part("scope", scope)?;
        let key = validate_id_part("key", key)?;
        if let Some(s) = secret {
            if s.len() > MAX_SECRET_BYTES {
                return Err(vault_err(
                    VaultErrorKind::QuotaExceeded,
                    format!("secret exceeds maximum size ({MAX_SECRET_BYTES} bytes)"),
                ));
            }
            crate::logging::record_secret_value(s);
        }

        match &self.backend {
            VaultBackend::InMemory(store) => {
                let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
                if secret.is_some()
                    && guard
                        .get(scope)
                        .is_some_and(|m| !m.contains_key(key) && m.len() >= MAX_SECRETS_PER_SCOPE)
                {
                    return Err(vault_err(
                        VaultErrorKind::QuotaExceeded,
                        format!(
                            "credential scope '{scope}' exceeds maximum key count ({MAX_SECRETS_PER_SCOPE})"
                        ),
                    ));
                }
                apply_secret_edit(&mut guard, scope, key, secret);
                Ok(())
            }
            #[cfg(any(debug_assertions, feature = "devtools"))]
            VaultBackend::TestFile(path) => {
                let _lock = test_file_lock(path)?;
                let mut secrets: ScopedSecrets = read_json_map(path)?;
                if secret.is_some()
                    && secrets
                        .get(scope)
                        .is_some_and(|m| !m.contains_key(key) && m.len() >= MAX_SECRETS_PER_SCOPE)
                {
                    return Err(vault_err(
                        VaultErrorKind::QuotaExceeded,
                        format!(
                            "credential scope '{scope}' exceeds maximum key count ({MAX_SECRETS_PER_SCOPE})"
                        ),
                    ));
                }
                apply_secret_edit(&mut secrets, scope, key, secret);
                write_json_private(path, &secrets)
            }
            VaultBackend::System => {
                let dir = system_dir()?;
                let _lock = vault_lock(&dir.join("vault.lock"))?;
                let path = index_path(&dir);
                let mut index: CredentialIndex = read_json_map(&path)?;
                match secret {
                    Some(val) => {
                        if index
                            .get(scope)
                            .is_some_and(|k| !k.contains(key) && k.len() >= MAX_SECRETS_PER_SCOPE)
                        {
                            return Err(vault_err(
                                VaultErrorKind::QuotaExceeded,
                                format!(
                                    "credential scope '{scope}' exceeds maximum key count ({MAX_SECRETS_PER_SCOPE})"
                                ),
                            ));
                        }
                        // Reserve the index entry first so a crash cannot orphan an
                        // unindexed secret; if the OS write fails cleanly without
                        // creating the entry, roll back the reservation under lock.
                        let added = update_index(&mut index, scope, key, true);
                        if added {
                            write_json_private(&path, &index)?;
                        }
                        if let Err(err) = os_store::set(scope, key, val) {
                            if added
                                && matches!(os_store::get(scope, key), Ok(None))
                                && update_index(&mut index, scope, key, false)
                            {
                                let _ = write_json_private(&path, &index);
                            }
                            return Err(vault_err(VaultErrorKind::Backend, err));
                        }
                    }
                    None => {
                        os_store::delete(scope, key)
                            .map_err(|e| vault_err(VaultErrorKind::Backend, e))?;
                        if update_index(&mut index, scope, key, false) {
                            write_json_private(&path, &index)?;
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// Lists all stored `(key, secret)` pairs in `scope` (e.g. `"plugin.<id>"`).
    pub fn list_scope(&self, scope: &str) -> BTreeMap<String, String> {
        let Ok(scope) = validate_id_part("scope", scope) else {
            return BTreeMap::new();
        };
        let out = match &self.backend {
            VaultBackend::InMemory(store) => {
                let guard = store.lock().unwrap_or_else(|e| e.into_inner());
                guard.get(scope).cloned().unwrap_or_default()
            }
            #[cfg(any(debug_assertions, feature = "devtools"))]
            VaultBackend::TestFile(path) => {
                let read = || -> Result<BTreeMap<String, String>> {
                    let _lock = try_vault_read_lock(&test_file_lock_path(path))?;
                    let secrets: ScopedSecrets = read_json_map(path)?;
                    Ok(secrets.get(scope).cloned().unwrap_or_default())
                };
                read().unwrap_or_else(|e| {
                    log_read_error(e);
                    BTreeMap::new()
                })
            }
            VaultBackend::System => {
                let read = || -> Result<BTreeMap<String, String>> {
                    let dir = system_dir()?;
                    let _lock = try_vault_read_lock(&dir.join("vault.lock"))?;
                    let mut index: CredentialIndex = read_json_map(&index_path(&dir))?;
                    let mut out = BTreeMap::new();
                    for key in index.remove(scope).into_iter().flatten() {
                        match os_store::get(scope, &key) {
                            Ok(Some(val)) => {
                                out.insert(key, val);
                            }
                            Ok(None) => {}
                            Err(e) => log_read_error(anyhow::Error::msg(e)),
                        }
                    }
                    Ok(out)
                };
                read().unwrap_or_else(|e| {
                    log_read_error(e);
                    BTreeMap::new()
                })
            }
        };
        for secret in out.values() {
            crate::logging::record_secret_value(secret);
        }
        out
    }
}

fn apply_secret_edit(map: &mut ScopedSecrets, scope: &str, key: &str, secret: Option<&str>) {
    match secret {
        Some(val) => {
            map.entry(scope.to_string())
                .or_default()
                .insert(key.to_string(), val.to_string());
        }
        None => {
            if let Some(entries) = map.get_mut(scope) {
                entries.remove(key);
                if entries.is_empty() {
                    map.remove(scope);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(debug_assertions, feature = "devtools"))]
    use super::{
        CredentialIndex, VaultBackend, VaultError, VaultErrorKind, read_json_map, test_file_lock,
        test_file_lock_path, try_vault_read_lock,
    };
    use super::{CredentialVault, MAX_SECRET_BYTES, MAX_SECRETS_PER_SCOPE, windows_target_name};
    #[cfg(any(debug_assertions, feature = "devtools"))]
    use crate::test_util::TestDir;

    #[test]
    fn windows_credential_names_distinguish_case_and_scope_boundary() {
        assert_ne!(
            windows_target_name("ABC", "key"),
            windows_target_name("abc", "key")
        );
        assert_ne!(
            windows_target_name("a", "b/c"),
            windows_target_name("a/b", "c")
        );
        assert_ne!(
            windows_target_name("a", "密钥"),
            windows_target_name("a", "密碼")
        );
    }

    #[cfg(any(debug_assertions, feature = "devtools"))]
    #[test]
    fn vault_read_and_lock_failures_leave_existing_data_intact() {
        let dir = TestDir::new("vault-lock");
        let path = dir.join("vault.json");
        let vault = CredentialVault {
            backend: VaultBackend::TestFile(path.clone()),
        };
        assert!(read_json_map::<CredentialIndex>(&path).unwrap().is_empty());
        std::fs::write(&path, b"broken JSON").unwrap();
        assert!(vault.try_get_secret("scope", "key").is_err());
        assert!(
            vault
                .set_secret("scope", "key", Some("replacement"))
                .is_err()
        );
        assert!(vault.set_secret("scope", "key", None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"broken JSON");

        let previous = br#"{"scope":{"old_key":"saved"}}"#;
        std::fs::write(&path, previous).unwrap();

        // Shared read locks allow concurrent readers without hitting Busy.
        let shared = try_vault_read_lock(&test_file_lock_path(&path))
            .unwrap()
            .expect("shared read lock");
        assert_eq!(
            vault.try_get_secret("scope", "old_key").unwrap().as_deref(),
            Some("saved")
        );
        drop(shared);

        let lock = test_file_lock(&path).unwrap();
        // Exclusive writer blocks strict try_get_secret with Busy, while fallback get_secret reads snapshot.
        let busy_err = vault.try_get_secret("scope", "old_key").unwrap_err();
        assert_eq!(
            busy_err.downcast_ref::<VaultError>().map(|e| e.kind),
            Some(VaultErrorKind::Busy)
        );
        assert_eq!(
            vault.get_secret("scope", "old_key").as_deref(),
            Some("saved")
        );
        assert_eq!(vault.list_scope("scope").len(), 1);
        assert!(vault.set_secret("scope", "new_key", Some("new")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        drop(lock);
        vault.set_secret("scope", "new_key", Some("new")).unwrap();
        assert_eq!(vault.list_scope("scope").len(), 2);
    }

    #[test]
    fn in_memory_vault_crud_scope_isolation_and_2048_byte_cap() {
        let vault = CredentialVault::in_memory();
        let clone = vault.clone();

        vault
            .set_secret("plugin.cloud", "access_token", Some("tok_123"))
            .unwrap();
        vault
            .set_secret("plugin.cloud", "refresh_token", Some("ref_456"))
            .unwrap();
        vault
            .set_secret("plugin.other", "api_key", Some("k_999"))
            .unwrap();

        assert_eq!(
            clone.get_secret("plugin.cloud", "access_token").as_deref(),
            Some("tok_123")
        );
        let listed = clone.list_scope("plugin.cloud");
        assert_eq!(listed.len(), 2);
        assert_eq!(
            listed.get("refresh_token").map(String::as_str),
            Some("ref_456")
        );

        // Delete one key and verify scope cleanup.
        vault
            .set_secret("plugin.cloud", "access_token", None)
            .unwrap();
        assert_eq!(vault.get_secret("plugin.cloud", "access_token"), None);
        assert_eq!(vault.list_scope("plugin.cloud").len(), 1);

        // Exact 2048-byte secret succeeds; 2049-byte secret fails closed.
        let max_ok = "a".repeat(MAX_SECRET_BYTES);
        vault
            .set_secret("plugin.cloud", "exact_limit", Some(&max_ok))
            .unwrap();
        let too_large = "a".repeat(MAX_SECRET_BYTES + 1);
        assert!(
            vault
                .set_secret("plugin.cloud", "too_large", Some(&too_large))
                .is_err()
        );

        // Control characters and empty identifiers are rejected.
        assert!(vault.set_secret("", "k", Some("v")).is_err());
        assert!(vault.set_secret("bad\nscope", "k", Some("v")).is_err());
        assert!(vault.set_secret("scope", "bad\0key", Some("v")).is_err());

        // Per-scope 64-key quota allows updates/deletes at capacity but blocks a 65th key.
        for i in 0..MAX_SECRETS_PER_SCOPE {
            vault
                .set_secret("quota.scope", &format!("k{i}"), Some("v"))
                .unwrap();
        }
        vault
            .set_secret("quota.scope", "k0", Some("updated"))
            .unwrap();
        assert!(
            vault
                .set_secret("quota.scope", "k_overflow", Some("v"))
                .is_err()
        );
    }
}
