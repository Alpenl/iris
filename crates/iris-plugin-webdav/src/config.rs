use super::{
    WebDav, WindowState, modal,
    protocol::{WebDavAuth, WebDavConnection},
};
use iris_plugin_sdk::{
    api::{
        is_valid_id,
        v2::{
            ErrorCode, HostCommand, Id,
            task::{CredentialBucket, Task},
            task_result::StoreWrite,
        },
    },
    client::{Host, completed},
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn decode_servers(raw: Option<&str>) -> Result<Vec<WebDavConnection>, ErrorCode> {
    let servers: Vec<WebDavConnection> = raw
        .map(serde_json::from_str)
        .transpose()
        .map_err(|_| ErrorCode::InvalidArgument)?
        .unwrap_or_default();
    let mut ids = BTreeSet::new();
    if raw.is_some_and(|s| s.len() > 32 * 1024)
        || servers.len() > 32
        || servers.iter().any(|c| {
            !is_valid_id(&c.id)
                || !ids.insert(&c.id)
                || c.name.is_empty()
                || c.name.len() > 256
                || c.endpoint.len() > 4096
                || c.base_path.len() > 4096
                || c.root_segments().is_err()
                || c.credential_key
                    .as_ref()
                    .is_some_and(|s| !is_valid_id(s) || s.len() > 128)
        })
    {
        return Err(ErrorCode::InvalidArgument);
    }
    Ok(servers)
}
pub(super) fn secret_key(conn: &WebDavConnection) -> String {
    conn.credential_key
        .clone()
        .unwrap_or_else(|| legacy_secret_key(&conn.id))
}
pub(super) fn legacy_secret_key(id: &str) -> String {
    let direct = format!("secret.{id}");
    if is_valid_id(&direct) {
        return direct;
    }
    let mut hi: u64 = 0xcbf2_9ce4_8422_2325;
    let mut lo: u64 = 0x8422_2325_cbf2_9ce4;
    for &byte in id.as_bytes() {
        hi = (hi ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        lo = (lo.rotate_left(5) ^ u64::from(byte)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    format!("secret.h{hi:016x}{lo:016x}")
}
fn write_state(servers: &[WebDavConnection], host: &mut impl Host) -> Result<Id, ErrorCode> {
    let raw = serde_json::to_string(servers).map_err(|_| ErrorCode::Internal)?;
    if raw.len() > 32 * 1024 {
        return Err(ErrorCode::QuotaExceeded);
    }
    host.submit(&Task::StateWrite {
        key: "servers".into(),
        value: Some(raw),
    })
}
enum Phase {
    Secret,
    State,
}
pub(super) struct Transaction {
    pub operation: Id,
    phase: Phase,
    servers: Vec<WebDavConnection>,
    new_key: Option<String>,
    old_key: Option<String>,
    modal: Option<String>,
}
impl Transaction {
    fn cleanup(key: Option<String>, host: &mut impl Host) {
        if let Some(key) = key {
            // The old key is retained until the new configuration is committed.
            // Cleanup failure may leave an unreachable secret, never a lost one.
            let _ = host.submit(&Task::SecretWrite {
                bucket: CredentialBucket::Plugin,
                key,
                value: None,
            });
        }
    }
}
impl WebDav {
    pub(super) fn save_config(
        &mut self,
        window: Id,
        state: &mut WindowState,
        modal: &str,
        values: &BTreeMap<String, String>,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        if self.config_owner.is_some() {
            return modal::toast("另一项连接配置正在保存，请稍后重试");
        }
        let mut validated = match modal::validate_modal_submit(
            &self.servers,
            modal,
            state.editing.as_deref(),
            values,
        ) {
            Ok(value) => value,
            Err(e) => return modal::toast(e),
        };
        let mut servers = self.servers.clone();
        let old = servers.iter().find(|c| c.id == validated.conn.id);
        let old_key = old.filter(|c| c.auth != WebDavAuth::None).map(secret_key);
        let mut new_key = None;
        if validated.secret.is_some() {
            let mut entropy = [0; 16];
            if let Err(e) = host.random(&mut entropy) {
                return modal::toast(format!(
                    "无法生成凭据版本：{}",
                    iris_plugin_sdk::error_message(e)
                ));
            }
            let suffix: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
            let key = format!("cred.{suffix}");
            validated.conn.credential_key = Some(key.clone());
            new_key = Some(key);
        }
        let old_key = old_key.filter(|old| Some(old) != validated.conn.credential_key.as_ref());
        if let Some(c) = servers.iter_mut().find(|c| c.id == validated.conn.id) {
            *c = validated.conn;
        } else {
            servers.push(validated.conn);
        }
        // Validate size before any credential mutation.
        let encoded = serde_json::to_string(&servers).unwrap();
        if decode_servers(Some(&encoded)).is_err() {
            return modal::toast("连接配置超出存储限制");
        }
        let (phase, operation) = if let Some(key) = &new_key {
            (
                Phase::Secret,
                host.submit(&Task::SecretWrite {
                    bucket: CredentialBucket::Plugin,
                    key: key.clone(),
                    value: validated.secret,
                }),
            )
        } else {
            (Phase::State, write_state(&servers, host))
        };
        match operation {
            Ok(operation) => {
                self.config_owner = Some(window);
                state.config = Some(Transaction {
                    operation,
                    phase,
                    servers,
                    new_key,
                    old_key,
                    modal: Some(modal.into()),
                });
                Vec::new()
            }
            Err(e) => modal::toast(format!(
                "配置尚未保存：{}",
                iris_plugin_sdk::error_message(e)
            )),
        }
    }
    pub(super) fn remove_config(
        &mut self,
        window: Id,
        state: &mut WindowState,
        source: &str,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        if self.config_owner.is_some() {
            return modal::toast("连接配置正在保存，请稍后重试");
        }
        let Some(conn) = self.servers.iter().find(|c| c.id == source) else {
            return Vec::new();
        };
        if state.editing.as_deref() == Some(source) {
            state.editing = None;
        }
        for win in self.windows.values_mut() {
            if win.editing.as_deref() == Some(source) {
                win.editing = None;
            }
        }
        let old_key = (conn.auth != WebDavAuth::None).then(|| secret_key(conn));
        let servers: Vec<_> = self
            .servers
            .iter()
            .filter(|c| c.id != source)
            .cloned()
            .collect();
        match write_state(&servers, host) {
            Ok(operation) => {
                self.config_owner = Some(window);
                state.config = Some(Transaction {
                    operation,
                    phase: Phase::State,
                    servers,
                    new_key: None,
                    old_key,
                    modal: None,
                });
                Vec::new()
            }
            Err(e) => modal::toast(format!(
                "连接尚未移除：{}",
                iris_plugin_sdk::error_message(e)
            )),
        }
    }
    pub(super) fn config_completed(
        &mut self,
        window: Id,
        state: &mut WindowState,
        result: Value,
        host: &mut impl Host,
    ) -> Result<Vec<HostCommand>, ErrorCode> {
        let mut tx = state.config.take().ok_or(ErrorCode::StaleHandle)?;
        if self.config_owner != Some(window) {
            return Err(ErrorCode::StaleHandle);
        }
        self.config_owner = None;
        if !completed::<StoreWrite>(result).is_ok_and(|v| v.stored) {
            if matches!(tx.phase, Phase::Secret) {
                Transaction::cleanup(tx.new_key, host);
                return Ok(modal::toast("凭据未能保存，请稍后重试"));
            }
            // A failed StateWrite has no commit receipt. Keep both versions: a
            // cancelled atomic write may already be durable. Reload to reconcile.
            self.loaded = false;
            let _ = self.reload(Some(window), host);
            return Ok(modal::toast("保存未确认，正在重新读取配置；原凭据仍保留"));
        }
        match tx.phase {
            Phase::Secret => {
                match write_state(&tx.servers, host) {
                    Ok(id) => {
                        tx.operation = id;
                        tx.phase = Phase::State;
                        state.config = Some(tx);
                        self.config_owner = Some(window);
                    }
                    Err(e) => {
                        Transaction::cleanup(tx.new_key, host);
                        return Ok(modal::toast(format!(
                            "新配置尚未保存：{}",
                            iris_plugin_sdk::error_message(e)
                        )));
                    }
                }
                Ok(Vec::new())
            }
            Phase::State => {
                self.servers = tx.servers;
                state.cache = None;
                if state
                    .editing
                    .as_deref()
                    .is_some_and(|id| !self.servers.iter().any(|c| c.id == id))
                {
                    state.editing = None;
                }
                for win in self.windows.values_mut() {
                    win.cache = None;
                    if win
                        .editing
                        .as_deref()
                        .is_some_and(|id| !self.servers.iter().any(|c| c.id == id))
                    {
                        win.editing = None;
                    }
                }
                Transaction::cleanup(tx.old_key, host);
                let mut commands = vec![HostCommand::SetSources {
                    sources: modal::source_descriptors(&self.servers),
                }];
                if let Some(id) = tx.modal {
                    if id == modal::EDIT_MODAL_ID {
                        state.editing = None;
                    }
                    commands.push(HostCommand::CloseModal { id: Some(id) });
                }
                commands.extend(modal::toast("WebDAV 配置已保存"));
                Ok(commands)
            }
        }
    }
}
