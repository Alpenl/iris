use super::*;

fn fixture(body: &str, alloc: &str, free: &str) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(module
        (memory (export "memory") 1 8)
        (data (i32.const 1000) "{{\22ok\22:[]}}")
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) {alloc})
        (func (export "iris_dealloc") (param i32 i32) {free})
        (func (export "iris_on_event") (param i32 i32) (result i64) {body}))"#
    ))
    .unwrap()
}
fn instance(bytes: &[u8]) -> Instance {
    let engine = RuntimeEngine::new().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let module = engine.compile(bytes, &cancel).unwrap();
    Instance::new(InstanceConfig {
        engine,
        module,
        role: Role::Control,
        owner: "fixture".into(),
        grants: BTreeSet::new(),
        ledger: Ledger::default(),
        cancel,
    })
    .unwrap()
}

#[test]
fn grows_memory_then_returns_independent_output() {
    let bytes = fixture(
        "i32.const 1 memory.grow drop i64.const 4294967296009",
        "i32.const 100",
        "",
    );
    let mut guest = instance(&bytes);
    assert!(matches!(guest.on_event(b"{}"), Ok(Reply::Ok { ok }) if ok.is_empty()));
    assert!(guest.on_event(b"{}").is_ok());
}

#[test]
fn window_blob_handles_are_scoped_and_expired_on_seek_or_close() {
    let mut guest = instance(&fixture("i64.const 0", "i32.const 100", ""));
    let gen_a = iris_api::v2::Generation::default();
    let gen_b = iris_api::v2::Generation {
        media: Id(3),
        ..gen_a
    };
    let ledger = guest.store.data().ledger.clone();
    guest.select_context(Some(Id(1)), gen_a);
    let blob = Arc::new(Blob::new(&ledger, "fixture", vec![1; 16]).unwrap());
    let first = guest.deliver_blob(blob.clone()).unwrap();
    guest.select_context(Some(Id(2)), gen_b);
    assert!(!guest.store.data().blobs.contains_key(&first));
    let second = guest.deliver_blob(blob.clone()).unwrap();
    guest.select_context(Some(Id(1)), gen_a);
    assert!(guest.store.data().blobs.contains_key(&first));
    assert!(!guest.store.data().blobs.contains_key(&second));
    assert_eq!(ledger.used(None, Resource::Handle), 2);
    let next = iris_api::v2::Generation {
        seek: Id(1),
        ..gen_a
    };
    guest.retain_contexts(gen_a, &BTreeMap::from([(Id(1), next)]));
    assert!(guest.store.data().blobs.is_empty());
    assert_eq!(ledger.used(None, Resource::Handle), 0);
    guest.select_context(Some(Id(1)), next);
    assert!(guest.store.data().blobs.is_empty());
    drop(blob);
    drop(guest);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}

#[test]
fn malformed_output_and_allocator_traps_poison_only_this_instance() {
    for (body, alloc, free) in [
        ("i64.const 429496729602", "i32.const 100", ""), // aliases input
        ("i64.const -1", "i32.const 100", ""),
        ("unreachable", "i32.const 100", ""),
        ("i64.const 0", "i32.const 0", ""),
        ("i64.const 0", "i32.const 100", "unreachable"),
    ] {
        let mut guest = instance(&fixture(body, alloc, free));
        assert!(guest.on_event(b"{}").is_err());
        assert!(matches!(guest.on_event(b"{}"), Err(ErrorCode::BadState)));
    }
}

#[test]
fn epoch_interrupts_dead_loop_and_cancel_is_observed() {
    let mut guest = instance(&fixture(
        "(loop $forever br $forever) i64.const 0",
        "i32.const 100",
        "",
    ));
    let start = Instant::now();
    assert!(matches!(
        guest.call(b"{}", Duration::from_millis(40)),
        Err(ErrorCode::Timeout)
    ));
    assert!(start.elapsed() < Duration::from_secs(2));
    let mut guest = instance(&fixture("i64.const 0", "i32.const 100", ""));
    guest.store.data().cancel.store(true, Ordering::Release);
    assert!(matches!(guest.on_event(b"{}"), Err(ErrorCode::Cancelled)));
}

#[test]
fn rejects_wasi_shared_memory_and_bad_imports() {
    let engine = RuntimeEngine::new().unwrap();
    for source in [
        r#"(module (memory (export "memory") 1 1 shared))"#,
        r#"(module (memory (export "memory") i64 1))"#,
        r#"(module (memory (export "memory") 1) (memory 1))"#,
        r#"(module (import "wasi_snapshot_preview1" "fd_read" (func)) (memory (export "memory") 1))"#,
    ] {
        assert!(
            engine
                .compile(&wat::parse_str(source).unwrap(), &AtomicBool::new(false))
                .is_err()
        );
    }
}

#[test]
fn cache_keeps_active_modules_and_instance_reservation_until_drop() {
    let engine = RuntimeEngine::new().unwrap();
    let bytes = fixture("i64.const 0", "i32.const 100", "");
    let cancel = Arc::new(AtomicBool::new(false));
    let first = engine.compile(&bytes, &cancel).unwrap();
    let second = engine.compile(&bytes, &cancel).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert!(engine.cached_bytes() > 0);
    let ledger = Ledger::default();
    let guest = Instance::new(InstanceConfig {
        engine: engine.clone(),
        module: first,
        role: Role::Control,
        owner: "p".into(),
        grants: BTreeSet::new(),
        ledger: ledger.clone(),
        cancel: cancel.clone(),
    })
    .unwrap();
    assert!(
        Instance::new(InstanceConfig {
            engine,
            module: second,
            role: Role::Control,
            owner: "p".into(),
            grants: BTreeSet::new(),
            ledger: ledger.clone(),
            cancel: cancel.clone(),
        })
        .is_err()
    );
    cancel.store(true, Ordering::Release);
    assert_eq!(ledger.used(Some("p"), Resource::ControlStore), 1);
    drop(guest);
    assert_eq!(ledger.used(None, Resource::WasmBytes), 0);
}

#[test]
fn every_blob_handle_is_charged_even_when_data_is_shared() {
    let mut guest = instance(&fixture("i64.const 0", "i32.const 100", ""));
    let state = guest.store.data();
    let blob = Arc::new(Blob::new(&state.ledger, &state.owner, vec![1; 16]).unwrap());
    for _ in 0..128 {
        guest.deliver_blob(blob.clone()).unwrap();
    }
    assert_eq!(
        guest.deliver_blob(blob).unwrap_err(),
        ErrorCode::QuotaExceeded
    );
    assert_eq!(guest.store.data().ledger.used(None, Resource::Handle), 128);
    let other = Arc::new(Blob::new(&guest.store.data().ledger, "other", vec![1]).unwrap());
    assert_eq!(guest.deliver_blob(other).unwrap_err(), ErrorCode::Denied);
    let ledger = guest.store.data().ledger.clone();
    drop(guest);
    assert_eq!(ledger.used(None, Resource::Handle), 0);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}

#[test]
fn output_blob_transfer_is_atomic_and_keeps_its_ledger_lease() {
    let mut guest = instance(&fixture("i64.const 0", "i32.const 100", ""));
    let ledger = guest.store.data().ledger.clone();
    let data = Arc::new(Blob::new(&ledger, "fixture", b"context".to_vec()).unwrap());
    let id = Id(guest.deliver_blob(data).unwrap());
    assert!(guest.detach_blobs(&BTreeSet::from([id, Id(0)])).is_err());
    assert!(guest.store.data().blobs.contains_key(&id.0));
    let output = guest.detach_blobs(&BTreeSet::from([id])).unwrap();
    assert!(!guest.store.data().blobs.contains_key(&id.0));
    drop(guest);
    assert_eq!(ledger.used(None, Resource::Handle), 1);
    assert_eq!(output[&id].data.bytes, b"context");
    drop(output);
    assert_eq!(ledger.used(None, Resource::Handle), 0);
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
}

#[test]
fn wasm_start_cannot_invoke_host_imports_before_ready() {
    let bad_state = ErrorCode::BadState.as_i64();
    let bad_state_i32 = ErrorCode::BadState as i32;
    let bytes = wat::parse_str(format!(
        r#"(module
        (import "iris_v2" "host_clock" (func $clock (param i32) (result i64)))
        (import "iris_v2" "host_random" (func $random (param i32 i32) (result i32)))
        (import "iris_v2" "host_blob_len" (func $blob_len (param i64) (result i64)))
        (import "iris_v2" "host_blob_read" (func $blob_read (param i64 i64 i32 i32) (result i64)))
        (import "iris_v2" "host_close" (func $close (param i64) (result i32)))
        (import "iris_v2" "host_crypto" (func $crypto (param i32 i32 i32 i32 i32 i32) (result i64)))
        (memory (export "memory") 1 4)
        (func $start
            (if (i64.ne (call $clock (i32.const 0)) (i64.const {bad_state})) (then unreachable))
            (if (i32.ne (call $random (i32.const 64) (i32.const 8)) (i32.const {bad_state_i32})) (then unreachable))
            (if (i64.ne (call $blob_len (i64.const 1)) (i64.const {bad_state})) (then unreachable))
            (if (i64.ne (call $blob_read (i64.const 1) (i64.const 0) (i32.const 64) (i32.const 8)) (i64.const {bad_state})) (then unreachable))
            (if (i32.ne (call $close (i64.const 1)) (i32.const {bad_state_i32})) (then unreachable))
            (if (i64.ne (call $crypto (i32.const 64) (i32.const 2) (i32.const 64) (i32.const 0) (i32.const 128) (i32.const 32)) (i64.const {bad_state})) (then unreachable)))
        (start $start)
        (func (export "iris_abi_version") (result i32) i32.const 2)
        (func (export "iris_alloc") (param i32) (result i32) i32.const 1024)
        (func (export "iris_dealloc") (param i32 i32))
        (func (export "iris_on_event") (param i32 i32) (result i64)
            (if (i64.lt_s (call $clock (i32.const 1)) (i64.const 0)) (then unreachable))
            i64.const 0))"#
    ))
    .unwrap();
    let mut guest = instance(&bytes);
    assert!(matches!(guest.on_event(b"{}"), Ok(Reply::Ok { ok }) if ok.is_empty()));
}
