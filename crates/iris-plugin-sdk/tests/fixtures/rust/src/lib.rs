use iris_plugin_sdk::{
    api::v2::{ErrorCode, GuestError, HostCommand, Id, Reply, task::Task},
    guest,
    serde_json::{self, Value},
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
};
thread_local! {
    static CONTEXT: Cell<u8> = const { Cell::new(0) };
    static READS: RefCell<BTreeMap<Id,(Id,u64,usize,u8)>> = const { RefCell::new(BTreeMap::new()) };
}
fn slow() -> Result<Id, ErrorCode> {
    guest::submit(&Task::HttpStart {
        method: "GET".into(),
        url: "https://fixture.example/slow".into(),
        headers: Vec::new(),
        body_handle: None,
        stream_body: false,
        follow_redirects: false,
        timeout_ms: Some(6000),
        origin_grant: None,
    })
}
fn reply(request: Id, offset: u64, len: usize, key: u8) -> Result<(), ErrorCode> {
    let bytes: Vec<u8> = (0..len)
        .map(|n| offset.wrapping_add(n as u64) as u8 ^ key)
        .collect();
    guest::Blob::new(&bytes)?.reply(request, false)?;
    Ok(())
}

fn event(value: Value) -> Reply<HostCommand> {
    let result = handle(value);
    match result {
        Ok(ok) => Reply::Ok { ok },
        Err(code) => Reply::Error {
            error: GuestError {
                code: code as i32,
                message: "invalid input".into(),
            },
        },
    }
}
fn handle(value: Value) -> Result<Vec<HostCommand>, ErrorCode> {
    if value["event"] == "window" {
        return handle(value["body"].clone());
    }
    let mut out = Vec::new();
    if value["vector"] == "decimal" {
        let id: Id =
            serde_json::from_value(value["id"].clone()).map_err(|_| ErrorCode::InvalidArgument)?;
        out.push(HostCommand::ShowToast {
            message: id.0.to_string(),
        });
    }
    match value["event"].as_str() {
        Some("init") if value["role"] == "control" && value["capabilities"].as_array().is_some_and(|caps| caps.iter().any(|cap| cap == "media_source")) => {
            out.push(HostCommand::SetSources { sources: vec![iris_plugin_sdk::api::v2::source::SourceDescriptor { id: "fixture.source".into(), name: "Fixture source".into(), ..Default::default() }] });
        }
        Some("source_resolve") => {
            let context = guest::Blob::new(&[0xa5])?.transfer();
            let command = serde_json::json!({"command":"resolve_stream", "request_id":value["request_id"], "media":{"stream":{"source_id":"fixture.source","entry_id":"av","kind":"video","source":{"mode":"random_access","resource_id":"av","size":null,"context":{"version":"1","blob":context,"transports":[]}}}}});
            out.push(serde_json::from_value(command).map_err(|_| ErrorCode::InvalidArgument)?);
        }
        Some("command") if value["id"] == "slow" => {
            slow()?;
        }
        Some("command") if value["id"] == "ping" => {
            out.push(HostCommand::ShowToast { message: "pong".into() });
        }
        Some("command") if value["id"] == "store_write" => {
            guest::submit(&Task::StateWrite { key: "fixture".into(), value: Some("persistent-value".into()) })?;
        }
        Some("command") if value["id"] == "store_read" => {
            guest::submit(&Task::StateRead { key: "fixture".into() })?;
        }
        Some("command") if value["id"] == "snapshot" => {
            out.push(HostCommand::ShowToast { message: value["snapshot"]["uri"].as_str().unwrap_or("none").into() });
        }
        Some("command") if value["id"] == "spin" => loop { std::hint::spin_loop(); },
        Some("stream_init" | "stream_context_changed") => {
            let id: Id = serde_json::from_value(value["context_blob"].clone())
                .map_err(|_| ErrorCode::InvalidArgument)?;
            let blob = guest::Blob::from_completion(id)?;
            let key = blob.read()?;
            if key.len() != 1 {
                return Err(ErrorCode::InvalidArgument);
            }
            CONTEXT.set(key[0]);
        }
        Some("slow") => {
            slow()?;
        }
        Some("completed") => {
            if value["result"]["ok"]["stored"] == true {
                guest::submit(&Task::StateRead { key: "fixture".into() })?;
                return Ok(out);
            }
            if value["result"]["ok"].get("store_value").is_some() {
                out.push(HostCommand::ShowToast { message: value["result"]["ok"]["store_value"].as_str().unwrap_or("absent").into() });
                return Ok(out);
            }
            let id: Id = serde_json::from_value(value["operation_id"].clone())
                .map_err(|_| ErrorCode::InvalidArgument)?;
            if let Some((request, offset, len, key)) = READS.with_borrow_mut(|r| r.remove(&id)) {
                reply(request, offset, len, key)?;
            } else {
                out.push(HostCommand::ShowToast {
                    message: "completed".into(),
                });
            }
        }
        Some("stream_read_cancelled") => {
            let id: Id = serde_json::from_value(value["request_id"].clone())
                .map_err(|_| ErrorCode::InvalidArgument)?;
            READS.with_borrow_mut(|r| r.retain(|_, entry| entry.0 != id));
        }
        Some("spin") => loop {
            std::hint::spin_loop();
        },
        Some("stream_read") => {
            let request: Id = serde_json::from_value(value["request_id"].clone())
                .map_err(|_| ErrorCode::InvalidArgument)?;
            let offset: Id = serde_json::from_value(value["offset"].clone())
                .map_err(|_| ErrorCode::InvalidArgument)?;
            let len = value["len"].as_u64().ok_or(ErrorCode::InvalidArgument)? as usize;
            if value["resource_id"] == "delayed" {
                let operation = slow()?;
                READS.with_borrow_mut(|r| {
                    r.insert(operation, (request, offset.0, len, CONTEXT.get()))
                });
            } else {
                reply(request, offset.0, len, CONTEXT.get())?;
            }
        }
        _ => {}
    }
    Ok(out)
}
iris_plugin_sdk::export_plugin!(event);
