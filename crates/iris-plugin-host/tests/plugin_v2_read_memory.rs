//! A dedicated test executable keeps allocation measurements independent of
//! other parallel tests. Count live Rust allocations, including the actor thread,
//! because the resource ledger alone cannot detect unaccounted read history.
use iris_api::v2::{ErrorCode, Generation, Role, task::Task};
use iris_core::concurrency::CancelToken;
use iris_plugin_host::{
    actor::{Actor, Executor, Operation, Spawn, Status, TaskContext},
    engine::RuntimeEngine,
    instance::Blob,
    ledger::{Ledger, Resource},
    pool::IoPool,
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicIsize, Ordering},
    },
    time::{Duration, Instant},
};

struct CountingAllocator;
static LIVE_BYTES: AtomicIsize = AtomicIsize::new(0);

// SAFETY: All allocation operations use System with the caller's unchanged
// pointer/layout. The atomic counter only observes successful operations.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE_BYTES.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE_BYTES.fetch_sub(layout.size() as isize, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(ptr, layout, size) };
        if !result.is_null() {
            LIVE_BYTES.fetch_add(size as isize - layout.size() as isize, Ordering::Relaxed);
        }
        result
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

struct NoIo;
impl Executor for NoIo {
    fn prepare(
        &self,
        _: Task,
        _: &BTreeMap<u64, Arc<Blob>>,
        _: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        Err(ErrorCode::Unsupported)
    }
    fn close(&self, _: u64) -> Result<(), ErrorCode> {
        Err(ErrorCode::StaleHandle)
    }
    fn stop(&self) {}
}

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "actor did not settle");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn read_batch(actor: &Actor, ledger: &Ledger, range: std::ops::Range<u64>) -> isize {
    for offset in range {
        let ticket = actor.read("resource".into(), offset, 1, None).unwrap();
        let result = ticket.receive(Duration::from_secs(5)).unwrap();
        assert_eq!(result.blob.bytes, [offset as u8]);
    }
    until(|| ledger.used(None, Resource::HostBytes) == 0);
    assert_eq!(ledger.used(None, Resource::StreamRead), 0);
    LIVE_BYTES.load(Ordering::Relaxed)
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn completed_wasm_reads_do_not_accumulate_unaccounted_host_memory() {
    let engine = RuntimeEngine::new().unwrap();
    let wasm =
        std::fs::read(std::env::var_os("IRIS_V2_RUST_GUEST").expect("Rust guest fixture")).unwrap();
    let module = engine.compile(&wasm, &CancelToken::new()).unwrap();
    let ledger = Ledger::default();
    let actor = Actor::spawn(Spawn {
        engine,
        module,
        role: Role::Stream,
        owner: "read-memory-regression".into(),
        grants: BTreeSet::new(),
        ledger: ledger.clone(),
        io: IoPool::new().unwrap(),
        executor: Arc::new(NoIo),
        generation: Generation::default(),
        wake: Arc::new(|| {}),
    })
    .unwrap();
    until(|| actor.status() == Status::Ready);
    actor
        .send(br#"{"event":"init","role":"stream"}"#.to_vec())
        .unwrap();
    until(|| actor.initialized());

    // Warm compilation/runtime caches, then keep reading without a seek/sweep.
    // The old delivered_reads set retained about 294 KiB over these 15,000 reads
    // even though both ledger counters were zero at every batch boundary.
    let baseline = read_batch(&actor, &ledger, 0..5000);
    let after = read_batch(&actor, &ledger, 5000..20_000);
    actor.stop();
    until(|| actor.is_quiescent());
    assert!(
        after - baseline < 64 * 1024,
        "completed reads retained {} bytes (baseline={baseline}, after={after})",
        after - baseline,
    );
}
