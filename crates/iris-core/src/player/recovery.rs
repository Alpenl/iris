//! Cross-platform video-track recovery state machine for GPU surface resets.
//!
//! When a GPU render device is lost (e.g. D3D11 device reset on Windows), the
//! render worker queries the active concrete video track (`vid`) associated with
//! the currently playing `playlist_entry_id` before tearing down the lost render
//! context, then reattaches `vid` on the recreated surface only if the same
//! playlist entry is still active.

use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};

use iris_mpv::{Error, Format, Mpv, Value};

const ERR_INVALID_PARAMETER: i32 = -4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveVideoTrack {
    pub(crate) entry: i64,
    pub(crate) id: i64,
}

struct VideoTrackRequest {
    query_id: u64,
    entry: i64,
    tx: Sender<Option<ActiveVideoTrack>>,
    wake: SyncSender<()>,
}

#[derive(Default)]
struct VideoRecovery {
    entry: Option<i64>,
    next_query_id: u64,
    pending: Option<VideoTrackRequest>,
}

impl VideoRecovery {
    fn allocate_query_id(&mut self) -> iris_mpv::Result<u64> {
        let id = self.next_query_id.checked_add(1).ok_or_else(|| Error {
            code: ERR_INVALID_PARAMETER,
            context: "exhausted video recovery query IDs".into(),
        })?;
        self.next_query_id = id;
        Ok(id)
    }

    fn on_start_file(&mut self, entry: i64) {
        self.entry = Some(entry);
    }

    fn on_end_file(&mut self, playlist_entry_id: i64) {
        if self.entry == Some(playlist_entry_id) {
            self.entry = None;
        }
    }

    fn on_overflow(&mut self, active_entry: Option<i64>) {
        self.entry = active_entry;
        self.cancel_pending();
    }

    fn on_shutdown(&mut self) {
        self.entry = None;
        self.cancel_pending();
    }

    fn cancel_pending(&mut self) {
        if let Some(VideoTrackRequest { tx, wake, .. }) = self.pending.take() {
            let _ = tx.send(None);
            let _ = wake.try_send(());
        }
    }

    fn reply(&mut self, query_id: u64, vid: Option<i64>) {
        if self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.query_id != query_id)
        {
            return;
        }
        if let Some(VideoTrackRequest {
            entry, tx, wake, ..
        }) = self.pending.take()
        {
            let track = vid
                .filter(|_| self.entry == Some(entry))
                .map(|id| ActiveVideoTrack { entry, id });
            let _ = tx.send(track);
            let _ = wake.try_send(());
        }
    }
}

/// Session-scoped handle passed to the platform video surface worker so device-reset
/// track recovery stays outside `iris-mpv` while sharing `EventPump`'s entry tracking.
#[derive(Clone)]
pub struct VideoRecoveryHandle {
    mpv: Arc<Mpv>,
    state: Arc<Mutex<VideoRecovery>>,
}

impl VideoRecoveryHandle {
    pub(super) fn new(mpv: Arc<Mpv>) -> Self {
        Self {
            mpv,
            state: Arc::new(Mutex::new(VideoRecovery::default())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VideoRecovery> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(in crate::player) fn on_start_file(&self, entry: i64) {
        self.lock().on_start_file(entry);
    }

    pub(in crate::player) fn on_end_file(&self, entry: i64) {
        self.lock().on_end_file(entry);
    }

    pub(in crate::player) fn on_overflow(&self, active_entry: Option<i64>) {
        self.lock().on_overflow(active_entry);
    }

    pub(in crate::player) fn on_shutdown(&self) {
        self.lock().on_shutdown();
    }

    pub(in crate::player) fn on_property_reply(&self, query_id: u64, error: i32, value: &Value) {
        let vid = if error >= 0
            && let Value::Str(raw) = value
        {
            raw.parse::<i64>().ok().filter(|&id| id > 0)
        } else {
            None
        };
        self.lock().reply(query_id, vid);
    }

    /// Requests the selected concrete video track before freeing a lost device.
    /// Returns `Ok(None)` when no playlist entry is active. Otherwise, `EventPump`
    /// delivers the result and signals `wake`; keep servicing render updates while waiting.
    pub fn request_active_vid(
        &self,
        wake: SyncSender<()>,
    ) -> iris_mpv::Result<Option<Receiver<Option<ActiveVideoTrack>>>> {
        let mut recovery = self.lock();
        let Some(entry) = recovery.entry else {
            return Ok(None);
        };
        if recovery.pending.is_some() {
            return Err(Error {
                code: ERR_INVALID_PARAMETER,
                context: "video track query already pending".into(),
            });
        }
        let query_id = recovery.allocate_query_id()?;
        self.mpv.get_property_async(query_id, "vid", Format::Str)?;
        let (tx, rx) = mpsc::channel();
        recovery.pending = Some(VideoTrackRequest {
            query_id,
            entry,
            tx,
            wake,
        });
        Ok(Some(rx))
    }

    /// Queues video reattachment after recreating the render context, provided
    /// the corresponding file is still active in the client event stream.
    pub fn restore_vid(&self, track: ActiveVideoTrack) -> iris_mpv::Result<bool> {
        let recovery = self.lock();
        if recovery.entry == Some(track.entry) {
            self.mpv
                .set_property_string_async("vid", &track.id.to_string())?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};

    use iris_mpv::{Mpv, Value};

    use super::{ActiveVideoTrack, VideoRecovery, VideoRecoveryHandle, VideoTrackRequest};

    #[test]
    fn recovery_discards_track_from_replaced_file_and_stale_query_ids() {
        let (wake, woke) = mpsc::sync_channel(1);
        let (tx, rx) = mpsc::channel();
        let mut recovery = VideoRecovery {
            entry: Some(7),
            next_query_id: 1,
            pending: Some(VideoTrackRequest {
                query_id: 1,
                entry: 7,
                tx,
                wake,
            }),
        };
        recovery.on_start_file(8);
        recovery.reply(1, Some(3));
        woke.try_recv().expect("video worker notified");
        assert!(rx.try_recv().expect("query completed").is_none());

        let (wake2, woke2) = mpsc::sync_channel(1);
        let (tx2, rx2) = mpsc::channel();
        let mut overflow = VideoRecovery {
            entry: Some(8),
            next_query_id: 1,
            pending: Some(VideoTrackRequest {
                query_id: 1,
                entry: 8,
                tx: tx2,
                wake: wake2,
            }),
        };
        overflow.on_overflow(Some(9));
        woke2.try_recv().expect("overflow unblocks video worker");
        assert!(rx2.try_recv().expect("overflow completes query").is_none());

        let q2 = overflow.allocate_query_id().unwrap();
        assert_eq!(q2, 2);
        let (wake3, woke3) = mpsc::sync_channel(1);
        let (tx3, rx3) = mpsc::channel();
        overflow.pending = Some(VideoTrackRequest {
            query_id: q2,
            entry: 9,
            tx: tx3,
            wake: wake3,
        });
        overflow.reply(1, Some(4));
        assert!(woke3.try_recv().is_err());
        assert!(rx3.try_recv().is_err());

        overflow.reply(2, Some(5));
        woke3.try_recv().expect("matching query wakes worker");
        let resolved = rx3
            .try_recv()
            .expect("matching query completes")
            .expect("active entry matches");
        assert_eq!((resolved.entry, resolved.id), (9, 5));
    }

    #[test]
    fn request_active_vid_and_restore_vid_report_active_entry_match() {
        let mut mpv = Mpv::create().unwrap();
        mpv.set_option("vo", "null").unwrap();
        mpv.set_option("ao", "null").unwrap();
        mpv.set_option("idle", "yes").unwrap();
        mpv.set_option("config", "no").unwrap();
        mpv.initialize().unwrap();
        let handle = VideoRecoveryHandle::new(Arc::new(mpv));
        let (wake, woke) = mpsc::sync_channel(1);
        assert!(handle.request_active_vid(wake).unwrap().is_none());
        assert!(woke.try_recv().is_err());

        assert!(
            !handle
                .restore_vid(ActiveVideoTrack { entry: 7, id: 1 })
                .unwrap()
        );
        handle.on_start_file(7);
        assert!(
            !handle
                .restore_vid(ActiveVideoTrack { entry: 8, id: 1 })
                .unwrap()
        );
        assert!(
            handle
                .restore_vid(ActiveVideoTrack { entry: 7, id: 2 })
                .unwrap()
        );

        let (wake2, woke2) = mpsc::sync_channel(1);
        let rx = handle
            .request_active_vid(wake2)
            .unwrap()
            .expect("active entry queries vid");
        handle.on_property_reply(1, 0, &Value::Str("2".into()));
        woke2.try_recv().expect("property reply wakes worker");
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(ActiveVideoTrack { entry: 7, id: 2 })
        );

        handle.on_end_file(7);
        assert!(
            !handle
                .restore_vid(ActiveVideoTrack { entry: 7, id: 2 })
                .unwrap()
        );
    }
}
