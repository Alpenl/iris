use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Default)]
struct Service {
    closed: AtomicU64,
    stopped: AtomicBool,
    busy: AtomicBool,
    generation: Mutex<Generation>,
}
impl Executor for Service {
    fn prepare(
        &self,
        _: Task,
        _: &BTreeMap<u64, Arc<Blob>>,
        _: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        Err(ErrorCode::Unsupported)
    }
    fn close(&self, handle: u64) -> Result<(), ErrorCode> {
        self.closed.store(handle, Ordering::Release);
        Ok(())
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
    fn generation_changed(&self, generation: Generation) {
        *self.generation.lock().unwrap() = generation;
    }
    fn is_quiescent(&self) -> bool {
        !self.busy.load(Ordering::Acquire)
    }
}

#[test]
fn windows_route_closes_and_retire_without_stopping_neighbors() {
    let ledger = Ledger::default();
    let global = Arc::new(Service::default());
    let router = WindowServices::new("windows".into(), ledger.clone(), global.clone());
    let a = Arc::new(Service::default());
    let b = Arc::new(Service::default());
    router
        .register(Id(1), Generation::default(), a.clone())
        .unwrap();
    router
        .register(Id(2), Generation::default(), b.clone())
        .unwrap();
    router.close_in(Some(Id(2)), 19).unwrap();
    assert_eq!(b.closed.load(Ordering::Acquire), 19);
    assert_eq!(a.closed.load(Ordering::Acquire), 0);
    assert_eq!(global.closed.load(Ordering::Acquire), 0);
    let next = Generation {
        media: Id(3),
        ..Generation::default()
    };
    router.window_changed(Id(1), Some(next));
    assert_eq!(*a.generation.lock().unwrap(), next);
    assert_eq!(*b.generation.lock().unwrap(), Generation::default());
    assert!(matches!(
        router.executor(Some(Id(1)), Some(Generation::default())),
        Err(ErrorCode::StaleHandle)
    ));
    a.busy.store(true, Ordering::Release);
    router.window_changed(Id(1), None);
    assert!(a.stopped.load(Ordering::Acquire));
    assert!(!b.stopped.load(Ordering::Acquire));
    assert_eq!(
        router.close_in(Some(Id(1)), 19),
        Err(ErrorCode::StaleHandle)
    );
    assert!(!router.is_quiescent());
    assert_eq!(ledger.used(None, Resource::Handle), 2);
    a.busy.store(false, Ordering::Release);
    assert!(router.is_quiescent());
    assert_eq!(ledger.used(None, Resource::Handle), 1);
    router.stop();
    assert!(b.stopped.load(Ordering::Acquire));
    assert!(global.stopped.load(Ordering::Acquire));
    assert!(router.is_quiescent());
    assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    assert_eq!(
        router.register(Id(3), next, Arc::new(Service::default())),
        Err(ErrorCode::Cancelled)
    );
}

#[test]
fn rapidly_closed_windows_cannot_evade_executor_capacity() {
    let router = WindowServices::new(
        "windows".into(),
        Ledger::default(),
        Arc::new(Service::default()),
    );
    let mut retired = Vec::new();
    for id in 1..=8 {
        let service = Arc::new(Service::default());
        service.busy.store(true, Ordering::Release);
        router
            .register(Id(id), Generation::default(), service.clone())
            .unwrap();
        router.window_changed(Id(id), None);
        retired.push(service);
    }
    assert_eq!(
        router.register(Id(9), Generation::default(), Arc::new(Service::default())),
        Err(ErrorCode::Backpressure)
    );
    retired[0].busy.store(false, Ordering::Release);
    router
        .register(Id(9), Generation::default(), Arc::new(Service::default()))
        .unwrap();
}
