//! In-memory compilation only. Active modules are part of the cache budget.
use super::{
    imports::{self, State},
    package::WASM_LIMIT,
};
use iris_api::v2::ErrorCode;
use iris_base::rt::{CancelToken, ThreadRole, spawn_thread};
use ring::digest::{SHA256, digest};
use std::{
    collections::BTreeMap,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};
use wasmtime::{Config, Engine, ExternType, Linker, Module};

const CACHE_LIMIT: usize = 256 << 20;

pub(crate) struct CompiledModule {
    pub(crate) module: Module,
    cost: usize,
}
struct Cache {
    modules: BTreeMap<[u8; 32], Arc<CompiledModule>>,
    compiling: usize,
    waiting: usize,
}
struct Timer {
    /// `(active_executions, stopping, thread_started)`
    state: Mutex<(usize, bool, bool)>,
    changed: Condvar,
}
struct Inner {
    engine: Engine,
    linker: Linker<State>,
    cache: Mutex<Cache>,
    changed: Condvar,
    timer: Arc<Timer>,
}

#[derive(Clone)]
pub(crate) struct RuntimeEngine(Arc<Inner>);
pub(crate) struct ExecutionGuard(RuntimeEngine);

impl RuntimeEngine {
    pub(crate) const TARGET: &'static str = "pulley64";

    pub(crate) fn new() -> Result<Self, ErrorCode> {
        Self::with_pulley(true)
    }

    fn with_pulley(pulley: bool) -> Result<Self, ErrorCode> {
        let mut config = Config::new();
        if pulley {
            config
                .target(Self::TARGET)
                .map_err(|_| ErrorCode::Internal)?;
        }
        config
            .epoch_interruption(true)
            .wasm_multi_memory(false)
            .wasm_memory64(false)
            .wasm_shared_everything_threads(false)
            .wasm_relaxed_simd(false)
            .max_wasm_stack(512 << 10);
        let engine = Engine::new(&config).map_err(|_| ErrorCode::Internal)?;
        let linker = imports::linker(&engine)?;
        let timer = Arc::new(Timer {
            state: Mutex::new((0, false, false)),
            changed: Condvar::new(),
        });
        let inner = Arc::new(Inner {
            engine,
            linker,
            cache: Mutex::new(Cache {
                modules: BTreeMap::new(),
                compiling: 0,
                waiting: 0,
            }),
            changed: Condvar::new(),
            timer,
        });
        Ok(Self(inner))
    }

    pub(crate) fn engine(&self) -> &Engine {
        &self.0.engine
    }
    pub(crate) fn linker(&self) -> &Linker<State> {
        &self.0.linker
    }
    pub(crate) fn execution(&self) -> Result<ExecutionGuard, ErrorCode> {
        self.execution_with(|job| {
            spawn_thread(ThreadRole::WasmEpochTimer, job)
                .map(|_| ())
                .map_err(|_| ErrorCode::Internal)
        })
    }
    fn execution_with(
        &self,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> Result<(), ErrorCode>,
    ) -> Result<ExecutionGuard, ErrorCode> {
        let mut state = self.0.timer.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 += 1;
        if !state.2 && !state.1 {
            let thread_timer = self.0.timer.clone();
            let thread_engine = self.0.engine.clone();
            if let Err(err) = spawn(Box::new(move || {
                let mut state = thread_timer.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    while state.0 == 0 && !state.1 {
                        state = thread_timer
                            .changed
                            .wait(state)
                            .unwrap_or_else(|e| e.into_inner());
                    }
                    if state.1 {
                        break;
                    }
                    let (next, timeout) = thread_timer
                        .changed
                        .wait_timeout(state, Duration::from_millis(10))
                        .unwrap_or_else(|e| e.into_inner());
                    state = next;
                    if state.0 > 0 && timeout.timed_out() {
                        thread_engine.increment_epoch();
                    }
                }
            })) {
                state.0 -= 1;
                return Err(err);
            }
            state.2 = true;
        }
        if state.0 == 1 {
            self.0.timer.changed.notify_one();
        }
        Ok(ExecutionGuard(self.clone()))
    }

    /// Run only on a background worker. Compilation cannot be epoch-interrupted;
    /// queued cancellation is observed and the two compiler slots are bounded.
    pub(crate) fn compile(
        &self,
        bytes: &[u8],
        cancel: &CancelToken,
    ) -> Result<Arc<CompiledModule>, ErrorCode> {
        if bytes.len() > WASM_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
        if !bytes.starts_with(b"\0asm\x01\0\0\0") {
            return Err(ErrorCode::Unsupported);
        }
        let key: [u8; 32] = digest(&SHA256, bytes)
            .as_ref()
            .try_into()
            .map_err(|_| ErrorCode::Internal)?;
        let mut cache = self.0.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if let Some(module) = cache.modules.get(&key) {
            return Ok(module.clone());
        }
        if cache.compiling >= 2 {
            if cache.waiting >= 4 {
                return Err(ErrorCode::Backpressure);
            }
            cache.waiting += 1;
            while cache.compiling >= 2 && !cancel.is_cancelled() {
                cache = self
                    .0
                    .changed
                    .wait_timeout(cache, Duration::from_millis(50))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            cache.waiting -= 1;
            if cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            if let Some(module) = cache.modules.get(&key) {
                return Ok(module.clone());
            }
        }
        // Evict only idle modules, never make live code disappear from accounting.
        cache.modules.retain(|_, m| Arc::strong_count(m) > 1);
        if cache.modules.values().map(|m| m.cost).sum::<usize>() >= CACHE_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
        cache.compiling += 1;
        drop(cache);
        let result = Module::from_binary(self.engine(), bytes)
            .map_err(|_| ErrorCode::InvalidArgument)
            .and_then(validate_module);
        let mut cache = self.0.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.compiling -= 1;
        self.0.changed.notify_all();
        let module = result?;
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if let Some(existing) = cache.modules.get(&key) {
            return Ok(existing.clone());
        }
        cache.modules.retain(|_, m| Arc::strong_count(m) > 1);
        let image = module.image_range();
        // Image bytes plus bounded input/type metadata allowance, not an RSS claim.
        let cost = (image.end as usize - image.start as usize).saturating_add(bytes.len());
        if cost > CACHE_LIMIT.saturating_sub(cache.modules.values().map(|m| m.cost).sum()) {
            return Err(ErrorCode::QuotaExceeded);
        }
        let module = Arc::new(CompiledModule { module, cost });
        cache.modules.insert(key, module.clone());
        Ok(module)
    }
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.0
            .0
            .timer
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0 -= 1;
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.timer.state.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.timer.changed.notify_one();
    }
}

fn validate_module(module: Module) -> Result<Module, ErrorCode> {
    let resources = module.resources_required();
    if resources.num_memories != 1
        || resources.num_tables > 1
        || resources.max_initial_table_size.unwrap_or(0) > 16384
    {
        return Err(ErrorCode::Unsupported);
    }
    match module.get_export("memory") {
        Some(ExternType::Memory(m)) if !m.is_64() && !m.is_shared() => {}
        _ => return Err(ErrorCode::Unsupported),
    }
    const IMPORTS: &[&str] = &[
        "host_submit",
        "host_blob_new",
        "host_blob_len",
        "host_blob_read",
        "host_close",
        "host_stream_reply",
        "host_stream_fail",
        "host_crypto",
        "host_random",
        "host_clock",
    ];
    for import in module.imports() {
        if import.module() != "iris_v2"
            || !IMPORTS.contains(&import.name())
            || !matches!(import.ty(), ExternType::Func(_))
        {
            return Err(ErrorCode::Unsupported);
        }
    }
    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::RuntimeEngine;
    use crate::{
        instance::{Instance, InstanceConfig},
        ledger::{Ledger, Resource},
    };
    use iris_api::v2::{ErrorCode, Role};
    use iris_base::rt::CancelToken;
    use std::collections::BTreeSet;

    impl RuntimeEngine {
        pub(crate) fn cached_bytes(&self) -> usize {
            self.0
                .cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .modules
                .values()
                .map(|m| m.cost)
                .sum()
        }
    }

    #[test]
    fn execution_spawn_failure_rolls_back_active_count_and_fails_closed() {
        let engine = RuntimeEngine::new().unwrap();
        let wasm = wat::parse_str(
            r#"(module
            (memory (export "memory") 1 2)
            (func (export "iris_abi_version") (result i32) i32.const 2)
            (func (export "iris_alloc") (param i32) (result i32) i32.const 1024)
            (func (export "iris_dealloc") (param i32 i32))
            (func (export "iris_on_event") (param i32 i32) (result i64) i64.const 0))"#,
        )
        .unwrap();
        let module = engine.compile(&wasm, &CancelToken::new()).unwrap();
        let ledger = Ledger::default();

        assert_eq!(
            engine.execution_with(|_| Err(ErrorCode::Internal)).err(),
            Some(ErrorCode::Internal)
        );
        assert_eq!(
            *engine
                .0
                .timer
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
            (0, false, false)
        );

        let inst = Instance::new(InstanceConfig {
            engine: engine.clone(),
            module,
            role: Role::Control,
            owner: "epoch-ok".into(),
            grants: BTreeSet::new(),
            ledger: ledger.clone(),
            cancel: CancelToken::new(),
        })
        .unwrap();
        assert!(
            engine
                .0
                .timer
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .2
        );
        drop(inst);
        assert_eq!(ledger.used(None, Resource::WasmBytes), 0);
    }

    #[test]
    #[ignore = "comparative Cranelift vs Pulley64 micro-benchmark for decision record"]
    fn compare_pulley64_vs_cranelift_dispatch_and_plugins() {
        use crate::package::{Package, Provenance};
        use std::time::Instant;

        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let pins = crate::official::Bootstrap::embedded(std::path::PathBuf::new())
            .unwrap()
            .pins();
        let mut cases: Vec<(&str, Vec<u8>, bool)> = vec![
            (
                "iris.guides",
                std::fs::read(root.join("Resources/Plugins/iris.guides.irisplugin")).unwrap(),
                true,
            ),
            (
                "iris.webdav",
                std::fs::read(root.join("Resources/Plugins/iris.webdav.irisplugin")).unwrap(),
                true,
            ),
        ];
        let bili_zip = root.join("target/plugin-v2/examples/community.bilibili.irisplugin");
        if let Ok(bytes) = std::fs::read(&bili_zip) {
            cases.push(("community.bilibili", bytes, false));
        }

        for (mode, pulley) in [("cranelift", false), ("pulley64", true)] {
            let engine = RuntimeEngine::with_pulley(pulley).unwrap();
            let ledger = Ledger::default();
            for (label, zip_bytes, bundled) in &cases {
                let prov = if *bundled {
                    Provenance::Bundled(&pins)
                } else {
                    Provenance::User
                };
                let pkg = Package::from_zip(zip_bytes, prov).unwrap();
                let wasm = pkg.wasm().unwrap();
                let cancel = CancelToken::new();
                let t0 = Instant::now();
                let module = engine.compile(wasm, &cancel).unwrap();
                let compile_ms = t0.elapsed().as_secs_f64() * 1000.0;

                let t1 = Instant::now();
                let mut inst = Instance::new(InstanceConfig {
                    engine: engine.clone(),
                    module,
                    role: Role::Control,
                    owner: format!("bench-{mode}-{label}"),
                    grants: pkg.manifest.permissions.iter().copied().collect(),
                    ledger: ledger.clone(),
                    cancel,
                })
                .unwrap();
                let inst_ms = t1.elapsed().as_secs_f64() * 1000.0;

                let t2 = Instant::now();
                for _ in 0..1_000 {
                    let _ = inst.on_event(br#"{"event":"shutdown"}"#).unwrap();
                }
                let dispatch_1000_ms = t2.elapsed().as_secs_f64() * 1000.0;
                println!(
                    "BENCH mode={mode} plugin={label} wasm_bytes={} compile_ms={compile_ms:.3} instantiate_ms={inst_ms:.3} dispatch_1000_ms={dispatch_1000_ms:.3}",
                    wasm.len()
                );
            }
        }
    }
}
