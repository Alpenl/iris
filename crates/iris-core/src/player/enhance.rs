//! Native acknowledgement owns attachment lifetimes; the guest never sends
//! mpv commands, options, filesystem paths or native track identifiers.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, ensure};
use iris_mpv::Mpv;

use super::{Chapter, Command, Session, next_command_receipt};

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
        wanted: bool,
    },
    Remove(Arc<dyn ExternalTrack>),
}

#[derive(Default)]
pub(super) struct TrackState {
    active: BTreeMap<(String, String), Arc<dyn ExternalTrack>>,
    pending: BTreeMap<u64, Pending>,
    retired: Vec<Arc<dyn ExternalTrack>>,
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
            || self
                .pending
                .values()
                .any(|p| matches!(p, Pending::Add { track, wanted: true } if !track.is_audio()))
    }
    fn remove(&mut self, mpv: &Mpv, track: Arc<dyn ExternalTrack>) -> Result<()> {
        if let Some(native) = native_track(mpv, track.as_ref()) {
            let receipt = next_command_receipt()?;
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
            self.pending.insert(receipt, Pending::Remove(track));
        } else {
            self.retired.push(track);
        }
        Ok(())
    }
    pub fn reply(&mut self, mpv: &Mpv, id: u64, error: i32) {
        match self.pending.remove(&id) {
            Some(Pending::Add { track, wanted }) if error >= 0 => {
                if wanted && track.is_live() && native_track(mpv, track.as_ref()).is_some() {
                    let key = (track.owner().to_string(), track.id().to_string());
                    if let Some(old) = self.active.insert(key, track) {
                        let hold = old.clone();
                        if let Err(error) = self.remove(mpv, old) {
                            self.retired.push(hold);
                            log::warn!("removing replaced track failed: {error}");
                        }
                    }
                } else {
                    let hold = track.clone();
                    if let Err(error) = self.remove(mpv, track) {
                        self.retired.push(hold);
                        log::warn!("removing revoked track failed: {error}");
                    }
                }
            }
            Some(Pending::Remove(track)) if error < 0 => {
                // A native failure does not prove release. Hold data through the
                // next load acknowledgement or the player's shutdown barrier.
                self.retired.push(track);
            }
            _ => {}
        }
    }
    pub fn revoke(&mut self, mpv: &Mpv, owner: &str) {
        for pending in self.pending.values_mut() {
            if let Pending::Add { track, wanted } = pending
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
            self.state.loaded && track.is_live(),
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
            && self.state.subs_visible
            && (replacing_selected_sub
                || (self.selected_track(super::TrackKind::Sub).is_none()
                    && !self.enhancements.tracks.has_sub_track()));
        let flag = if select_sub { "select" } else { "auto" };
        // Applies also to enhancements of ordinary local media; decoder policy
        // for the already-open primary stream has already been captured by mpv.
        super::scoped::restore_attachment_policy(&self.mpv)?;
        let receipt = next_command_receipt()?;
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
                wanted: true,
            },
        );
        Ok(())
    }

    pub fn revoke_plugin_tracks(&mut self, owner: &str) {
        self.enhancements.tracks.revoke(&self.mpv, owner);
        if self.enhancements.timeline_owner.as_deref() == Some(owner) {
            self.enhancements.timeline_owner = None;
            self.chapters_dirty = true;
        }
    }

    pub fn set_plugin_timeline(&mut self, owner: String, chapters: &[Chapter]) -> Result<()> {
        ensure!(
            self.state.loaded && chapters.len() <= 1000,
            "invalid timeline"
        );
        ensure!(
            chapters.iter().all(|c| c.time.is_finite() && c.time >= 0.)
                && chapters.windows(2).all(|w| w[0].time < w[1].time),
            "unordered timeline"
        );
        self.enhancements.timeline_owner = Some(owner);
        self.state.chapters = chapters.to_vec();
        self.update_plugin_chapter();
        Ok(())
    }

    pub(super) fn update_plugin_chapter(&mut self) {
        if self.enhancements.timeline_owner.is_some() {
            self.state.current_chapter = self
                .state
                .chapters
                .iter()
                .rposition(|c| c.time <= self.state.time_pos)
                .map_or(-1, |v| v as i64);
        }
    }

    pub(super) fn plugin_chapter_seek(&self, command: &Command) -> Option<f64> {
        self.enhancements.timeline_owner.as_ref()?;
        let index = match command {
            Command::SelectChapter(index) => *index,
            Command::NextChapter => (self.state.current_chapter + 1).max(0) as usize,
            Command::PrevChapter => (self.state.current_chapter - 1).max(0) as usize,
            _ => return None,
        };
        Some(
            self.state
                .chapters
                .get(index)
                .map_or(self.state.time_pos, |c| c.time),
        )
    }
}

#[cfg(test)]
mod tests;
