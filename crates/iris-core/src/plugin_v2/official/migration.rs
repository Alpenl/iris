use super::super::{installation::Installed, runtime::Setup, storage::Storage};
use crate::{fs_util, vfs::HttpCancel};
use iris_api::v2::ErrorCode;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{ErrorKind, Read},
    path::Path,
    time::Duration,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Complete {
    schema: u32,
    plugin: String,
    installation: String,
}

pub(super) fn migrate(root: &Path, installed: &Installed, setup: &Setup) -> Result<(), ErrorCode> {
    if !matches!(installed.plugin_id.as_str(), "iris.guides" | "iris.webdav") {
        return Err(ErrorCode::Denied);
    }
    let directory = root
        .join("plugin-v2-migration")
        .join(&installed.installation_id);
    let complete = directory.join("complete.json");
    let _lock = fs_util::lock_file_with_timeout(
        &directory.join("migration.lock"),
        Duration::from_millis(250),
    )
    .map_err(|_| ErrorCode::Backpressure)?;
    if let Some(bytes) = read_bounded(&complete, 4096)? {
        let record: Complete = crate::plugin_v2::json::decode(&bytes, 4096)?;
        if record.schema != 1
            || record.plugin != installed.plugin_id
            || record.installation != installed.installation_id
        {
            return Err(ErrorCode::Denied);
        }
        return Ok(());
    }
    let store = Storage::new(
        setup.state_directory.clone(),
        installed.installation_id.clone(),
        setup.vault.clone(),
        setup.resources.ledger.clone(),
    )?;
    let key = if installed.plugin_id == "iris.guides" {
        "mode"
    } else {
        "servers"
    };
    let cancel = HttpCancel::default();
    if store.read_state(key, &cancel)?.0.is_none() {
        let legacy = root
            .join("plugin_state")
            .join(format!("{}.json", installed.plugin_id));
        if let Some(bytes) = read_bounded(&legacy, 16 << 20)? {
            // Snapshot is private and immutable, including any historical URL
            // userinfo. Keep the original and old vault entries for rollback.
            let digest: String = ring::digest::digest(&ring::digest::SHA256, &bytes)
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let backup = directory.join(format!("v1-{digest}.json"));
            if let Some(existing) = read_bounded(&backup, 16 << 20)? {
                if existing != bytes {
                    return Err(ErrorCode::StaleHandle);
                }
            } else {
                let backups = std::fs::read_dir(&directory)
                    .map_err(|_| ErrorCode::IoError)?
                    .filter_map(Result::ok)
                    .filter(|p| p.file_name().to_string_lossy().starts_with("v1-"))
                    .count();
                if backups >= 8 {
                    return Err(ErrorCode::QuotaExceeded);
                }
                fs_util::atomic_write_private(&backup, &bytes).map_err(|_| ErrorCode::IoError)?;
            }
            let state: BTreeMap<String, String> =
                serde_json::from_slice(&bytes).map_err(|_| ErrorCode::InvalidArgument)?;
            if let Some(value) = state.get(key) {
                let value = if key == "servers" {
                    migrate_servers(value, installed, setup)?
                } else if matches!(
                    value.as_str(),
                    "off" | "thirds" | "golden" | "cross" | "cinema" | "safe"
                ) {
                    value.clone()
                } else {
                    return Err(ErrorCode::InvalidArgument);
                };
                store.write_state(key, Some(&value), &cancel)?;
            }
        }
    }
    // Missing legacy data and an already-populated v2 store are both terminal:
    // a later reset/reinstall cannot resurrect a stale configuration silently.
    let record = Complete {
        schema: 1,
        plugin: installed.plugin_id.clone(),
        installation: installed.installation_id.clone(),
    };
    fs_util::atomic_write_private(
        &complete,
        serde_json::to_vec(&record).map_err(|_| ErrorCode::Internal)?,
    )
    .map_err(|_| ErrorCode::IoError)
}
fn read_bounded(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, ErrorCode> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ErrorCode::IoError),
    };
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ErrorCode::IoError)?;
    if bytes.len() > limit {
        return Err(ErrorCode::QuotaExceeded);
    }
    Ok(Some(bytes))
}
#[derive(Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Auth {
    #[default]
    None,
    Basic {
        username: String,
    },
    Bearer,
}
#[derive(Serialize, Deserialize)]
struct Connection {
    id: String,
    name: String,
    endpoint: String,
    #[serde(default)]
    base_path: String,
    #[serde(default)]
    auth: Auth,
    #[serde(default)]
    allow_insecure_auth: bool,
}
fn migrate_servers(raw: &str, installed: &Installed, setup: &Setup) -> Result<String, ErrorCode> {
    let mut connections: Vec<Connection> =
        serde_json::from_str(raw).map_err(|_| ErrorCode::InvalidArgument)?;
    if connections.len() > 32 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut ids = BTreeSet::new();
    let scope = format!("plugin-v2.{}", installed.installation_id);
    let mut secrets = Vec::new();
    for conn in &mut connections {
        if !iris_api::is_valid_id(&conn.id)
            || conn.id.len() > 96
            || !ids.insert(conn.id.clone())
            || conn.name.is_empty()
            || conn.name.len() > 256
            || conn.endpoint.len() > 4096
            || conn.base_path.len() > 4096
        {
            return Err(ErrorCode::InvalidArgument);
        }
        let mut url = url::Url::parse(&conn.endpoint).map_err(|_| ErrorCode::InvalidArgument)?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(ErrorCode::InvalidArgument);
        }
        let mut embedded = None;
        if !url.username().is_empty() || url.password().is_some() {
            let username = percent_encoding::percent_decode_str(url.username())
                .decode_utf8()
                .map_err(|_| ErrorCode::InvalidArgument)?
                .into_owned();
            if username.is_empty() || username.contains([':', '\r', '\n', '\0']) {
                return Err(ErrorCode::InvalidArgument);
            }
            if conn.auth != Auth::None
                && !matches!(&conn.auth, Auth::Basic { username: current } if current == &username)
            {
                return Err(ErrorCode::AuthFailed);
            }
            embedded = url
                .password()
                .map(|p| {
                    percent_encoding::percent_decode_str(p)
                        .decode_utf8()
                        .map(String::from)
                })
                .transpose()
                .map_err(|_| ErrorCode::InvalidArgument)?;
            conn.auth = Auth::Basic { username };
            url.set_password(None)
                .map_err(|_| ErrorCode::InvalidArgument)?;
            url.set_username("")
                .map_err(|_| ErrorCode::InvalidArgument)?;
        }
        if matches!(&conn.auth, Auth::Basic { username } if username.is_empty() || username.len() > 256 || username.contains([':', '\r', '\n', '\0']))
        {
            return Err(ErrorCode::InvalidArgument);
        }
        url.set_query(None);
        url.set_fragment(None);
        conn.endpoint = url.into();
        // Only the host Origin sheet can grant plaintext credential authority.
        conn.allow_insecure_auth = false;
        if conn.auth != Auth::None {
            let key = format!("secret.{}", conn.id);
            let destination = format!("p.{key}");
            if setup
                .vault
                .try_get_secret(&scope, &destination)
                .map_err(|_| ErrorCode::IoError)?
                .is_none()
            {
                let value = setup
                    .vault
                    .try_get_secret("builtin.iris.webdav", &key)
                    .map_err(|_| ErrorCode::IoError)?
                    .or(embedded);
                if value
                    .as_ref()
                    .is_some_and(|v| v.len() > 2048 || v.contains(['\r', '\n', '\0']))
                {
                    return Err(ErrorCode::InvalidArgument);
                }
                // No secret is fabricated for an unauthenticated legacy record.
                if let Some(value) = value {
                    secrets.push((destination, value));
                }
            }
        }
    }
    let encoded = serde_json::to_string(&connections).map_err(|_| ErrorCode::Internal)?;
    if encoded.len() > 32 << 10 {
        return Err(ErrorCode::QuotaExceeded);
    }
    // Validate the entire record before writing any destination credentials.
    for (key, value) in secrets {
        setup
            .vault
            .set_secret(&scope, &key, Some(&value))
            .map_err(|_| ErrorCode::IoError)?;
    }
    Ok(encoded)
}
