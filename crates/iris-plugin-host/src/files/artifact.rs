use super::identity::{PinnedFile, canonical_host_dir};
use crate::{
    instance::next_id,
    ledger::{Activity, ActivityLease, Ledger, Reservation, Resource},
};
use cap_std::fs::{Dir, OpenOptions};
use iris_api::v2::ErrorCode;
use iris_net::http_client::HttpCancel;
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, mpsc},
    time::Duration,
};

struct Cleanup {
    root: Arc<TempRoot>,
    name: String,
    _leases: Vec<Reservation>,
    _activities: Vec<ActivityLease>,
}
fn cleanup_queue() -> Result<&'static mpsc::Sender<Cleanup>, ErrorCode> {
    static QUEUE: OnceLock<Result<mpsc::Sender<Cleanup>, ()>> = OnceLock::new();
    QUEUE
        .get_or_init(|| {
            // Every queued job retains a global Handle reservation; the ledger
            // bounds both this channel and the retry list, without blocking Drop.
            let (tx, rx) = mpsc::channel::<Cleanup>();
            iris_net::concurrency::spawn_thread(
                iris_net::concurrency::ThreadRole::ArtifactCleanup,
                move || {
                    let mut pending = Vec::<Cleanup>::new();
                    loop {
                        match rx.recv_timeout(Duration::from_millis(250)) {
                            Ok(job) => pending.push(job),
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(_) => {}
                        }
                        pending.extend(rx.try_iter());
                        // Failed deletion keeps its handle/disk quota. There can be at
                        // most 1024 jobs including live artifacts; no retry queue grows
                        // independently of the global handle ledger.
                        pending.retain(|job| {
                            job.root
                                .directory()
                                .remove_file(&job.name)
                                .is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound)
                        });
                    }
                },
            )
            .map(|_| tx)
            .map_err(|_| ())
        })
        .as_ref()
        .map_err(|_| ErrorCode::Internal)
}
pub struct TempRoot {
    dir: Option<Dir>,
    pub(crate) path: PathBuf,
    parent_path: PathBuf,
    owner: String,
    ledger: Ledger,
    parent: Option<Dir>,
    name: String,
    activity: Activity,
}
impl TempRoot {
    pub fn new(parent: &Path, owner: String, ledger: Ledger) -> Result<Arc<Self>, ErrorCode> {
        cleanup_queue()?;
        std::fs::create_dir_all(parent).map_err(|_| ErrorCode::IoError)?;
        let parent_path = canonical_host_dir(parent)?;
        let parent = Dir::open_ambient_dir(&parent_path, cap_std::ambient_authority())
            .map_err(|_| ErrorCode::IoError)?;
        let name = crate::installation::random_identity()?;
        let mut builder = cap_std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use cap_std::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        // Keep a mutable binding on all targets for the platform-specific mode.
        builder.recursive(false);
        parent
            .create_dir_with(&name, &builder)
            .map_err(|_| ErrorCode::IoError)?;
        let dir = parent.open_dir(&name).map_err(|_| ErrorCode::IoError)?;
        Ok(Arc::new(Self {
            dir: Some(dir),
            path: parent_path.join(&name),
            parent_path,
            owner,
            ledger,
            parent: Some(parent),
            name,
            activity: Activity::default(),
        }))
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    fn directory(&self) -> &Dir {
        self.dir.as_ref().expect("live temporary directory")
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
        let name = format!("{}.{}", crate::installation::random_identity()?, format);
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
impl Drop for TempRoot {
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
pub struct ArtifactWriter {
    file: Option<File>,
    root: Arc<TempRoot>,
    name: String,
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
            root: root.clone(),
            name: name.into(),
            _leases: vec![lease],
            _activities: activities,
        };
        enqueue(job);
    }
}
fn enqueue(job: Cleanup) {
    match cleanup_queue() {
        Ok(tx) => {
            if let Err(error) = tx.send(job) {
                // A failed cleanup worker must not silently release disk quota.
                // Retaining the job fails closed and leaves shutdown non-quiescent.
                std::mem::forget(error.0);
            }
        }
        Err(_) => std::mem::forget(job),
    }
}
