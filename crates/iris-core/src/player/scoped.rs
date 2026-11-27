//! Ready externally-scoped media admission. No plugin arguments become mpv options.

use std::sync::Arc;

use anyhow::{Result, ensure};
use iris_mpv::Mpv;

use super::Session;
use super::machine::{MediaIdentity, OpeningTransport};
use crate::library::Kind;
use crate::media_uri::MediaUri;
use crate::settings::Settings;

/// Demuxer profile admitted for an externally-scoped media stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamProfile {
    Hls,
}

/// Sealed loopback attachment prepared alongside a [`ScopedPlayback`].
#[derive(Clone, Debug)]
pub struct ScopedAttachment {
    pub id: String,
    pub url: String,
    pub label: String,
    pub lang: Option<String>,
    pub default: bool,
    pub audio: bool,
}

/// Host-side handle to an externally-scoped media relay (e.g. protocol-2 plugin stream).
pub trait ScopedStream: Send + Sync {
    fn root_url(&self) -> Result<String>;
    fn is_cancelled(&self) -> bool;
    fn advance_seek(&self) -> Result<()>;
    fn stop(&self);
}

struct EmptyScoped;
impl ScopedStream for EmptyScoped {
    fn root_url(&self) -> Result<String> {
        anyhow::bail!("no scoped stream")
    }
    fn is_cancelled(&self) -> bool {
        true
    }
    fn advance_seek(&self) -> Result<()> {
        Ok(())
    }
    fn stop(&self) {}
}

pub(super) fn empty_scoped() -> std::sync::Weak<dyn ScopedStream> {
    std::sync::Weak::<EmptyScoped>::new()
}

/// Ready-to-open scoped media playback descriptor admitted by [`Session::open_scoped`].
pub struct ScopedPlayback {
    pub url: String,
    pub profile: Option<StreamProfile>,
    pub kind: Kind,
    pub title: String,
    pub is_live: bool,
    pub attachments: Vec<ScopedAttachment>,
    pub stream: Arc<dyn ScopedStream>,
}

const OPAQUE_FORMATS: &str = "mov,matroska,mpegts,mpeg,mpegvideo,avi,flv,ogg,mp3,aac,ac3,eac3,flac,wav,aiff,webvtt,srt,ass,yuv4mpegpipe,jpeg_pipe,png_pipe,webp_pipe,bmp_pipe,tiff_pipe,gif,ico,avif";

fn options(profile: Option<StreamProfile>) -> Vec<(&'static str, String)> {
    let (format, protocols, formats, references) = match profile {
        Some(StreamProfile::Hls) => (
            "hls",
            "http,tcp,crypto",
            "hls,mpegts,mov,aac,ac3,eac3,mp3,webvtt",
            true,
        ),
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
        playback: Arc<ScopedPlayback>,
        uri: MediaUri,
        settings: &Settings,
        resume: f64,
    ) -> Result<()> {
        self.open_scoped_inner(playback, uri, settings, resume, true)
    }

    pub(super) fn reopen_scoped_after_gpu_reset(
        &mut self,
        playback: Arc<ScopedPlayback>,
        uri: MediaUri,
        settings: &Settings,
        resume: f64,
    ) -> Result<()> {
        self.open_scoped_inner(playback, uri, settings, resume, false)
    }

    fn open_scoped_inner(
        &mut self,
        playback: Arc<ScopedPlayback>,
        uri: MediaUri,
        settings: &Settings,
        resume: f64,
        user_open: bool,
    ) -> Result<()> {
        ensure!(
            !matches!(uri, MediaUri::Local(_)),
            "plugin media cannot adopt a local identity"
        );
        ensure!(!playback.stream.is_cancelled(), "playback session expired");
        let same_stream = self
            .machine
            .source
            .scoped_playback()
            .is_some_and(|old| Arc::ptr_eq(old, &playback));
        let unpause_after_eof = if user_open {
            if same_stream {
                playback.stream.advance_seek()?;
            }
            self.should_unpause_after_eof()
        } else {
            false
        };
        self.reset_backend_for_load()?;
        self.machine.on_eof = settings.on_eof;
        let mut owned = options(playback.profile);
        let apply_resume = (!user_open || settings.resume_playback)
            && !playback.is_live
            && resume.is_finite()
            && resume > 0.;
        if apply_resume {
            owned.push(("start", format!("+{resume:.3}")));
        }
        owned.push((
            "loop-file",
            self.machine.loop_value(playback.is_live).into(),
        ));
        let opts: Vec<_> = owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let old_relay = self.event_pump.set_relay(Default::default());
        let old_scope = self.event_pump.set_scoped(Arc::downgrade(&playback.stream));
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
        let pending_tracks = playback.attachments.clone();
        let profile_label = match playback.profile {
            Some(StreamProfile::Hls) => "hls",
            None => "direct",
        };
        let attachment_count = playback.attachments.len();
        let kind = playback.kind;
        let is_live = playback.is_live;
        let title = playback.title.clone();
        self.prepare_enhancements_for_load();
        let ops = self.machine.begin_load(
            id,
            MediaIdentity {
                local_path: None,
                uri: uri.to_uri_string(),
                title: Some(title),
                private_identity: false,
                kind,
                photo: None,
            },
            is_live,
            OpeningTransport::PluginScoped {
                playback,
                pending_tracks,
            },
            unpause_after_eof,
        );
        let _ = self.run_ops(ops);
        crate::logging::event(
            crate::logging::EventLevel::Info,
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

    pub fn scoped_stream(&self) -> Option<Arc<dyn ScopedStream>> {
        self.machine.source.scoped_stream()
    }

    pub fn release_scoped(&mut self) {
        self.discard_scoped();
    }

    pub(super) fn discard_scoped(&mut self) {
        if self.machine.source.is_plugin_scoped() {
            let eof_paused = self.machine.source.eof_paused();
            self.machine.source.stop_active_transports();
            self.machine.source = super::MediaSourceState::Idle { eof_paused };
        }
        self.event_pump.set_scoped(empty_scoped());
    }
}

#[cfg(test)]
#[path = "scoped/tests/lifecycle.rs"]
mod tests;
