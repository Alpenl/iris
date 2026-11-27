//! Native acknowledgement owns attachment lifetimes; the guest never sends
//! mpv commands, options, filesystem paths or native track identifiers.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, ensure};
use iris_mpv::Mpv;

use super::{Chapter, Command, CommandReceipts, Session};

/// Externally managed subtitle/audio attachment bound to a host playback session.
pub trait ExternalTrack: Send + Sync {
    fn owner(&self) -> &str;
    fn id(&self) -> &str;
    fn label(&self) -> &str;
    fn lang(&self) -> Option<&str>;
    fn is_audio(&self) -> bool;
    fn url(&self) -> &str;
    fn is_live(&self) -> bool;
}

enum Pending {
    Add {
        track: Arc<dyn ExternalTrack>,
        entry_id: Option<i64>,
        wanted: bool,
        stopping: bool,
    },
    Remove {
        track: Arc<dyn ExternalTrack>,
        stopping: bool,
    },
    Stop,
}

pub(super) struct TrackState {
    receipts: CommandReceipts,
    active: BTreeMap<(String, String), Arc<dyn ExternalTrack>>,
    pending: BTreeMap<u64, Pending>,
    retired: Vec<Arc<dyn ExternalTrack>>,
}

impl TrackState {
    pub(super) fn new(receipts: CommandReceipts) -> Self {
        Self {
            receipts,
            active: BTreeMap::new(),
            pending: BTreeMap::new(),
            retired: Vec::new(),
        }
    }
}

fn native_track(mpv: &Mpv, track: &dyn ExternalTrack) -> Option<i64> {
    let raw = mpv.get_property_string("track-list").ok()?;
    if raw.len() > 1 << 20 {
        return None;
    }
    let values: Vec<serde_json::Value> = serde_json::from_str(&raw).ok()?;
    values.iter().take(1024).find(|v| {
        v["external-filename"].as_str() == Some(track.url())
            && v["type"].as_str() == Some(if track.is_audio() { "audio" } else { "sub" })
    })?["id"]
        .as_i64()
}

impl TrackState {
    fn has_sub_track(&self) -> bool {
        self.active.values().any(|t| !t.is_audio())
            || self.pending.values().any(|p| {
                matches!(
                    p,
                    Pending::Add {
                        track,
                        wanted: true,
                        ..
                    } if !track.is_audio()
                )
            })
    }
    fn has_pending_stop(&self) -> bool {
        self.pending.values().any(|p| matches!(p, Pending::Stop))
    }
    pub fn has_pending_adds(&self) -> bool {
        self.pending
            .values()
            .any(|p| matches!(p, Pending::Add { .. }))
    }
    pub fn has_any_tracks(&self) -> bool {
        !self.active.is_empty() || !self.pending.is_empty() || !self.retired.is_empty()
    }
    pub fn has_retired_or_pending(&self) -> bool {
        !self.retired.is_empty() || !self.pending.is_empty()
    }
    pub fn snapshot_wanted_live(&self) -> Vec<Arc<dyn ExternalTrack>> {
        let mut out: Vec<Arc<dyn ExternalTrack>> = self
            .active
            .values()
            .filter(|t| t.is_live())
            .cloned()
            .collect();
        for pending in self.pending.values() {
            if let Pending::Add {
                track,
                wanted: true,
                ..
            } = pending
                && track.is_live()
            {
                if let Some(pos) = out
                    .iter()
                    .position(|t| t.owner() == track.owner() && t.id() == track.id())
                {
                    out[pos] = Arc::clone(track);
                } else {
                    out.push(Arc::clone(track));
                }
            }
        }
        out
    }
    pub fn snapshot_wanted_weak(&self) -> Vec<std::sync::Weak<dyn ExternalTrack>> {
        self.snapshot_wanted_live()
            .iter()
            .map(Arc::downgrade)
            .collect()
    }
    fn remove(&mut self, mpv: &Mpv, track: Arc<dyn ExternalTrack>) -> Result<()> {
        if let Some(native) = native_track(mpv, track.as_ref()) {
            let receipt = self.receipts.next()?;
            mpv.command_with_reply(
                receipt,
                &[
                    if track.is_audio() {
                        "audio-remove"
                    } else {
                        "sub-remove"
                    },
                    &native.to_string(),
                ],
            )?;
            self.pending.insert(
                receipt,
                Pending::Remove {
                    track,
                    stopping: false,
                },
            );
        } else {
            self.retired.push(track);
        }
        Ok(())
    }
    pub fn reply(
        &mut self,
        mpv: &Mpv,
        current_entry: Option<i64>,
        current_loaded: bool,
        id: u64,
        error: i32,
    ) -> bool {
        let Some(pending) = self.pending.remove(&id) else {
            return false;
        };
        match pending {
            Pending::Add {
                track,
                entry_id,
                wanted,
                stopping,
            } => {
                let same_entry = entry_id.is_some() && entry_id == current_entry;
                if error >= 0
                    && wanted
                    && same_entry
                    && track.is_live()
                    && native_track(mpv, track.as_ref()).is_some()
                {
                    let key = (track.owner().to_string(), track.id().to_string());
                    if let Some(old) = self.active.insert(key, Arc::clone(&track)) {
                        if old.url() == track.url() {
                            self.retired.push(old);
                        } else {
                            let hold = old.clone();
                            if let Err(error) = self.remove(mpv, old) {
                                self.retired.push(hold);
                                log::warn!("removing replaced track failed: {error}");
                            }
                        }
                    }
                } else if stopping {
                    if self.has_pending_stop() {
                        self.retired.push(track);
                    } else if error >= 0
                        && same_entry
                        && native_track(mpv, track.as_ref()).is_some()
                    {
                        let hold = track.clone();
                        if let Err(error) = self.remove(mpv, track) {
                            self.retired.push(hold);
                            log::warn!("removing stopped track failed: {error}");
                        }
                    }
                } else if !same_entry {
                    if current_entry.is_some() && !current_loaded {
                        self.retired.push(track);
                    }
                } else if error >= 0 {
                    let url_in_use = self.active.values().any(|t| t.url() == track.url())
                        || self.pending.values().any(|p| {
                            matches!(
                                p,
                                Pending::Add {
                                    track: other,
                                    entry_id: e,
                                    wanted: true,
                                    ..
                                } if *e == current_entry && other.url() == track.url()
                            )
                        });
                    if url_in_use {
                        self.retired.push(track);
                    } else {
                        let hold = track.clone();
                        if let Err(error) = self.remove(mpv, track) {
                            self.retired.push(hold);
                            log::warn!("removing revoked track failed: {error}");
                        }
                    }
                }
            }
            Pending::Remove { track, stopping } if error < 0 => {
                // A native failure does not prove release unless a subsequent
                // `stop` barrier has already completed.
                if !stopping || self.has_pending_stop() {
                    self.retired.push(track);
                }
            }
            Pending::Stop => {
                if !self.has_pending_stop() {
                    self.retired.clear();
                }
                return false;
            }
            _ => {}
        }
        true
    }
    pub fn revoke(&mut self, mpv: &Mpv, owner: &str) {
        for pending in self.pending.values_mut() {
            if let Pending::Add { track, wanted, .. } = pending
                && track.owner() == owner
            {
                *wanted = false;
            }
        }
        let keys: Vec<_> = self
            .active
            .keys()
            .filter(|(p, _)| p == owner)
            .cloned()
            .collect();
        for key in keys {
            if let Some(track) = self.active.remove(&key) {
                let hold = track.clone();
                if self.remove(mpv, track).is_err() {
                    self.retired.push(hold);
                }
            }
        }
    }
    pub fn loading(&mut self) {
        self.retired
            .extend(std::mem::take(&mut self.active).into_values());
        for pending in self.pending.values_mut() {
            if let Pending::Add { wanted, .. } = pending {
                *wanted = false;
            }
        }
    }
    pub fn stopping(&mut self) {
        self.retired
            .extend(std::mem::take(&mut self.active).into_values());
        for pending in self.pending.values_mut() {
            match pending {
                Pending::Add {
                    wanted, stopping, ..
                } => {
                    *wanted = false;
                    *stopping = true;
                }
                Pending::Remove { stopping, .. } => {
                    *stopping = true;
                }
                Pending::Stop => {}
            }
        }
    }
    pub fn track_stop(&mut self, receipt: u64) {
        self.pending.insert(receipt, Pending::Stop);
    }
    pub fn on_end_file(&mut self) {
        self.active.clear();
        self.retired.clear();
        for pending in self.pending.values_mut() {
            if let Pending::Add { wanted, .. } = pending {
                *wanted = false;
            }
        }
    }
    pub fn loaded(&mut self) {
        self.retired.clear();
    }
    pub fn clear(&mut self) {
        self.active.clear();
        self.pending.clear();
        self.retired.clear();
    }
}

impl Session {
    pub fn attach_plugin_track(&mut self, track: Arc<dyn ExternalTrack>) -> Result<()> {
        ensure!(
            self.machine.state.loaded && track.is_live(),
            "attachment has no live media"
        );
        ensure!(
            self.enhancements.tracks.active.len()
                + self.enhancements.tracks.pending.len()
                + self.enhancements.tracks.retired.len()
                < 32,
            "too many native attachments"
        );
        let key = (track.owner().to_string(), track.id().to_string());
        for pending in self.enhancements.tracks.pending.values_mut() {
            if let Pending::Add {
                track: previous,
                wanted,
                ..
            } = pending
                && previous.owner() == track.owner()
                && previous.id() == track.id()
            {
                *wanted = false;
            }
        }
        let replacing_selected_sub = !track.is_audio()
            && self
                .enhancements
                .tracks
                .active
                .get(&key)
                .and_then(|old| native_track(&self.mpv, old.as_ref()))
                .is_some_and(|old_id| {
                    self.selected_track(super::TrackKind::Sub)
                        .is_some_and(|sel| sel.id == old_id)
                });
        let select_sub = !track.is_audio()
            && self.machine.state.subs_visible
            && (replacing_selected_sub
                || (self.selected_track(super::TrackKind::Sub).is_none()
                    && !self.enhancements.tracks.has_sub_track()
                    && !self.machine.source.has_deferred_local_subs()));
        let flag = if select_sub { "select" } else { "auto" };
        // Applies also to enhancements of ordinary local media; decoder policy
        // for the already-open primary stream has already been captured by mpv.
        super::scoped::restore_attachment_policy(&self.mpv)?;
        let receipt = self.receipts.next()?;
        self.mpv.command_with_reply(
            receipt,
            &[
                if track.is_audio() {
                    "audio-add"
                } else {
                    "sub-add"
                },
                track.url(),
                flag,
                track.label(),
                track.lang().unwrap_or(""),
            ],
        )?;
        if let Some(old) = self.enhancements.tracks.active.remove(&key) {
            let hold = old.clone();
            if self.enhancements.tracks.remove(&self.mpv, old).is_err() {
                self.enhancements.tracks.retired.push(hold);
            }
        }
        self.enhancements.tracks.pending.insert(
            receipt,
            Pending::Add {
                track,
                entry_id: self.machine.source.entry_id(),
                wanted: true,
                stopping: false,
            },
        );
        Ok(())
    }

    pub fn revoke_plugin_tracks(&mut self, owner: &str) {
        self.enhancements.tracks.revoke(&self.mpv, owner);
        match &mut self.machine.source {
            super::MediaSourceState::Opening {
                reattach_on_load, ..
            } => reattach_on_load.retain(|t| t.owner() != owner),
            super::MediaSourceState::Failed { enhancements, .. } => {
                enhancements
                    .tracks
                    .retain(|w| w.upgrade().is_some_and(|t| t.owner() != owner));
                if enhancements
                    .timeline
                    .as_ref()
                    .is_some_and(|(o, _)| o == owner)
                {
                    enhancements.timeline = None;
                }
            }
            _ => {}
        }
        if self.enhancements.timeline_owner.as_deref() == Some(owner) {
            self.enhancements.timeline_owner = None;
            self.machine.source.mark_chapters_dirty();
        }
    }

    pub fn set_plugin_timeline(&mut self, owner: String, chapters: &[Chapter]) -> Result<()> {
        ensure!(
            self.machine.state.loaded && chapters.len() <= 1000,
            "invalid timeline"
        );
        ensure!(
            chapters.iter().all(|c| c.time.is_finite() && c.time >= 0.)
                && chapters.windows(2).all(|w| w[0].time < w[1].time),
            "unordered timeline"
        );
        self.enhancements.timeline_owner = Some(owner);
        self.machine.state.chapters = chapters.to_vec();
        self.update_plugin_chapter();
        Ok(())
    }

    pub(super) fn update_plugin_chapter(&mut self) {
        if self.enhancements.timeline_owner.is_some() {
            self.machine.state.current_chapter = self
                .machine
                .state
                .chapters
                .iter()
                .rposition(|c| c.time <= self.machine.state.time_pos)
                .map_or(-1, |v| v as i64);
        }
    }

    pub(super) fn plugin_chapter_seek(&self, command: &Command) -> Option<f64> {
        self.enhancements.timeline_owner.as_ref()?;
        let index = match command {
            Command::SelectChapter(index) => *index,
            Command::NextChapter => (self.machine.state.current_chapter + 1).max(0) as usize,
            Command::PrevChapter => (self.machine.state.current_chapter - 1).max(0) as usize,
            _ => return None,
        };
        Some(
            self.machine
                .state
                .chapters
                .get(index)
                .map_or(self.machine.state.time_pos, |c| c.time),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::library::Kind;
    use crate::player::machine::{MediaIdentity, OpeningTransport};
    use crate::player::{MonitorLutFilter, Session, SessionEvent};

    struct StubLut;

    impl MonitorLutFilter for StubLut {
        fn filter_spec(&self) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    #[test]
    fn lut_reply_does_not_mark_tracks_dirty() {
        let mut session = Session::new().unwrap();
        session.machine.begin_load(
            1,
            MediaIdentity {
                local_path: None,
                uri: "file:///video.mp4".into(),
                title: None,
                private_identity: false,
                kind: Kind::Video,
                photo: None,
            },
            false,
            OpeningTransport::Local,
            false,
        );
        let _ = session.machine.promote_opening_to_loaded();
        let _ = session.machine.take_deferred_refresh_ops();
        session
            .enhancements
            .retired_luts
            .push((77, Arc::new(StubLut)));
        session
            .enhancements
            .retired_luts
            .push((78, Arc::new(StubLut)));

        session.apply(&SessionEvent::CommandReply { id: 77, error: 0 });
        assert_eq!(session.enhancements.retired_luts.len(), 1);
        assert_eq!(session.enhancements.retired_luts[0].0, 78);
        assert!(
            session.machine.take_deferred_refresh_ops().is_empty(),
            "LUT receipt must not trigger a redundant track-list IPC query"
        );

        session.apply(&SessionEvent::CommandReply { id: 78, error: 0 });
        assert!(session.enhancements.retired_luts.is_empty());

        session.apply(&SessionEvent::CommandReply { id: 999, error: 0 });
        assert!(session.machine.take_deferred_refresh_ops().is_empty());
        session.shutdown();
    }

    #[test]
    fn error_event_retires_active_lut_until_vf_clear_reply() {
        let mut session = Session::new_headless(true).unwrap();
        let lut: Arc<dyn MonitorLutFilter> = Arc::new(StubLut);
        session.machine.begin_load(
            1,
            MediaIdentity {
                local_path: None,
                uri: "file:///video.mp4".into(),
                title: None,
                private_identity: false,
                kind: Kind::Video,
                photo: None,
            },
            false,
            OpeningTransport::Local,
            false,
        );
        session.machine.state.loaded = true;
        session.enhancements.monitor_lut = Some(Arc::clone(&lut));
        assert_eq!(Arc::strong_count(&lut), 2);

        session.apply(&SessionEvent::Error(-13));
        assert!(!session.state().loaded);
        assert!(session.enhancements.monitor_lut.is_none());
        assert_eq!(session.enhancements.retired_luts.len(), 1);
        assert_eq!(Arc::strong_count(&lut), 2);

        let receipt = session.enhancements.retired_luts[0].0;
        session.apply(&SessionEvent::CommandReply {
            id: receipt,
            error: 0,
        });
        assert!(session.enhancements.retired_luts.is_empty());
        assert_eq!(Arc::strong_count(&lut), 1);
        session.shutdown();
    }
}
