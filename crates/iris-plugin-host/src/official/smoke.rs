use super::{Bootstrap, GUIDES, WEBDAV};
use crate::{
    engine::RuntimeEngine,
    instance::{Instance, InstanceConfig},
    ledger::{Ledger, Resource},
    package::{Package, Provenance},
};
use iris_api::v2::{ErrorCode, Reply, Role};
use iris_base::rt::CancelToken;
use std::{sync::Arc, time::Instant};

/// Executes embedded, digest-verified Wasm in the actual host process. No UI,
/// installed packages, profile writes, network, credentials or native tools.
pub fn check() -> Result<serde_json::Value, ErrorCode> {
    let pins = Bootstrap::embedded(std::path::PathBuf::new())?.pins();
    let engine = RuntimeEngine::new()?;
    let ledger = Ledger::default();
    let mut rows = Vec::new();
    for bytes in [GUIDES, WEBDAV] {
        let package = Package::from_zip(bytes, Provenance::Bundled(&pins))?;
        let cancel = CancelToken::new();
        let start = Instant::now();
        let module = engine.compile(package.wasm().ok_or(ErrorCode::Unsupported)?, &cancel)?;
        let compile = start.elapsed();
        let start = Instant::now();
        let cached = engine.compile(package.wasm().ok_or(ErrorCode::Unsupported)?, &cancel)?;
        let cache_hit = start.elapsed();
        if !Arc::ptr_eq(&module, &cached) {
            return Err(ErrorCode::Internal);
        }
        drop(cached);
        let start = Instant::now();
        let mut instance = Instance::new(InstanceConfig {
            engine: engine.clone(),
            module,
            role: Role::Control,
            owner: "host-runtime-smoke".into(),
            grants: package.manifest.permissions.iter().copied().collect(),
            ledger: ledger.clone(),
            cancel,
        })?;
        let instantiate = start.elapsed();
        let start = Instant::now();
        match instance.on_event(br#"{"event":"shutdown"}"#)? {
            Reply::Ok { ok } if ok.is_empty() => {}
            _ => return Err(ErrorCode::Internal),
        }
        rows.push(serde_json::json!({"plugin":package.manifest.id,"content_sha256":package.digest.iter().map(|b|format!("{b:02x}")).collect::<String>(),"wasm_bytes":package.wasm().map_or(0, |b| b.len()),"compile_ms":compile.as_secs_f64()*1000.,"cache_hit_ms":cache_hit.as_secs_f64()*1000.,"instantiate_ms":instantiate.as_secs_f64()*1000.,"callback_ms":start.elapsed().as_secs_f64()*1000.}));
        drop(instance);
    }
    if ledger.used(None, Resource::WasmBytes) != 0 || ledger.used(None, Resource::Handle) != 0 {
        return Err(ErrorCode::Internal);
    }
    Ok(
        serde_json::json!({"status":"ok","protocol":iris_api::PROTOCOL_VERSION,"abi":iris_api::v2::ABI_VERSION,"engine":RuntimeEngine::TARGET,"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"debug":cfg!(debug_assertions),"plugins":rows}),
    )
}
