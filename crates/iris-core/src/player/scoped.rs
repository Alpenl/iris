//! Ready protocol-2 media admission. No plugin arguments become mpv options.
use super::Session;
use crate::{
    media_uri::MediaUri,
    plugin_v2::stream::{manifest::Profile, prepare::Playback, session::StreamSession},
    settings::Settings,
};
use anyhow::{Result, ensure};
use iris_mpv::Mpv;
use std::sync::Arc;

const OPAQUE_FORMATS: &str = "mov,matroska,mpegts,mpeg,mpegvideo,avi,flv,ogg,mp3,aac,ac3,eac3,flac,wav,aiff,webvtt,srt,ass,yuv4mpegpipe,jpeg_pipe,png_pipe,webp_pipe,bmp_pipe,tiff_pipe,gif,ico,avif";
fn options(profile: Option<Profile>) -> Vec<(&'static str, String)> {
    let (format, protocols, formats, references) = match profile {
        Some(Profile::Hls) => (
            "hls",
            "http,tcp,crypto",
            "hls,mpegts,mov,aac,ac3,eac3,mp3,webvtt",
            true,
        ),
        Some(Profile::Dash) => ("dash", "http,tcp", "dash,mov,matroska,webvtt", true),
        None => ("", "http,tcp", OPAQUE_FORMATS, false),
    };
    vec![
        ("demuxer", "lavf".into()),
        ("demuxer-lavf-format", format.into()),
        (
            "access-references",
            if references { "yes" } else { "no" }.into(),
        ),
        (
            "demuxer-lavf-o",
            format!(
                "protocol_whitelist=%{}%{protocols},format_whitelist=%{}%{formats},enable_drefs=0,use_absolute_path=0,reconnect=1,reconnect_delay_max=2,reconnect_on_network_error=1,reconnect_on_http_error=%7%502,503",
                protocols.len(),
                formats.len()
            ),
        ),
        ("autoload-files", "no".into()),
        ("sub-auto", "no".into()),
        ("audio-file-auto", "no".into()),
        (
            "stream-lavf-o",
            "reconnect=1,reconnect_delay_max=2,reconnect_on_network_error=1,reconnect_on_http_error=%7%502,503".into(),
        ),
    ]
}
pub(super) fn restore_attachment_policy(mpv: &Mpv) -> Result<()> {
    for (key, value) in options(None) {
        mpv.set_property_string(&format!("file-local-options/{key}"), &value)?;
    }
    Ok(())
}
impl Session {
    pub fn open_scoped(
        &mut self,
        playback: Arc<Playback>,
        uri: MediaUri,
        settings: &Settings,
        resume: f64,
    ) -> Result<()> {
        ensure!(
            !matches!(uri, MediaUri::Local(_)),
            "plugin media cannot adopt a local identity"
        );
        ensure!(
            !playback.relay.session().cancellation().is_cancelled(),
            "playback session expired"
        );
        if self
            .scoped_playback
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, &playback))
        {
            playback.relay.session().advance_seek()?;
        }
        self.reset_backend_for_load()?;
        self.on_eof.set(settings.on_eof);
        let mut owned = options(playback.profile);
        if settings.resume_playback && !playback.is_live && resume.is_finite() && resume > 0. {
            owned.push(("start", format!("+{resume:.3}")));
        }
        owned.push(("loop-file", self.loop_value(playback.is_live).into()));
        let opts: Vec<_> = owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let old_relay = self.event_pump.set_relay(Default::default());
        let old_scope = self
            .event_pump
            .set_scoped(Arc::downgrade(playback.relay.session()));
        let id = match self
            .mpv
            .loadfile_with_options(&playback.url, "replace", &opts)
        {
            Ok(id) => id,
            Err(error) => {
                self.event_pump.set_relay(old_relay);
                self.event_pump.set_scoped(old_scope);
                return Err(error.into());
            }
        };
        if let Some(old) = &self.scoped_playback
            && !Arc::ptr_eq(old, &playback)
        {
            old.relay.stop();
        }
        self.playback_relay = None;
        self.pending_scoped_tracks = playback.attachments.clone();
        self.reset_for_load(
            None,
            uri.to_uri_string(),
            Some(playback.title.clone()),
            id,
            Vec::new(),
            playback.kind,
            playback.is_live,
            None,
        );
        let profile_label = match playback.profile {
            Some(Profile::Hls) => "hls",
            Some(Profile::Dash) => "dash",
            None => "direct",
        };
        let attachment_count = playback.attachments.len();
        let kind = playback.kind;
        let is_live = playback.is_live;
        self.last_remote = None;
        self.scoped_playback = Some(playback);
        crate::logging::record_info(
            "player",
            "media_open",
            serde_json::json!({
                "source": "plugin_scoped",
                "kind": format!("{kind:?}"),
                "profile": profile_label,
                "is_live": is_live,
                "has_resume": resume > 0.,
                "attachments": attachment_count,
            }),
        );
        Ok(())
    }
    pub fn plugin_session(&self) -> Option<Arc<StreamSession>> {
        self.scoped_playback
            .as_ref()
            .map(|p| p.relay.session().clone())
    }
    pub fn release_scoped(&mut self) {
        self.discard_scoped();
    }
    pub(super) fn discard_scoped(&mut self) {
        self.pending_scoped_tracks.clear();
        if let Some(playback) = self.scoped_playback.take() {
            playback.relay.stop();
        }
        self.event_pump.set_scoped(Default::default());
    }
    pub(super) fn attach_scoped_tracks(&mut self) {
        for track in std::mem::take(&mut self.pending_scoped_tracks) {
            let command = if track.audio { "audio-add" } else { "sub-add" };
            let flags = if track.default { "select" } else { "auto" };
            if self
                .mpv
                .command_async(&[
                    command,
                    &track.url,
                    flags,
                    &track.label,
                    track.lang.as_deref().unwrap_or(""),
                ])
                .is_err()
            {
                log::warn!("could not attach scoped media track");
            }
        }
    }
}

#[cfg(test)]
mod tests;
