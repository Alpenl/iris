//! Blocking I/O stays outside Store threads; stream workers are reserved.
use iris_api::v2::{ErrorCode, Role};
use iris_net::concurrency::{ThreadRole, budget, spawn_thread_named};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

pub(crate) type Job = Box<dyn FnOnce() + Send>;

const WORKER_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct State {
    stopped: bool,
    queued: usize,
    spawned: usize,
    next_worker: usize,
    order: VecDeque<String>,
    jobs: BTreeMap<String, VecDeque<Job>>,
    active: BTreeMap<String, usize>,
}
struct Lane {
    name: String,
    state: Mutex<State>,
    ready: Condvar,
    max_workers: usize,
    capacity: usize,
    owner_queue: usize,
    owner_active: usize,
    idle_timeout: Duration,
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
        Ok(Self(workers(name, workers_count, queue, queue, 1)))
    }
    pub(crate) fn submit(&self, owner: &str, job: Job) -> Result<(), ErrorCode> {
        submit(&self.0.0, owner, job)
    }
}

impl IoPool {
    pub fn new() -> Result<Self, ErrorCode> {
        Ok(Self {
            control: workers("control", budget::PLUGIN_CONTROL_IO_WORKERS, 96, 32, 2),
            stream: workers("stream", budget::PLUGIN_STREAM_IO_WORKERS, 32, 16, 4),
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

fn submit(lane: &Arc<Lane>, owner: &str, job: Job) -> Result<(), ErrorCode> {
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

    // Spawn worker threads lazily up to max_workers only for actually runnable work.
    let needed = needed_workers(&state, lane);
    while state.spawned < needed {
        let worker_id = state.next_worker;
        state.next_worker = state.next_worker.wrapping_add(1);
        state.spawned += 1;
        let thread_lane = lane.clone();
        if spawn_thread_named(
            ThreadRole::PluginPoolWorker,
            &format!("{}-io-{worker_id}", lane.name),
            move || worker(thread_lane),
        )
        .is_err()
        {
            state.spawned -= 1;
            if state.spawned == 0 {
                // Roll back the job we just enqueued since no worker exists to run it.
                if let Some(q) = state.jobs.get_mut(owner) {
                    let _ = q.pop_back();
                    if q.is_empty() {
                        state.jobs.remove(owner);
                        state.order.retain(|o| o != owner);
                    }
                }
                state.queued -= 1;
                return Err(ErrorCode::Internal);
            }
            break;
        }
    }
    lane.ready.notify_all();
    Ok(())
}

fn needed_workers(state: &State, lane: &Lane) -> usize {
    let active_total: usize = state.active.values().sum();
    let runnable: usize = state
        .jobs
        .iter()
        .map(|(o, q)| {
            let cur = state.active.get(o).copied().unwrap_or(0);
            q.len().min(lane.owner_active.saturating_sub(cur))
        })
        .sum();
    let min_alive = usize::from(state.queued > 0 && active_total == 0);
    (active_total + runnable)
        .max(min_alive)
        .min(lane.max_workers)
}

fn workers(
    name: &str,
    count: usize,
    capacity: usize,
    owner_queue: usize,
    owner_active: usize,
) -> Arc<Dispatcher> {
    workers_with_timeout(
        name,
        count,
        capacity,
        owner_queue,
        owner_active,
        WORKER_IDLE_TIMEOUT,
    )
}

fn workers_with_timeout(
    name: &str,
    count: usize,
    capacity: usize,
    owner_queue: usize,
    owner_active: usize,
    idle_timeout: Duration,
) -> Arc<Dispatcher> {
    Arc::new(Dispatcher(Arc::new(Lane {
        name: name.to_owned(),
        state: Mutex::new(State::default()),
        ready: Condvar::new(),
        max_workers: count,
        capacity,
        owner_queue,
        owner_active,
        idle_timeout,
    })))
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
                    state.spawned = state.spawned.saturating_sub(1);
                    return;
                }
                if let Some(job) = take(&mut state, lane.owner_active) {
                    break job;
                }
                let (next, timeout) = lane
                    .ready
                    .wait_timeout(state, lane.idle_timeout)
                    .unwrap_or_else(|e| e.into_inner());
                state = next;
                if timeout.timed_out()
                    && !state.stopped
                    && state.spawned > needed_workers(&state, &lane)
                {
                    state.spawned = state.spawned.saturating_sub(1);
                    return;
                }
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

    #[test]
    fn idle_extra_worker_exits_while_busy_owner_is_capped() {
        let disp = workers_with_timeout("capped", 4, 16, 8, 2, Duration::from_millis(20));
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));

        for _ in 0..3 {
            let gate = gate.clone();
            let started = started_tx.clone();
            let done = done_tx.clone();
            submit(
                &disp.0,
                "busy",
                Box::new(move || {
                    started.send("busy").unwrap();
                    let (lock, cv) = &*gate;
                    drop(cv.wait_while(lock.lock().unwrap(), |go| !*go).unwrap());
                    done.send("busy").unwrap();
                }),
            )
            .unwrap();
        }
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "busy"
        );
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "busy"
        );

        let other_done = done_tx.clone();
        submit(
            &disp.0,
            "other",
            Box::new(move || {
                other_done.send("other").unwrap();
            }),
        )
        .unwrap();
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "other"
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while disp.0.state.lock().unwrap().spawned > 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "extra idle worker remained parked despite owner_active cap"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(disp.0.state.lock().unwrap().queued, 1);

        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "busy"
        );
        for _ in 0..3 {
            assert_eq!(
                done_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
                "busy"
            );
        }
    }
}
