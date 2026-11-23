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
    let (uri, title) = snapshot_identity(session.current_media_uri(), || {
        session
            .current()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| session.current_title().map(String::from))
    });
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

fn snapshot_identity(
    uri: Option<MediaUri>,
    title: impl FnOnce() -> Option<String>,
) -> (Option<String>, Option<String>) {
    if uri.as_ref().is_some_and(|uri| !uri.may_share_identity()) {
        // Remote display titles can themselves contain the submitted URL.
        return (None, None);
    }
    (uri.map(|uri| uri.to_uri_string()), title())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_open_cannot_expose_url_in_snapshot_uri_or_fallback_title() {
        let signed = "https://media.test/video?arbitrary-credential=secret";
        assert_eq!(
            snapshot_identity(Some(MediaUri::parse(signed)), || Some(signed.into())),
            (None, None)
        );
        for identity in [
            "iris-plugin://cloud/drive/42",
            "https://media.test/public.mp4",
        ] {
            assert_eq!(
                snapshot_identity(Some(MediaUri::parse(identity)), || Some("Video".into())),
                (Some(identity.into()), Some("Video".into()))
            );
        }
    }
}
