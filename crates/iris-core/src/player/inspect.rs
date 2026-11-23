//! Media stats, audio device enumeration, and JSON track/chapter parsers for [`Session`].

use std::path::Path;

use crate::library::Kind;

use super::Session;
use super::model::{AudioDevice, Chapter, MediaStats, Track, TrackKind};

impl Session {
    pub fn tracks_of(&self, kind: TrackKind) -> Vec<&Track> {
        self.state
            .tracks
            .iter()
            .filter(|t| t.kind == kind)
            .collect()
    }

    pub fn selected_track(&self, kind: TrackKind) -> Option<&Track> {
        self.state
            .tracks
            .iter()
            .find(|t| t.kind == kind && t.selected)
    }

    pub fn chapter_at(&self, time: f64) -> Option<&Chapter> {
        self.state.chapters.iter().rfind(|c| c.time <= time + 0.05)
    }

    /// Queries available audio output devices reported by libmpv.
    pub fn audio_devices(&self) -> Vec<AudioDevice> {
        let parsed = self
            .mpv
            .get_property_string("audio-device-list")
            .ok()
            .map(|raw| parse_audio_device_list_json(&raw))
            .unwrap_or_default();
        if parsed.is_empty() {
            vec![AudioDevice {
                name: "auto".into(),
                description: "自动选择设备".into(),
            }]
        } else {
            parsed
        }
    }

    /// Queries detailed media / decoder stats on demand (only when Info panel is open).
    pub fn media_stats(&self) -> MediaStats {
        let path = self.current.as_deref();
        let file_name = path
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| self.current_title.clone())
            .unwrap_or_default();
        let file_size = path
            .filter(|p| !crate::library::is_slow_network_path(p))
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len());
        let prop_str = |name: &str| self.mpv.get_property_string(name).unwrap_or_default();
        let prop_f64 = |name: &str| {
            self.mpv
                .get_property_f64(name)
                .ok()
                .filter(|v| v.is_finite())
        };
        let prop_i64 = |name: &str| {
            self.mpv
                .get_property_string(name)
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
        };
        let (w, h) = self.state.video_size;
        MediaStats {
            file_name,
            file_size,
            file_format: prop_str("file-format"),
            resolution: (w > 0 && h > 0).then_some((w, h)),
            video_codec: prop_str("video-codec"),
            video_fps: prop_f64("estimated-vf-fps").or_else(|| prop_f64("container-fps")),
            video_bitrate: prop_f64("video-bitrate"),
            pixel_format: prop_str("video-params/pixelformat"),
            hwdec: self.state.hwdec.clone(),
            dropped_frames: prop_i64("frame-drop-count"),
            audio_codec: prop_str("audio-codec-name"),
            audio_channels: prop_str("audio-params/channels"),
            audio_sample_rate: prop_i64("audio-params/samplerate"),
            audio_bitrate: prop_f64("audio-bitrate"),
            photo_colour: self.state.photo_colour,
            photo_exif: self.state.photo_exif.clone(),
        }
    }

    /// Refreshes only the live-changing playback metrics on an existing `MediaStats` snapshot.
    pub fn refresh_dynamic_stats(&self, stats: &mut MediaStats) {
        let prop_f64 = |name: &str| {
            self.mpv
                .get_property_f64(name)
                .ok()
                .filter(|v| v.is_finite())
        };
        let prop_i64 = |name: &str| {
            self.mpv
                .get_property_string(name)
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
        };
        let (w, h) = self.state.video_size;
        stats.resolution = (w > 0 && h > 0).then_some((w, h));
        stats.hwdec.clone_from(&self.state.hwdec);
        match self.kind {
            Some(Kind::Video) => {
                stats.video_fps =
                    prop_f64("estimated-vf-fps").or_else(|| prop_f64("container-fps"));
                stats.video_bitrate = prop_f64("video-bitrate");
                stats.dropped_frames = prop_i64("frame-drop-count");
            }
            Some(Kind::Audio) => {
                stats.audio_bitrate = prop_f64("audio-bitrate");
            }
            _ => {}
        }
    }

    pub(super) fn refresh_chapters(&mut self) {
        let Ok(raw) = self.mpv.get_property_string("chapter-list") else {
            self.state.chapters.clear();
            return;
        };
        self.state.chapters = parse_chapter_list_json(&raw);
    }

    pub(super) fn refresh_tracks(&mut self) {
        let Ok(raw) = self.mpv.get_property_string("track-list") else {
            self.state.tracks.clear();
            return;
        };
        self.state.tracks = parse_track_list_json(&raw);
    }
}

pub(super) fn parse_track_list_json(raw: &str) -> Vec<Track> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let mut tracks = Vec::with_capacity(items.len().min(64));
    for (i, item) in items.iter().take(64).enumerate() {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let kind = match obj.get("type").and_then(|v| v.as_str()) {
            Some("audio") => TrackKind::Audio,
            Some("sub") => TrackKind::Sub,
            _ => continue,
        };
        let id = obj
            .get("id")
            .and_then(|v| v.as_i64())
            .unwrap_or(i as i64 + 1);
        let str_field = |k: &str| {
            obj.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let bool_field = |k: &str| obj.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        tracks.push(Track {
            id,
            kind,
            title: str_field("title"),
            lang: str_field("lang"),
            codec: str_field("codec"),
            selected: bool_field("selected"),
            external: bool_field("external"),
        });
    }
    tracks
}

pub(super) fn parse_chapter_list_json(raw: &str) -> Vec<Chapter> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let mut chapters = Vec::with_capacity(items.len().min(128));
    for (i, item) in items.iter().take(128).enumerate() {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let time = obj
            .get("time")
            .and_then(|v| v.as_f64())
            .filter(|t| t.is_finite())
            .unwrap_or(0.);
        let title = obj
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("第 {} 章", i + 1));
        chapters.push(Chapter { title, time });
    }
    chapters
}

pub(super) fn parse_audio_device_list_json(raw: &str) -> Vec<AudioDevice> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let mut devices = Vec::with_capacity(items.len().min(64));
    for item in items.iter().take(64) {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let Some(name) = obj.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        let description = obj
            .get("description")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(name)
            .to_string();
        devices.push(AudioDevice {
            name: name.to_string(),
            description,
        });
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mpv_track_chapter_and_audio_device_json() {
        let tracks = parse_track_list_json(
            r#"[
                {"id":1,"type":"video","codec":"h264","selected":true},
                {"id":1,"type":"audio","lang":"jpn","codec":"aac","selected":true,"external":false},
                {"id":2,"type":"sub","title":"简体中文","lang":"zh","codec":"ass","selected":false,"external":true}
            ]"#,
        );
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].kind, TrackKind::Audio);
        assert!(tracks[0].selected);
        assert_eq!(tracks[1].kind, TrackKind::Sub);
        assert!(tracks[1].external);

        let chapters =
            parse_chapter_list_json(r#"[{"title":"Intro","time":0.0},{"title":"","time":42.5}]"#);
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[1].title, "第 2 章");

        let devs = parse_audio_device_list_json(
            r#"[{"name":"auto","description":"Autoselect"},{"name":"dac","description":"USB DAC"}]"#,
        );
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[1].description, "USB DAC");
    }
}
