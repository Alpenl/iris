use super::identity::{PinnedFile, canonical_host_dir};
use crate::{
    crypto::random_identity,
    instance::next_id,
    ledger::{Activity, ActivityLease, Ledger, Reservation, Resource},
};
use cap_std::fs::{Dir, OpenOptions};
use iris_api::v2::ErrorCode;
use iris_base::net::http_client::HttpCancel;
use std::{
    fs::File,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, PoisonError},
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub(crate) struct TempDirHandle {
    dir: Option<Dir>,
    parent: Option<Dir>,
    name: String,
    parent_path: PathBuf,
}

impl TempDirHandle {
    pub(crate) fn directory(&self) -> &Dir {
        self.dir.as_ref().expect("live temporary directory")
    }
}

impl Drop for TempDirHandle {
    fn drop(&mut self) {
        drop(self.dir.take());
        if let Some(parent) = self.parent.take() {
            let _ = parent.remove_dir(&self.name);
        }
        // The parent handle is closed before removing the now-empty
        // per-installation directory, so the cleanup does not depend on
        // whether the platform allows removing a directory that is still open.
        let _ = std::fs::remove_dir(&self.parent_path);
    }
}

struct Cleanup {
    dir: Arc<TempDirHandle>,
    name: String,
    _leases: Vec<Reservation>,
    _activities: Vec<ActivityLease>,
}

#[derive(Clone, Copy)]
pub(crate) struct DrainPolicy {
    pub poll_interval: Duration,
    pub drain_retry_limit: usize,
    pub drain_retry_delay: Duration,
    pub drain_timeout: Duration,
    pub join_timeout: Duration,
}

impl Default for DrainPolicy {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(250),
            drain_retry_limit: 5,
            drain_retry_delay: Duration::from_millis(20),
            drain_timeout: Duration::from_millis(250),
            join_timeout: Duration::from_millis(500),
        }
    }
}

type RemoveFn = dyn Fn(&TempDirHandle, &str) -> std::io::Result<()> + Send + Sync;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Running,
    ShuttingDown,
    Detached,
    Stopped,
}

struct CleanerState {
    phase: Phase,
    pending: Vec<Cleanup>,
    worker: Option<JoinHandle<()>>,
}

struct CleanerShared {
    state: Mutex<CleanerState>,
    changed: Condvar,
    done: Condvar,
    policy: DrainPolicy,
    remover: Option<Arc<RemoveFn>>,
}

impl CleanerShared {
    fn new(policy: DrainPolicy) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CleanerState {
                phase: Phase::Idle,
                pending: Vec::new(),
                worker: None,
            }),
            changed: Condvar::new(),
            done: Condvar::new(),
            policy,
            remover: None,
        })
    }

    fn try_remove_ok(&self, job: &Cleanup) -> std::io::Result<()> {
        let res = match &self.remover {
            Some(remover) => remover(&job.dir, &job.name),
            None => job.dir.directory().remove_file(&job.name),
        };
        match res {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn delete_sync_or_retain(&self, job: Cleanup) {
        if let Err(err) = self.try_remove_ok(&job) {
            log::warn!(
                "post-shutdown synchronous artifact cleanup failed for `{}`: {err}; retaining file and ledger quota",
                job.name
            );
            std::mem::forget(job);
        }
    }

    fn shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match state.phase {
            Phase::Idle => {
                state.phase = Phase::Stopped;
                return;
            }
            Phase::Detached => return,
            Phase::Stopped => {
                let handle = state.worker.take();
                drop(state);
                if let Some(handle) = handle {
                    let _ = handle.join();
                }
                return;
            }
            Phase::Running => {
                state.phase = Phase::ShuttingDown;
                self.changed.notify_all();
            }
            Phase::ShuttingDown => {}
        }
        let deadline = Instant::now() + self.policy.join_timeout;
        while state.phase != Phase::Stopped {
            if state.phase == Phase::Detached {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                log::warn!(
                    "artifact cleanup worker did not exit within {:?} join timeout; detaching cleanup thread",
                    self.policy.join_timeout
                );
                state.phase = Phase::Detached;
                let _detached = state.worker.take();
                self.done.notify_all();
                return;
            }
            let (next, _) = self
                .done
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
        }
        let handle = state.worker.take();
        drop(state);
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

fn worker_loop(shared: Arc<CleanerShared>) {
    let mut batch = Vec::<Cleanup>::new();
    loop {
        let phase = {
            let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
            while state.phase == Phase::Running && state.pending.is_empty() && batch.is_empty() {
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if state.phase == Phase::Running && state.pending.is_empty() && !batch.is_empty() {
                let (next, _) = shared
                    .changed
                    .wait_timeout(state, shared.policy.poll_interval)
                    .unwrap_or_else(PoisonError::into_inner);
                state = next;
            }
            batch.append(&mut state.pending);
            state.phase
        };

        if matches!(
            phase,
            Phase::ShuttingDown | Phase::Detached | Phase::Stopped
        ) {
            if phase == Phase::ShuttingDown {
                drain_on_shutdown(&shared, &mut batch);
            } else {
                retain_undeleted(&shared, &mut batch);
            }
            let mut extra = {
                let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
                std::mem::take(&mut state.pending)
            };
            if !extra.is_empty() {
                retain_undeleted(&shared, &mut extra);
            }
            let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.phase = Phase::Stopped;
            shared.done.notify_all();
            return;
        }

        // Failed deletion keeps its handle/disk quota. There can be at most 1024
        // jobs including live artifacts; no retry queue grows independently of
        // the global handle ledger.
        batch.retain(|job| shared.try_remove_ok(job).is_err());
    }
}

fn drain_on_shutdown(shared: &CleanerShared, batch: &mut Vec<Cleanup>) {
    if batch.is_empty() {
        return;
    }
    let deadline = Instant::now() + shared.policy.drain_timeout;
    let max_attempts = shared.policy.drain_retry_limit.max(1);
    let mut last_errors: Vec<(String, std::io::Error)> = Vec::new();
    for attempt in 0..max_attempts {
        last_errors.clear();
        batch.retain(|job| match shared.try_remove_ok(job) {
            Ok(()) => false,
            Err(err) => {
                last_errors.push((job.name.clone(), err));
                true
            }
        });
        if batch.is_empty() {
            return;
        }
        let detached = shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .phase
            == Phase::Detached;
        let now = Instant::now();
        if detached || attempt + 1 >= max_attempts || now >= deadline {
            break;
        }
        let sleep_for = shared.policy.drain_retry_delay.min(deadline - now);
        if !sleep_for.is_zero() {
            std::thread::sleep(sleep_for);
        }
    }
    let remaining = batch.len();
    for (name, err) in &last_errors {
        log::warn!(
            "artifact cleanup drain exhausted after bounded retries for `{name}` ({remaining} remaining): {err}; retaining file and ledger quota"
        );
    }
    for job in batch.drain(..) {
        std::mem::forget(job);
    }
}

fn retain_undeleted(shared: &CleanerShared, batch: &mut Vec<Cleanup>) {
    batch.retain(|job| match shared.try_remove_ok(job) {
        Ok(()) => false,
        Err(err) => {
            log::warn!(
                "artifact cleanup failed during detached shutdown for `{}`: {err}; retaining file and ledger quota",
                job.name
            );
            true
        }
    });
    for job in batch.drain(..) {
        std::mem::forget(job);
    }
}

#[derive(Clone)]
pub(crate) struct CleanerQueue(Arc<CleanerShared>);

impl CleanerQueue {
    pub(crate) fn ensure_started(&self) -> Result<(), ErrorCode> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.phase == Phase::Idle {
            let shared = self.0.clone();
            let handle = iris_base::rt::spawn_thread(
                iris_base::rt::ThreadRole::ArtifactCleanup,
                move || worker_loop(shared),
            )
            .map_err(|_| ErrorCode::Internal)?;
            state.phase = Phase::Running;
            state.worker = Some(handle);
        }
        Ok(())
    }

    fn enqueue(&self, job: Cleanup) {
        let sync_job = {
            let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
            match state.phase {
                Phase::Idle => {
                    let shared = self.0.clone();
                    match iris_base::rt::spawn_thread(
                        iris_base::rt::ThreadRole::ArtifactCleanup,
                        move || worker_loop(shared),
                    ) {
                        Ok(handle) => {
                            state.phase = Phase::Running;
                            state.worker = Some(handle);
                            state.pending.push(job);
                            self.0.changed.notify_one();
                            None
                        }
                        Err(_) => Some(job),
                    }
                }
                Phase::Running => {
                    state.pending.push(job);
                    self.0.changed.notify_one();
                    None
                }
                Phase::ShuttingDown | Phase::Detached | Phase::Stopped => Some(job),
            }
        };
        if let Some(job) = sync_job {
            self.0.delete_sync_or_retain(job);
        }
    }
}

struct CleanerOwner(Arc<CleanerShared>);

impl Drop for CleanerOwner {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

/// Host-owned background cleanup queue and worker lifecycle for temporary artifacts.
#[derive(Clone)]
pub(crate) struct ArtifactCleaner(Arc<CleanerOwner>);

impl ArtifactCleaner {
    pub(crate) fn new() -> Self {
        Self(Arc::new(CleanerOwner(CleanerShared::new(
            DrainPolicy::default(),
        ))))
    }

    pub(crate) fn queue(&self) -> CleanerQueue {
        CleanerQueue(self.0.0.clone())
    }

    pub(crate) fn shutdown(&self) {
        self.0.0.shutdown();
    }
}

pub struct TempRoot {
    _standalone_owner: Option<ArtifactCleaner>,
    cleaner: CleanerQueue,
    handle: Arc<TempDirHandle>,
    pub(crate) path: PathBuf,
    owner: String,
    ledger: Ledger,
    activity: Activity,
}

impl TempRoot {
    pub fn new(parent: &Path, owner: String, ledger: Ledger) -> Result<Arc<Self>, ErrorCode> {
        let cleaner = ArtifactCleaner::new();
        let queue = cleaner.queue();
        Self::build(parent, owner, ledger, queue, Some(cleaner))
    }

    pub(crate) fn with_cleaner(
        parent: &Path,
        owner: String,
        ledger: Ledger,
        cleaner: CleanerQueue,
    ) -> Result<Arc<Self>, ErrorCode> {
        Self::build(parent, owner, ledger, cleaner, None)
    }

    fn build(
        parent: &Path,
        owner: String,
        ledger: Ledger,
        cleaner: CleanerQueue,
        standalone_owner: Option<ArtifactCleaner>,
    ) -> Result<Arc<Self>, ErrorCode> {
        cleaner.ensure_started()?;
        std::fs::create_dir_all(parent).map_err(|_| ErrorCode::IoError)?;
        let parent_path = canonical_host_dir(parent)?;
        let parent_dir = Dir::open_ambient_dir(&parent_path, cap_std::ambient_authority())
            .map_err(|_| ErrorCode::IoError)?;
        let name = random_identity()?;
        let mut builder = cap_std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use cap_std::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        // Keep a mutable binding on all targets for the platform-specific mode.
        builder.recursive(false);
        parent_dir
            .create_dir_with(&name, &builder)
            .map_err(|_| ErrorCode::IoError)?;
        let dir = parent_dir.open_dir(&name).map_err(|_| ErrorCode::IoError)?;
        let path = parent_path.join(&name);
        let handle = Arc::new(TempDirHandle {
            dir: Some(dir),
            parent: Some(parent_dir),
            name,
            parent_path,
        });
        Ok(Arc::new(Self {
            _standalone_owner: standalone_owner,
            cleaner,
            handle,
            path,
            owner,
            ledger,
            activity: Activity::default(),
        }))
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    fn directory(&self) -> &Dir {
        self.handle.directory()
    }

    pub fn is_quiescent(&self) -> bool {
        self.activity.is_idle()
    }

    pub fn writer(self: &Arc<Self>, format: &str) -> Result<ArtifactWriter, ErrorCode> {
        if !matches!(
            format,
            "wav"
                | "flac"
                | "m4a"
                | "ogg"
                | "mkv"
                | "mp4"
                | "webm"
                | "vtt"
                | "srt"
                | "ass"
                | "txt"
                | "json"
                | "cube"
                | "csv"
                | "edl"
                | "xmp"
        ) {
            return Err(ErrorCode::Unsupported);
        }
        let lease = self.ledger.reserve(
            &self.owner,
            &[(Resource::Handle, 1), (Resource::HostBytes, 4096)],
        )?;
        let name = format!("{}.{}", random_identity()?, format);
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = self
            .directory()
            .open_with(&name, &options)
            .map_err(|_| ErrorCode::IoError)?
            .into_std();
        Ok(ArtifactWriter {
            file: Some(file),
            root: self.clone(),
            name,
            format: format.into(),
            bytes: 0,
            lease: Some(lease),
            activities: vec![self.activity.start()],
        })
    }
}

pub struct ArtifactWriter {
    file: Option<File>,
    root: Arc<TempRoot>,
    pub(super) name: String,
    format: String,
    bytes: u64,
    lease: Option<Reservation>,
    activities: Vec<ActivityLease>,
}

impl ArtifactWriter {
    pub(crate) fn with_activity(mut self, lease: ActivityLease) -> Self {
        self.activities.push(lease);
        self
    }

    pub fn write(&mut self, bytes: &[u8], cancel: &HttpCancel) -> Result<(), ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        self.lease
            .as_mut()
            .ok_or(ErrorCode::BadState)?
            .extend(&[(Resource::TempBytes, bytes.len() as u64)])?;
        self.file
            .as_mut()
            .ok_or(ErrorCode::BadState)?
            .write_all(bytes)
            .map_err(|_| ErrorCode::IoError)?;
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    pub fn finish(mut self, cancel: &HttpCancel) -> Result<Arc<Artifact>, ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let file = self.file.take().ok_or(ErrorCode::BadState)?;
        file.sync_all().map_err(|_| ErrorCode::IoError)?;
        drop(file);
        let file = PinnedFile::open(&self.root.path.join(&self.name))?;
        if file.initial.size != self.bytes {
            return Err(ErrorCode::IoError);
        }
        Ok(Arc::new(Artifact {
            id: iris_api::v2::Id(next_id()?),
            file: Some(file),
            root: self.root.clone(),
            name: self.name.clone(),
            format: self.format.clone(),
            lease: self.lease.take(),
            activities: std::mem::take(&mut self.activities),
        }))
    }
}

impl Drop for ArtifactWriter {
    fn drop(&mut self) {
        drop(self.file.take());
        queue(
            &self.root,
            &self.name,
            self.lease.take(),
            std::mem::take(&mut self.activities),
        );
    }
}

pub struct Artifact {
    pub id: iris_api::v2::Id,
    file: Option<PinnedFile>,
    root: Arc<TempRoot>,
    name: String,
    pub format: String,
    lease: Option<Reservation>,
    activities: Vec<ActivityLease>,
}

impl Artifact {
    pub fn size(&self) -> u64 {
        self.file.as_ref().map_or(0, |f| f.initial.size)
    }

    pub fn owner(&self) -> &str {
        self.root.owner()
    }

    pub fn path(&self) -> PathBuf {
        self.root.path.join(&self.name)
    }

    pub fn read_at(
        &self,
        offset: u64,
        bytes: &mut [u8],
        cancel: &HttpCancel,
    ) -> Result<usize, ErrorCode> {
        self.file
            .as_ref()
            .ok_or(ErrorCode::BadState)?
            .read_at(offset, bytes, cancel)
    }
}

impl Drop for Artifact {
    fn drop(&mut self) {
        drop(self.file.take());
        queue(
            &self.root,
            &self.name,
            self.lease.take(),
            std::mem::take(&mut self.activities),
        );
    }
}

fn queue(
    root: &Arc<TempRoot>,
    name: &str,
    lease: Option<Reservation>,
    activities: Vec<ActivityLease>,
) {
    if let Some(lease) = lease {
        let job = Cleanup {
            dir: root.handle.clone(),
            name: name.into(),
            _leases: vec![lease],
            _activities: activities,
        };
        root.cleaner.enqueue(job);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ArtifactCleaner, CleanerOwner, CleanerShared, CleanerState, DrainPolicy, Phase, RemoveFn,
    };
    use std::sync::{Arc, Condvar, Mutex};

    impl ArtifactCleaner {
        pub(crate) fn with_test_hooks(policy: DrainPolicy, remover: Arc<RemoveFn>) -> Self {
            let shared = Arc::new(CleanerShared {
                state: Mutex::new(CleanerState {
                    phase: Phase::Idle,
                    pending: Vec::new(),
                    worker: None,
                }),
                changed: Condvar::new(),
                done: Condvar::new(),
                policy,
                remover: Some(remover),
            });
            Self(Arc::new(CleanerOwner(shared)))
        }
    }
}
