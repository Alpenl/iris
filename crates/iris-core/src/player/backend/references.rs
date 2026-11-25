//! Per-load demuxer reference policies.
use crate::player::relay::Service;
use anyhow::Result;
use iris_mpv::Mpv;

pub(super) fn enable_hls(mpv: &Mpv, service: &Service) -> Result<bool> {
    let path = mpv.get_property_string("stream-open-filename")?;
    let Some(playlist) = service.playlist_url(&path)? else {
        return Ok(false);
    };
    const PROTOCOLS: &str = "http,tcp,crypto";
    const FORMATS: &str = "hls,mpegts,mov,aac,ac3,eac3,mp3,webvtt";
    let options = format!(
        "protocol_whitelist=%{}%{PROTOCOLS},format_whitelist=%{}%{FORMATS},enable_drefs=0,use_absolute_path=0,reconnect_on_http_error=503",
        PROTOCOLS.len(),
        FORMATS.len()
    );
    mpv.set_property_string("file-local-options/demuxer", "lavf")?;
    mpv.set_property_string("file-local-options/demuxer-lavf-format", "hls")?;
    mpv.set_property_string("file-local-options/demuxer-lavf-o", &options)?;
    mpv.set_property_string("stream-open-filename", &playlist)?;
    mpv.set_property_string("file-local-options/access-references", "yes")?;
    Ok(true)
}

pub(super) fn disable_hls(mpv: &Mpv) -> Result<()> {
    mpv.set_property_string("file-local-options/access-references", "no")?;
    mpv.set_property_string("file-local-options/demuxer", "")?;
    mpv.set_property_string("file-local-options/demuxer-lavf-format", "")?;
    mpv.set_property_string("file-local-options/demuxer-lavf-o", "")?;
    Ok(())
}
