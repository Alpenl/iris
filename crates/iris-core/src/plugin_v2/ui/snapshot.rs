use super::*;
use crate::{library::Kind, player::Session};

pub fn snapshot_from_session(session: &Session, favorite: bool, media: Id) -> PlaybackSnapshot {
    let state = session.state();
    let kind = session.kind().map(|k| match k {
        Kind::Video => iris_api::MediaKind::Video,
        Kind::Photo => iris_api::MediaKind::Photo,
        Kind::Audio => iris_api::MediaKind::Audio,
    });
    let identity = session
        .current_media_uri()
        .filter(|uri| session.may_share_current_identity() && uri.may_share_identity());
    let (uri, title) = if let Some(uri) = identity {
        (
            Some(uri.to_uri_string()),
            session
                .current()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .or_else(|| session.current_title().map(str::to_owned)),
        )
    } else {
        (None, None)
    };
    PlaybackSnapshot {
        media: kind.as_ref().map(|_| media),
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
        chapter: u32::try_from(state.current_chapter).ok(),
        is_live: state.is_live,
        buffering: state.buffering,
    }
}
