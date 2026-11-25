use iris_api::v2::{
    ErrorCode,
    task::{MediaContainer, SampleRate},
};
use std::ffi::OsString;
use std::path::Path;
const INPUT_FORMATS: &str =
    "mov,matroska,mpegts,mpeg,mpegvideo,avi,flv,ogg,mp3,aac,ac3,eac3,flac,wav,aiff";
fn strings(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}
fn input(url: &str) -> Vec<OsString> {
    // In the pinned FFmpeg release MOV external drefs/absolute paths default to
    // disabled. Passing those demuxer-private options to WAV/Matroska is an
    // error, so they are not global CLI flags. Playlists/concat are excluded.
    let mut args = strings(&[
        "-hide_banner",
        "-nostdin",
        "-nostats",
        "-loglevel",
        "error",
        "-threads",
        "2",
        "-protocol_whitelist",
        "http,tcp",
        "-format_whitelist",
        INPUT_FORMATS,
    ]);
    args.extend(strings(&["-i", url]));
    args
}
pub fn audio(url: &str, track: u32, rate: SampleRate) -> Result<Vec<OsString>, ErrorCode> {
    if track > 63 {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut args = input(url);
    args.extend(strings(&[
        "-map",
        &format!("0:a:{track}"),
        "-vn",
        "-sn",
        "-dn",
        "-map_metadata",
        "-1",
        "-ac",
        "1",
        "-ar",
        match rate {
            SampleRate::Hz16000 => "16000",
            SampleRate::Hz48000 => "48000",
        },
        "-c:a",
        "pcm_s16le",
        "-protocol_whitelist",
        "pipe",
        "-f",
        "s16le",
        "pipe:1",
    ]));
    Ok(args)
}
pub fn slice(
    url: &str,
    start: u64,
    end: u64,
    container: MediaContainer,
) -> Result<Vec<OsString>, ErrorCode> {
    if start >= end || end > 7 * 86400 * 1000 {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut args = input(url);
    // Input seeking preserves original packet timestamps with copyts. Packet
    // extents are inspected afterwards; requested points are never reported as
    // measured keyframes without checking the result.
    let position = args
        .iter()
        .position(|a| a == "-i")
        .ok_or(ErrorCode::Internal)?;
    args.splice(position..position, strings(&["-ss", &seconds(start)]));
    args.extend(strings(&[
        "-copyts",
        "-to",
        &seconds(end),
        "-map",
        "0:v?",
        "-map",
        "0:a?",
        "-sn",
        "-dn",
        "-map_metadata",
        "-1",
        "-map_chapters",
        "-1",
        "-c",
        "copy",
        "-avoid_negative_ts",
        "disabled",
        "-protocol_whitelist",
        "pipe",
    ]));
    if matches!(container, MediaContainer::Mp4) {
        args.extend(strings(&[
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof",
        ]));
    }
    args.extend(strings(&[
        "-f",
        match container {
            MediaContainer::Matroska => "matroska",
            MediaContainer::Mp4 => "mp4",
            MediaContainer::Webm => "webm",
        },
        "pipe:1",
    ]));
    Ok(args)
}
pub fn probe(url: &str) -> Vec<OsString> {
    strings(&[
        "-v",
        "error",
        "-protocol_whitelist",
        "http,tcp",
        "-format_whitelist",
        INPUT_FORMATS,
        "-show_packets",
        "-show_entries",
        "packet=stream_index,pts_time,duration_time,flags",
        "-of",
        "compact=p=1:nk=0",
        url,
    ])
}
pub fn whisper(
    audio: &Path,
    model: &Path,
    language: Option<&str>,
) -> Result<Vec<OsString>, ErrorCode> {
    let language = language.unwrap_or("auto");
    // A literal language code only; no prompt, grammar, script, model URL or
    // output filename can enter this argument vector from the guest.
    if language != "auto" && !(language.len() == 2 || language.len() == 3) {
        return Err(ErrorCode::InvalidArgument);
    }
    if !language.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(ErrorCode::InvalidArgument);
    }
    let mut args = strings(&["-np", "-ng", "-t", "4", "-p", "1", "-l", language, "-m"]);
    args.push(model.into());
    args.push("-f".into());
    args.push(audio.into());
    Ok(args)
}
pub fn metadata(url: &str, proxy: &str) -> Vec<OsString> {
    strings(&[
        "--ignore-config",
        "--no-exec",
        "--no-plugin-dirs",
        "--no-js-runtimes",
        "--no-remote-components",
        "--no-cache-dir",
        "--no-update",
        "--no-playlist",
        "--playlist-end",
        "1",
        "--skip-download",
        "--dump-single-json",
        "--no-warnings",
        "--no-progress",
        "--no-color",
        "--socket-timeout",
        "20",
        "--retries",
        "0",
        "--extractor-retries",
        "0",
        "--proxy",
        proxy,
        "--",
        url,
    ])
}
fn seconds(value: u64) -> String {
    format!("{}.{:03}", value / 1000, value % 1000)
}
