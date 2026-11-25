//! Media enhancement helpers: CID lookup, danmaku windowing, WBI-signed `/x/player/wbi/v2`,
//! and sequential BCC -> WebVTT subtitle artifact staging.

use super::*;
use iris_plugin_sdk::api::v2::{
    enhance::{EnhancementResource, SubtitleFormat},
    task::ArtifactFormat,
    task_result::{ArtifactResult, ArtifactWriterResult},
};

impl BilibiliPlugin {
    pub(super) fn lookup_playing_cid(&self, uri: &str) -> Option<(String, u64, u64)> {
        if let Some(found) = self.cid_lookup.get(uri) {
            return Some(found.clone());
        }
        let tail = uri.rsplit('/').next()?;
        if let Some(found) = self.cid_lookup.get(tail) {
            return Some(found.clone());
        }
        if let Some(rest) = tail.strip_prefix("ugc:") {
            let (bvid, cid_s) = rest.split_once(':')?;
            let cid = cid_s.parse::<u64>().ok()?;
            return Some((bvid.to_owned(), 0, cid));
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn fetch_enhancements(
        &self,
        state: &mut WindowState,
        media: Id,
        bvid: &str,
        aid: u64,
        cid: u64,
        host: &mut impl Host,
        now_secs: u64,
    ) {
        let _ = self.start_http(
            state,
            "GET",
            format!("https://api.bilibili.com/x/v1/dm/list.so?oid={cid}"),
            true,
            None,
            HttpFlow::DanmakuXml { media, cid },
            host,
        );
        if self.wbi.mixin().is_none() {
            state.needs_wbi_player_v2 = true;
            if !state
                .http_jobs
                .values()
                .any(|j| matches!(j.flow, HttpFlow::Nav { .. }))
            {
                let _ = self.start_http(
                    state,
                    "GET",
                    "https://api.bilibili.com/x/web-interface/nav".into(),
                    true,
                    None,
                    HttpFlow::Nav { open_modal: false },
                    host,
                );
            }
        } else {
            state.needs_wbi_player_v2 = false;
        }
        self.request_player_v2(state, media, bvid, aid, cid, false, host, now_secs);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn request_player_v2(
        &self,
        state: &mut WindowState,
        media: Id,
        bvid: &str,
        aid: u64,
        cid: u64,
        retried_wbi: bool,
        host: &mut impl Host,
        now_secs: u64,
    ) {
        let mut params = vec![("cid", cid.to_string())];
        if !bvid.is_empty() {
            params.push(("bvid", bvid.to_owned()));
        } else if aid > 0 {
            params.push(("aid", aid.to_string()));
        }
        let q = if let Some(mixin) = self.wbi.mixin() {
            wbi::sign_wbi_query(&params, &mixin, now_secs)
        } else {
            wbi::unsigned_query(&params)
        };
        let _ = self.start_http(
            state,
            "GET",
            format!("https://api.bilibili.com/x/player/wbi/v2?{q}"),
            true,
            None,
            HttpFlow::PlayerV2 {
                media,
                cid,
                retried_wbi,
            },
            host,
        );
    }

    pub(super) fn push_danmaku_window(
        &self,
        state: &WindowState,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        if !self.prefs.danmaku_enabled {
            return Vec::new();
        }
        let (Some(media), Some(win)) = (state.active_media, state.overlay_window.as_ref()) else {
            return Vec::new();
        };
        if state.danmaku.is_empty() {
            return Vec::new();
        }
        let cues = danmaku::cues_for_window(&state.danmaku, win, self.prefs.danmaku_opacity);
        let Ok(bytes) = serde_json::to_vec(&cues) else {
            return Vec::new();
        };
        let Ok(blob) = host.blob(&bytes) else {
            return Vec::new();
        };
        vec![HostCommand::SetTimedOverlay {
            media,
            generation: state.overlay_gen,
            track: "danmaku".into(),
            window_revision: win.revision,
            cues: blob,
        }]
    }

    pub(super) fn handle_sub_artifact_completed(
        &self,
        state: &mut WindowState,
        mut sub_job: PendingSubtitleArtifact,
        result: Value,
        host: &mut impl Host,
    ) -> Vec<HostCommand> {
        if sub_job.writer.is_none() {
            let Ok(opened) = completed::<ArtifactWriterResult>(result) else {
                return Vec::new();
            };
            let Ok(blob) = host.blob(&sub_job.vtt_bytes) else {
                host.close(opened.artifact_writer);
                return Vec::new();
            };
            let write_res = host.submit(&Task::ArtifactWrite {
                handle: opened.artifact_writer,
                blob,
                finish: true,
            });
            host.close(blob);
            if let Ok(next_op) = write_res {
                sub_job.op = next_op;
                sub_job.writer = Some(opened.artifact_writer);
                sub_job.vtt_bytes.clear();
                state.sub_jobs.insert(next_op, sub_job);
            } else {
                host.close(opened.artifact_writer);
            }
            return Vec::new();
        }
        let Ok(done) = completed::<ArtifactResult>(result) else {
            return Vec::new();
        };
        if state.active_media == Some(sub_job.media) {
            return vec![HostCommand::AttachSubtitle {
                media: sub_job.media,
                generation: state.generation,
                id: sub_job.sub.id,
                label: format!("B站字幕 · {}", sub_job.sub.label),
                lang: Some(sub_job.sub.lang),
                format: SubtitleFormat::WebVtt,
                resource: EnhancementResource::Artifact {
                    handle: done.artifact,
                },
            }];
        }
        host.close(done.artifact);
        Vec::new()
    }

    pub(super) fn stage_subtitle_bcc(
        &self,
        state: &mut WindowState,
        media: Id,
        sub: SubtitleTrackMeta,
        body: &[u8],
        host: &mut impl Host,
    ) {
        if state.active_media != Some(media) {
            return;
        }
        if let Some(vtt) = danmaku::bcc_json_to_webvtt(body)
            && state.sub_jobs.len() < 4
            && let Ok(op) = host.submit(&Task::ArtifactBegin {
                format: ArtifactFormat::WebVtt,
            })
        {
            state.sub_jobs.insert(
                op,
                PendingSubtitleArtifact {
                    op,
                    writer: None,
                    media,
                    sub,
                    vtt_bytes: vtt.into_bytes(),
                },
            );
        }
        self.pump_next_subtitle(state, host);
    }

    pub(super) fn pump_next_subtitle(&self, state: &mut WindowState, host: &mut impl Host) {
        if state
            .http_jobs
            .values()
            .any(|j| matches!(j.flow, HttpFlow::SubtitleBcc { .. }))
        {
            return;
        }
        while !state.pending_subtitles.is_empty() {
            let (media, sub) = state.pending_subtitles.remove(0);
            if state.active_media != Some(media) {
                continue;
            }
            let url = sub.url.clone();
            if self
                .start_http(
                    state,
                    "GET",
                    url,
                    true,
                    None,
                    HttpFlow::SubtitleBcc { media, sub },
                    host,
                )
                .is_ok()
            {
                break;
            }
        }
    }

    pub(super) fn remember_cid(
        &mut self,
        state: &mut WindowState,
        meta: &VideoMeta,
        requested_entry_id: &str,
    ) {
        let val = (meta.bvid.clone(), meta.aid, meta.cid);
        state.last_resolved_cid = Some(val.clone());
        for key in [
            meta.entry_id.as_str(),
            meta.bvid.as_str(),
            requested_entry_id,
        ] {
            if key.is_empty() {
                continue;
            }
            while self.cid_lookup.len() >= 128 {
                let Some(first) = self.cid_lookup.keys().next().cloned() else {
                    break;
                };
                self.cid_lookup.remove(&first);
            }
            self.cid_lookup.insert(key.to_owned(), val.clone());
            if !meta.source_id.is_empty() {
                let full_uri = format!(
                    "iris-plugin://community.bilibili/{}/{}",
                    meta.source_id,
                    key.trim_start_matches('/')
                );
                self.cid_lookup.insert(full_uri, val.clone());
            }
        }
    }
}
