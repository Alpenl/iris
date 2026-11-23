//! Cross-platform credential vault backed by macOS Keychain and Windows
//! Credential Manager (fail-closed, zero plaintext disk fallback), plus a
//! thread-safe in-memory backend for deterministic unit and integration tests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};

use crate::fs_util;

/// Maximum secret payload size in bytes (strictly below Windows `CRED_MAX_CREDENTIAL_BLOB_SIZE = 2560`).
pub const MAX_SECRET_BYTES: usize = 2048;

const MAX_KEY_BYTES: usize = 256;

static SYSTEM_VAULT_LOCK: Mutex<()> = Mutex::new(());

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

fn validate_id_part<'a>(label: &str, val: &'a str) -> Result<&'a str> {
    let trimmed = val.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_KEY_BYTES {
        bail!("invalid credential {label}");
    }
    if trimmed.bytes().any(|b| matches!(b, b'\0' | b'\r' | b'\n')) {
        bail!("credential {label} contains control characters");
    }
    Ok(trimmed)
}

fn read_json_map<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    let Ok(text) = std::fs::read_to_string(path) else {
        return T::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn write_json_private<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value).context("failed serializing vault JSON")?;
    fs_util::atomic_write_private(path, json.as_bytes())
        .with_context(|| format!("failed writing vault file {}", path.display()))
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join("credential_index.json")
}

fn update_index(dir: &Path, scope: &str, key: &str, present: bool) -> Result<()> {
    let path = index_path(dir);
    let mut index: CredentialIndex = read_json_map(&path);
    if present {
        index
            .entry(scope.to_string())
            .or_default()
            .insert(key.to_string());
    } else if let Some(keys) = index.get_mut(scope) {
        keys.remove(key);
        if keys.is_empty() {
            index.remove(scope);
        }
    }
    if index.is_empty() && !path.exists() {
        return Ok(());
    }
    write_json_private(&path, &index)
}

#[cfg(target_os = "macos")]
mod os_store {
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    fn service_name(scope: &str) -> String {
        format!("app.iris.player.{scope}")
    }

    pub fn set(scope: &str, key: &str, secret: &str) -> Result<(), String> {
        set_generic_password(&service_name(scope), key, secret.as_bytes())
            .map_err(|e| e.to_string())
    }

    pub fn get(scope: &str, key: &str) -> Option<String> {
        let bytes = get_generic_password(&service_name(scope), key).ok()?;
        String::from_utf8(bytes).ok()
    }

    pub fn delete(scope: &str, key: &str) {
        let _ = delete_generic_password(&service_name(scope), key);
    }
}

#[cfg(target_os = "windows")]
mod os_store {
    use core::ffi::c_void;
    use windows_sys::Win32::Security::Credentials::{
        CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW,
        CredDeleteW, CredFree, CredReadW, CredWriteW,
    };

    fn target_wide(scope: &str, key: &str) -> Vec<u16> {
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
        let mut target = target_wide(scope, key);
        let mut user: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
        let mut blob: Vec<u8> = bytes.to_vec();
        let cred = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: std::ptr::null_mut(),
            LastWritten: Default::default(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: if blob.is_empty() {
                std::ptr::null_mut()
            } else {
                blob.as_mut_ptr()
            },
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: user.as_mut_ptr(),
        };
        let ok = unsafe { CredWriteW(&cred, 0) };
        if ok == 0 {
            Err("CredWriteW failed".into())
        } else {
            Ok(())
        }
    }

    pub fn get(scope: &str, key: &str) -> Option<String> {
        let target = target_wide(scope, key);
        let mut pcred: *mut CREDENTIALW = std::ptr::null_mut();
        let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut pcred) };
        if ok == 0 || pcred.is_null() {
            return None;
        }
        let value = unsafe {
            let cred = &*pcred;
            let bytes = if cred.CredentialBlob.is_null() || cred.CredentialBlobSize == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize)
            };
            String::from_utf8(bytes.to_vec()).ok()
        };
        unsafe { CredFree(pcred as *const c_void) };
        value
    }

    pub fn delete(scope: &str, key: &str) {
        let target = target_wide(scope, key);
        unsafe {
            let _ = CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0);
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod os_store {
    pub fn set(_scope: &str, _key: &str, _secret: &str) -> Result<(), String> {
        Err("native OS credential store unavailable on this platform".into())
    }

    pub fn get(_scope: &str, _key: &str) -> Option<String> {
        None
    }

    pub fn delete(_scope: &str, _key: &str) {}
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
        match &self.backend {
            VaultBackend::InMemory(store) => {
                let guard = store.lock().unwrap_or_else(|e| e.into_inner());
                guard.get(scope)?.get(key).cloned()
            }
            #[cfg(any(debug_assertions, feature = "devtools"))]
            VaultBackend::TestFile(path) => {
                let _guard = SYSTEM_VAULT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let secrets: ScopedSecrets = read_json_map(path);
                secrets.get(scope)?.get(key).cloned()
            }
            VaultBackend::System => {
                let _guard = SYSTEM_VAULT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                os_store::get(scope, key)
            }
        }
    }

    /// Stores or deletes (`secret = None`) a secret under `(scope, key)`.
    pub fn set_secret(&self, scope: &str, key: &str, secret: Option<&str>) -> Result<()> {
        let scope = validate_id_part("scope", scope)?;
        let key = validate_id_part("key", key)?;
        if let Some(s) = secret
            && s.len() > MAX_SECRET_BYTES
        {
            bail!("secret exceeds maximum size ({MAX_SECRET_BYTES} bytes)");
        }

        match &self.backend {
            VaultBackend::InMemory(store) => {
                let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
                apply_secret_edit(&mut guard, scope, key, secret);
                Ok(())
            }
            #[cfg(any(debug_assertions, feature = "devtools"))]
            VaultBackend::TestFile(path) => {
                let _guard = SYSTEM_VAULT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let mut secrets: ScopedSecrets = read_json_map(path);
                apply_secret_edit(&mut secrets, scope, key, secret);
                write_json_private(path, &secrets)
            }
            VaultBackend::System => {
                let _guard = SYSTEM_VAULT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let dir = fs_util::data_dir()
                    .ok_or_else(|| anyhow!("application data dir unavailable"))?;
                match secret {
                    Some(val) => {
                        os_store::set(scope, key, val).map_err(|err| {
                            anyhow!("OS credential store rejected {scope}/{key}: {err}")
                        })?;
                        update_index(&dir, scope, key, true)?;
                    }
                    None => {
                        os_store::delete(scope, key);
                        update_index(&dir, scope, key, false)?;
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
        match &self.backend {
            VaultBackend::InMemory(store) => {
                let guard = store.lock().unwrap_or_else(|e| e.into_inner());
                guard.get(scope).cloned().unwrap_or_default()
            }
            #[cfg(any(debug_assertions, feature = "devtools"))]
            VaultBackend::TestFile(path) => {
                let _guard = SYSTEM_VAULT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let secrets: ScopedSecrets = read_json_map(path);
                secrets.get(scope).cloned().unwrap_or_default()
            }
            VaultBackend::System => {
                let _guard = SYSTEM_VAULT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let Some(dir) = fs_util::data_dir() else {
                    return BTreeMap::new();
                };
                let index: CredentialIndex = read_json_map(&index_path(&dir));
                let Some(keys) = index.get(scope) else {
                    return BTreeMap::new();
                };
                let mut out = BTreeMap::new();
                for key in keys {
                    if let Some(val) = os_store::get(scope, key) {
                        out.insert(key.clone(), val);
                    }
                }
                out
            }
        }
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
    use super::*;

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
    }
}
