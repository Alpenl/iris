//! Blocking I/O stays outside Store threads; stream workers are reserved.
use iris_api::v2::{ErrorCode, Role};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Condvar, Mutex},
};

pub(crate) type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct State {
    stopped: bool,
    queued: usize,
    order: VecDeque<String>,
    jobs: BTreeMap<String, VecDeque<Job>>,
    active: BTreeMap<String, usize>,
}
struct Lane {
    state: Mutex<State>,
    ready: Condvar,
    capacity: usize,
    owner_queue: usize,
    owner_active: usize,
}
struct Dispatcher(Arc<Lane>);
impl Drop for Dispatcher {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.stopped = true;
        state.jobs.clear();
        state.order.clear();
        state.queued = 0;
        self.0.ready.notify_all();
    }
}

#[derive(Clone)]
pub struct IoPool {
    control: Arc<Dispatcher>,
    stream: Arc<Dispatcher>,
}

/// Filesystem/JIT admission is separate from live plugin I/O. Dropping the last
/// dispatcher cancels queued work; running jobs keep their own reservations.
#[derive(Clone)]
pub(crate) struct WorkPool(Arc<Dispatcher>);
impl WorkPool {
    pub(crate) fn new(name: &str, workers_count: usize, queue: usize) -> Result<Self, ErrorCode> {
        Ok(Self(workers(name, workers_count, queue, queue, 1)?))
    }
    pub(crate) fn submit(&self, owner: &str, job: Job) -> Result<(), ErrorCode> {
        submit(&self.0.0, owner, job)
    }
}

impl IoPool {
    pub fn new() -> Result<Self, ErrorCode> {
        Ok(Self {
            control: workers("control", 4, 96, 32, 2)?,
            stream: workers("stream", 8, 32, 16, 4)?,
        })
    }
    pub(crate) fn submit(&self, role: Role, owner: &str, job: Job) -> Result<(), ErrorCode> {
        let lane = &match role {
            Role::Control => &self.control,
            Role::Stream => &self.stream,
        }
        .0;
        submit(lane, owner, job)
    }
}

fn submit(lane: &Lane, owner: &str, job: Job) -> Result<(), ErrorCode> {
    let mut state = lane.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.stopped {
        return Err(ErrorCode::Cancelled);
    }
    if state.queued >= lane.capacity
        || state.jobs.get(owner).map_or(0, VecDeque::len) >= lane.owner_queue
    {
        return Err(ErrorCode::Backpressure);
    }
    if !state.jobs.contains_key(owner) {
        state.order.push_back(owner.to_owned());
    }
    state
        .jobs
        .entry(owner.to_owned())
        .or_default()
        .push_back(job);
    state.queued += 1;
    lane.ready.notify_all();
    Ok(())
}

fn workers(
    name: &str,
    count: usize,
    capacity: usize,
    owner_queue: usize,
    owner_active: usize,
) -> Result<Arc<Dispatcher>, ErrorCode> {
    let dispatcher = Arc::new(Dispatcher(Arc::new(Lane {
        state: Mutex::new(State::default()),
        ready: Condvar::new(),
        capacity,
        owner_queue,
        owner_active,
    })));
    for n in 0..count {
        let lane = dispatcher.0.clone();
        std::thread::Builder::new()
            .name(format!("iris-v2-{name}-io-{n}"))
            .spawn(move || worker(lane))
            .map_err(|_| ErrorCode::Internal)?;
    }
    Ok(dispatcher)
}

fn take(state: &mut State, limit: usize) -> Option<(String, Job)> {
    for _ in 0..state.order.len() {
        let owner = state.order.pop_front()?;
        if state.active.get(&owner).copied().unwrap_or(0) >= limit {
            state.order.push_back(owner);
            continue;
        }
        let jobs = state.jobs.get_mut(&owner)?;
        let job = jobs.pop_front()?;
        if jobs.is_empty() {
            state.jobs.remove(&owner);
        } else {
            state.order.push_back(owner.clone());
        }
        state.queued -= 1;
        *state.active.entry(owner.clone()).or_default() += 1;
        return Some((owner, job));
    }
    None
}

fn worker(lane: Arc<Lane>) {
    loop {
        let (owner, job) = {
            let mut state = lane.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stopped {
                    return;
                }
                if let Some(job) = take(&mut state, lane.owner_active) {
                    break job;
                }
                state = lane.ready.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
        let mut state = lane.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = state.active.get_mut(&owner) {
            *active -= 1;
            if *active == 0 {
                state.active.remove(&owner);
            }
        }
        lane.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn busy_plugin_cannot_occupy_all_workers_or_reserved_stream_lane() {
        let pool = IoPool::new().unwrap();
        let (started, rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        for _ in 0..4 {
            let gate = gate.clone();
            let started = started.clone();
            pool.submit(
                Role::Control,
                "busy",
                Box::new(move || {
                    started.send("busy").unwrap();
                    let (lock, cv) = &*gate;
                    drop(cv.wait_while(lock.lock().unwrap(), |done| !*done).unwrap());
                }),
            )
            .unwrap();
        }
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "busy");
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "busy");
        let other = started.clone();
        pool.submit(
            Role::Control,
            "other",
            Box::new(move || {
                other.send("other").unwrap();
            }),
        )
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "other");
        pool.submit(
            Role::Stream,
            "busy",
            Box::new(move || {
                started.send("stream").unwrap();
            }),
        )
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "stream");
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }
}
