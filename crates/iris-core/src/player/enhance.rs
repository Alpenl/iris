//! Native acknowledgement owns attachment lifetimes; the guest never sends
//! mpv commands, options, filesystem paths or native track identifiers.
use super::{Chapter, Command, Session};
use crate::plugin_v2::{enhancement::Attachment, instance::next_id};
use anyhow::{Result, ensure};
use iris_mpv::Mpv;
use std::{collections::BTreeMap, sync::Arc};

enum Pending {
    Add {
        track: Arc<Attachment>,
        wanted: bool,
    },
    Remove(Arc<Attachment>),
}

#[derive(Default)]
pub(super) struct TrackState {
    active: BTreeMap<(String, String), Arc<Attachment>>,
    pending: BTreeMap<u64, Pending>,
    retired: Vec<Arc<Attachment>>,
}
fn native_track(mpv: &Mpv, track: &Attachment) -> Option<i64> {
    let raw = mpv.get_property_string("track-list").ok()?;
    if raw.len() > 1 << 20 {
        return None;
    }
    let values: Vec<serde_json::Value> = serde_json::from_str(&raw).ok()?;
    values.iter().take(1024).find(|v| {
        v["external-filename"].as_str() == Some(track.url())
            && v["type"].as_str() == Some(if track.audio { "audio" } else { "sub" })
    })?["id"]
        .as_i64()
}
impl TrackState {
    fn has_sub_track(&self) -> bool {
        self.active.values().any(|t| !t.audio)
            || self
                .pending
                .values()
                .any(|p| matches!(p, Pending::Add { track, wanted: true } if !track.audio))
    }
    fn remove(&mut self, mpv: &Mpv, track: Arc<Attachment>) -> Result<()> {
        if let Some(native) = native_track(mpv, &track) {
            let receipt = next_id().map_err(|e| anyhow::anyhow!("{e:?}"))?;
            mpv.command_with_reply(
                receipt,
                &[
                    if track.audio {
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
                if wanted && track.is_live() && native_track(mpv, &track).is_some() {
                    let key = (track.owner.clone(), track.id.clone());
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
                && track.owner == owner
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
    pub fn attach_plugin_track(&mut self, track: Arc<Attachment>) -> Result<()> {
        ensure!(
            self.state.loaded && track.is_live(),
            "attachment has no live media"
        );
        ensure!(
            self.plugin_tracks.active.len()
                + self.plugin_tracks.pending.len()
                + self.plugin_tracks.retired.len()
                < 32,
            "too many native attachments"
        );
        let key = (track.owner.clone(), track.id.clone());
        for pending in self.plugin_tracks.pending.values_mut() {
            if let Pending::Add {
                track: previous,
                wanted,
            } = pending
                && previous.owner == track.owner
                && previous.id == track.id
            {
                *wanted = false;
            }
        }
        let replacing_selected_sub = !track.audio
            && self
                .plugin_tracks
                .active
                .get(&key)
                .and_then(|old| native_track(&self.mpv, old))
                .is_some_and(|old_id| {
                    self.selected_track(super::TrackKind::Sub)
                        .is_some_and(|sel| sel.id == old_id)
                });
        let select_sub = !track.audio
            && self.state.subs_visible
            && (replacing_selected_sub
                || (self.selected_track(super::TrackKind::Sub).is_none()
                    && !self.plugin_tracks.has_sub_track()));
        let flag = if select_sub { "select" } else { "auto" };
        // Applies also to enhancements of ordinary local media; decoder policy
        // for the already-open primary stream has already been captured by mpv.
        super::scoped::restore_attachment_policy(&self.mpv)?;
        let receipt = next_id().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        self.mpv.command_with_reply(
            receipt,
            &[
                if track.audio { "audio-add" } else { "sub-add" },
                track.url(),
                flag,
                &track.label,
                track.lang.as_deref().unwrap_or(""),
            ],
        )?;
        if let Some(old) = self.plugin_tracks.active.remove(&key) {
            let hold = old.clone();
            if self.plugin_tracks.remove(&self.mpv, old).is_err() {
                self.plugin_tracks.retired.push(hold);
            }
        }
        self.plugin_tracks.pending.insert(
            receipt,
            Pending::Add {
                track,
                wanted: true,
            },
        );
        Ok(())
    }
    pub fn revoke_plugin_tracks(&mut self, owner: &str) {
        self.plugin_tracks.revoke(&self.mpv, owner);
        if self.plugin_timeline.as_deref() == Some(owner) {
            self.plugin_timeline = None;
            self.chapters_dirty = true;
        }
    }
    pub fn set_plugin_timeline(
        &mut self,
        owner: String,
        chapters: &[iris_api::v2::enhance::Chapter],
    ) -> Result<()> {
        ensure!(
            self.state.loaded && chapters.len() <= 1000,
            "invalid timeline"
        );
        ensure!(
            chapters.windows(2).all(|w| w[0].start_ms < w[1].start_ms),
            "unordered timeline"
        );
        self.plugin_timeline = Some(owner);
        self.state.chapters = chapters
            .iter()
            .map(|chapter| Chapter {
                time: chapter.start_ms.0 as f64 / 1000.,
                title: chapter.title.clone(),
            })
            .collect();
        self.update_plugin_chapter();
        Ok(())
    }
    pub(super) fn update_plugin_chapter(&mut self) {
        if self.plugin_timeline.is_some() {
            self.state.current_chapter = self
                .state
                .chapters
                .iter()
                .rposition(|c| c.time <= self.state.time_pos)
                .map_or(-1, |v| v as i64);
        }
    }
    pub(super) fn plugin_chapter_seek(&self, command: &Command) -> Option<f64> {
        self.plugin_timeline.as_ref()?;
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
