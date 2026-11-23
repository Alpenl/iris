//! Read-only [`PlaybackSnapshot`] projection and cached snapshot tracking.

use iris_api::{HostEvent, MediaKind, PlaybackSnapshot};

use crate::library::Kind;
use crate::media_uri::MediaUri;
use crate::player::Session;

/// Builds a read-only [`PlaybackSnapshot`] from the current playback session.
pub fn snapshot_from_session(session: &Session, favorite: bool) -> PlaybackSnapshot {
    let state = session.state();
    let kind = session.kind().map(|k| match k {
        Kind::Video => MediaKind::Video,
        Kind::Photo => MediaKind::Photo,
        Kind::Audio => MediaKind::Audio,
    });
    let uri = session.current_uri().map(String::from).or_else(|| {
        session
            .current()
            .map(|p| MediaUri::Local(p.to_path_buf()).to_uri_string())
    });
    let title = session
        .current()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .or_else(|| session.current_title().map(String::from));
    PlaybackSnapshot {
        uri,
        title,
        kind,
        position: state.time_pos,
        duration: state.duration,
        paused: state.paused,
        volume: state.volume,
        muted: state.muted,
        speed: state.speed,
        favorite,
        chapter: state.current_chapter,
        is_live: state.is_live,
        buffering: state.buffering,
    }
}

pub(super) fn sync_cached_snapshot(cached: &mut PlaybackSnapshot, event: &HostEvent) {
    match event {
        HostEvent::MediaOpened { snapshot } | HostEvent::StateChanged { snapshot } => {
            *cached = snapshot.clone();
        }
        HostEvent::Tick {
            position,
            duration,
            paused,
        } => {
            cached.position = *position;
            cached.duration = *duration;
            cached.paused = *paused;
        }
        _ => {}
    }
}
