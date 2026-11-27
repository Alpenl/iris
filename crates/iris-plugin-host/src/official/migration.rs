use super::super::{installation::Installed, runtime::Setup, storage::Storage};
use iris_api::v2::ErrorCode;
use iris_core::fs_util;
use iris_core::media_uri::MediaUri;
use iris_net::http_client::HttpCancel;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    time::Duration,
};

const MAX_SAFE_WEBDAV_ID_LEN: usize = 48;
const MIGRATION_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

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
    let _lock =
        fs_util::lock_file_with_timeout(&directory.join("migration.lock"), MIGRATION_LOCK_TIMEOUT)
            .map_err(|_| ErrorCode::Backpressure)?;
    if let Some(bytes) = read_bounded(&complete, 4096)? {
        let record: Complete =
            iris_core::bounded_json::decode(&bytes, 4096).map_err(super::super::json_error)?;
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
    let mut latest_backup = None;
    let mut remapped_ids = BTreeMap::new();
    let existing_state = store.read_state(key, &cancel)?.0;
    if let Some(existing_value) = existing_state {
        if key == "servers" {
            let (recovered_remapped, recovered_backup) =
                recover_webdav_remapping(&directory, root, &existing_value)?;
            remapped_ids = recovered_remapped;
            latest_backup = recovered_backup;
        } else {
            latest_backup = find_latest_backup(&directory);
        }
    } else {
        let legacy = root
            .join("plugin_state")
            .join(format!("{}.json", installed.plugin_id));
        if let Some(bytes) = read_bounded(&legacy, 16 << 20)? {
            // Snapshot is private and immutable, including any historical URL
            // userinfo. Keep the original and old vault entries for rollback.
            let digest = sha256_hex(&bytes);
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
            latest_backup = Some(backup);
            let state: BTreeMap<String, String> =
                serde_json::from_slice(&bytes).map_err(|_| ErrorCode::InvalidArgument)?;
            if let Some(value) = state.get(key) {
                let value = if key == "servers" {
                    let (encoded, remapped) = migrate_servers(value, installed, setup)?;
                    remapped_ids = remapped;
                    encoded
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
    if !remapped_ids.is_empty() {
        remap_persisted_webdav_references(root, &remapped_ids)?;
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
    .map_err(|_| ErrorCode::IoError)?;
    // Prune superseded failed-attempt migration backups while retaining the final snapshot.
    if let Ok(entries) = std::fs::read_dir(&directory) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with("v1-")
                && latest_backup.as_ref() != Some(&path)
            {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    Ok(())
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_key: Option<String>,
}
fn is_valid_legacy_id(s: &str) -> bool {
    if s.len() <= 64 {
        return iris_api::is_valid_id(s);
    }
    let bytes = s.as_bytes();
    bytes.len() <= 96
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && !s.contains("..")
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        && !s.split('.').any(|stem| {
            let b = stem.as_bytes();
            matches!(
                stem.to_ascii_uppercase().as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ) || (b.len() == 4
                && b[3].is_ascii_digit()
                && (b[..3].eq_ignore_ascii_case(b"COM") || b[..3].eq_ignore_ascii_case(b"LPT")))
        })
}
type RecoveredRemapping = (BTreeMap<String, String>, Option<PathBuf>);

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn find_latest_backup(directory: &Path) -> Option<PathBuf> {
    std::fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("v1-"))
        })
}
fn remapped_ids_from_legacy_bytes(bytes: &[u8]) -> Option<(BTreeMap<String, String>, Vec<String>)> {
    let state: BTreeMap<String, String> = serde_json::from_slice(bytes).ok()?;
    let raw = state.get("servers")?;
    let connections: Vec<Connection> = serde_json::from_str(raw).ok()?;
    if connections.len() > 32 {
        return None;
    }
    let mut raw_ids = BTreeSet::new();
    let mut mapped_ids = BTreeSet::new();
    let mut remapped = BTreeMap::new();
    let mut migrated_list = Vec::with_capacity(connections.len());
    for conn in connections {
        if !is_valid_legacy_id(&conn.id) || !raw_ids.insert(conn.id.clone()) {
            return None;
        }
        let migrated_id = remap_legacy_connection_id(&conn.id);
        if !mapped_ids.insert(migrated_id.clone()) {
            return None;
        }
        if migrated_id != conn.id {
            remapped.insert(conn.id, migrated_id.clone());
        }
        migrated_list.push(migrated_id);
    }
    Some((remapped, migrated_list))
}
fn recover_webdav_remapping(
    directory: &Path,
    root: &Path,
    existing_servers: &str,
) -> Result<RecoveredRemapping, ErrorCode> {
    let existing_ids: Option<Vec<String>> =
        serde_json::from_str::<Vec<Connection>>(existing_servers)
            .ok()
            .map(|conns| conns.into_iter().map(|c| c.id).collect());
    let mut fallback = None;
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !entry.file_name().to_string_lossy().starts_with("v1-") {
                continue;
            }
            if let Some(bytes) = read_bounded(&path, 16 << 20)?
                && let Some((remapped, ids)) = remapped_ids_from_legacy_bytes(&bytes)
            {
                if existing_ids.as_ref() == Some(&ids) {
                    return Ok((remapped, Some(path)));
                }
                if fallback.is_none() {
                    fallback = Some((remapped, Some(path)));
                }
            }
        }
    }
    let legacy = root.join("plugin_state").join("iris.webdav.json");
    if let Some(bytes) = read_bounded(&legacy, 16 << 20)?
        && let Some((remapped, _)) = remapped_ids_from_legacy_bytes(&bytes)
    {
        let candidate = directory.join(format!("v1-{}.json", sha256_hex(&bytes)));
        let backup = candidate.exists().then_some(candidate);
        return Ok((remapped, backup));
    }
    Ok(fallback.unwrap_or_else(|| (BTreeMap::new(), None)))
}
fn remap_legacy_connection_id(legacy_id: &str) -> String {
    if legacy_id.len() <= MAX_SAFE_WEBDAV_ID_LEN {
        return legacy_id.to_owned();
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, legacy_id.as_bytes());
    let mut out = String::with_capacity(35);
    out.push_str("v1-");
    for byte in &digest.as_ref()[..16] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
fn migrate_servers(
    raw: &str,
    installed: &Installed,
    setup: &Setup,
) -> Result<(String, BTreeMap<String, String>), ErrorCode> {
    let mut connections: Vec<Connection> =
        serde_json::from_str(raw).map_err(|_| ErrorCode::InvalidArgument)?;
    if connections.len() > 32 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut raw_ids = BTreeSet::new();
    let mut mapped_ids = BTreeSet::new();
    let mut remapped = BTreeMap::new();
    let scope = format!("plugin-v2.{}", installed.installation_id);
    let mut secrets = Vec::new();
    for conn in &mut connections {
        let legacy_id = conn.id.clone();
        if !is_valid_legacy_id(&legacy_id)
            || !raw_ids.insert(legacy_id.clone())
            || conn.name.is_empty()
            || conn.name.len() > 256
            || conn.endpoint.len() > 4096
            || conn.base_path.len() > 4096
            || conn
                .credential_key
                .as_deref()
                .is_some_and(|k| !is_valid_legacy_id(k))
        {
            return Err(ErrorCode::InvalidArgument);
        }
        let migrated_id = remap_legacy_connection_id(&legacy_id);
        if !mapped_ids.insert(migrated_id.clone()) {
            return Err(ErrorCode::InvalidArgument);
        }
        if migrated_id != legacy_id {
            remapped.insert(legacy_id.clone(), migrated_id.clone());
        }
        let legacy_secret_key = conn
            .credential_key
            .take()
            .unwrap_or_else(|| format!("secret.{legacy_id}"));
        conn.id = migrated_id;
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
                    .try_get_secret("builtin.iris.webdav", &legacy_secret_key)
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
    Ok((encoded, remapped))
}
fn remap_media_uri(uri: &MediaUri, remapped: &BTreeMap<String, String>) -> Option<MediaUri> {
    match uri {
        MediaUri::Plugin {
            plugin_id,
            source_id,
            entry_id,
        } if plugin_id == "iris.webdav" => {
            remapped.get(source_id).map(|next_id| MediaUri::Plugin {
                plugin_id: plugin_id.clone(),
                source_id: next_id.clone(),
                entry_id: entry_id.clone(),
            })
        }
        _ => None,
    }
}
fn decode_hex_uri_str(hex_text: &str) -> Option<String> {
    if hex_text.is_empty() || !hex_text.len().is_multiple_of(2) || !hex_text.is_ascii() {
        return None;
    }
    let bytes: Vec<u8> = (0..hex_text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex_text[i..i + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    if !bytes.contains(&0)
        && let Ok(text) = String::from_utf8(bytes.clone())
        && !text.is_empty()
    {
        return Some(text);
    }
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_le_bytes)
        .collect();
    if units.contains(&0) {
        None
    } else {
        String::from_utf16(&units).ok()
    }
}
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}
fn remap_tsv_file(path: &Path, remapped: &BTreeMap<String, String>) -> Result<(), ErrorCode> {
    let Some(bytes) = read_bounded(path, 16 << 20)? else {
        return Ok(());
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(());
    };
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        let (col0, rest) = match line.split_once('\t') {
            Some((a, b)) => (a, Some(b)),
            None => (line, None),
        };
        if let Some(decoded) = decode_hex_uri_str(col0.trim())
            && let Some(next) = remap_media_uri(&MediaUri::parse_persisted(&decoded), remapped)
        {
            out.push_str(&hex_encode(next.to_uri_string().as_bytes()));
            if let Some(rest) = rest {
                out.push('\t');
                out.push_str(rest);
            }
            out.push('\n');
            changed = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if changed {
        fs_util::atomic_write(path, out).map_err(|_| ErrorCode::IoError)?;
    }
    Ok(())
}
fn remap_persisted_webdav_references(
    root: &Path,
    remapped: &BTreeMap<String, String>,
) -> Result<(), ErrorCode> {
    let locations_path = root.join("locations.json");
    if let Some(bytes) = read_bounded(&locations_path, 4 << 20)?
        && let Ok(mut locations) = serde_json::from_slice::<iris_core::vfs::LocationStore>(&bytes)
    {
        let mut changed = false;
        for bookmark in &mut locations.bookmarks {
            if let Some(next) = remap_media_uri(&bookmark.uri, remapped) {
                bookmark.uri = next;
                changed = true;
            }
        }
        if changed {
            locations
                .save_to(&locations_path)
                .map_err(|_| ErrorCode::IoError)?;
        }
    }
    let tsv_names = [
        "records.tsv",
        "uri_records.tsv",
        "recent.tsv",
        "recent_uris.tsv",
    ];
    if tsv_names.iter().any(|name| root.join(name).exists()) {
        let _lock =
            fs_util::lock_file_with_timeout(&root.join("records.lock"), MIGRATION_LOCK_TIMEOUT)
                .map_err(|e| {
                    if e.kind() == ErrorKind::WouldBlock {
                        ErrorCode::Backpressure
                    } else {
                        ErrorCode::IoError
                    }
                })?;
        for name in tsv_names {
            remap_tsv_file(&root.join(name), remapped)?;
        }
    }
    let annotations_path = root.join("annotations.json");
    if annotations_path.exists() {
        let _lock =
            fs_util::lock_file_with_timeout(&root.join("annotations.lock"), MIGRATION_LOCK_TIMEOUT)
                .map_err(|e| {
                    if e.kind() == ErrorKind::WouldBlock {
                        ErrorCode::Backpressure
                    } else {
                        ErrorCode::IoError
                    }
                })?;
        if let Some(bytes) = read_bounded(&annotations_path, 16 << 20)?
            && let Ok(records) = iris_core::bounded_json::decode::<
                BTreeMap<String, serde_json::Value>,
            >(&bytes, 16 << 20)
        {
            let mut next_records = BTreeMap::new();
            let mut changed = false;
            for (key, value) in records {
                if let Some(next) = remap_media_uri(&MediaUri::parse_persisted(&key), remapped) {
                    next_records.entry(next.to_uri_string()).or_insert(value);
                    changed = true;
                } else {
                    next_records.insert(key, value);
                }
            }
            if changed {
                let encoded = iris_core::bounded_json::encode(&next_records, 16 << 20)
                    .map_err(super::super::json_error)?;
                fs_util::atomic_write_private(&annotations_path, encoded)
                    .map_err(|_| ErrorCode::IoError)?;
            }
        }
    }
    Ok(())
}
