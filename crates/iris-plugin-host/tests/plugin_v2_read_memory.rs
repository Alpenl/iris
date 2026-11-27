//! A dedicated test executable keeps allocation measurements independent of
//! other parallel tests. Count live Rust allocations, including the actor thread,
//! because the resource ledger alone cannot detect unaccounted read history.
use iris_plugin_host::StreamReadMemoryProbe;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicIsize, Ordering},
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

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "actor did not settle");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn read_batch(probe: &StreamReadMemoryProbe, range: std::ops::Range<u64>) -> isize {
    for offset in range {
        let bytes = probe
            .read_single_byte(offset, Duration::from_secs(5))
            .unwrap();
        assert_eq!(bytes, [offset as u8]);
    }
    until(|| probe.host_bytes_used() == 0);
    assert_eq!(probe.stream_reads_used(), 0);
    LIVE_BYTES.load(Ordering::Relaxed)
}

#[test]
#[ignore = "requires compiled Rust guest; run scripts/test-plugin-v2.py"]
fn completed_wasm_reads_do_not_accumulate_unaccounted_host_memory() {
    let wasm =
        std::fs::read(std::env::var_os("IRIS_V2_RUST_GUEST").expect("Rust guest fixture")).unwrap();
    let probe = StreamReadMemoryProbe::spawn(&wasm).unwrap();

    // Warm compilation/runtime caches, then keep reading without a seek/sweep.
    // The old delivered_reads set retained about 294 KiB over these 15,000 reads
    // even though both ledger counters were zero at every batch boundary.
    let baseline = read_batch(&probe, 0..5000);
    let after = read_batch(&probe, 5000..20_000);
    probe.stop();
    until(|| probe.is_quiescent());
    assert!(
        after - baseline < 64 * 1024,
        "completed reads retained {} bytes (baseline={baseline}, after={after})",
        after - baseline,
    );
}
