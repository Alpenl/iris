use iris_api::v2::{Id, source::PlaybackSnapshot};
use iris_core::library::Kind;
use iris_core::player::PlaybackSnapshotInput;

/// Maps a pure [`PlaybackSnapshotInput`] into a protocol-2 [`PlaybackSnapshot`],
/// enforcing host-side identity-sharing privacy (`private_identity` and
/// [`iris_core::media_uri::MediaUri::may_share_identity`]).
pub fn snapshot_from_input(
    input: &PlaybackSnapshotInput,
    favorite: bool,
    media: Id,
) -> PlaybackSnapshot {
    let kind = input.kind.map(|k| match k {
        Kind::Video => iris_api::MediaKind::Video,
        Kind::Photo => iris_api::MediaKind::Photo,
        Kind::Audio => iris_api::MediaKind::Audio,
    });
    let identity = input
        .media_uri
        .as_ref()
        .filter(|uri| !input.private_identity && uri.may_share_identity());
    let (uri, title) = if let Some(uri) = identity {
        (
            Some(uri.to_uri_string()),
            input
                .local_file_name
                .clone()
                .or_else(|| input.current_title.clone()),
        )
    } else {
        (None, None)
    };
    PlaybackSnapshot {
        media: kind.as_ref().map(|_| media),
        uri,
        title,
        kind,
        position: input.position,
        duration: input.duration,
        paused: input.paused,
        volume: input.volume,
        muted: input.muted,
        speed: input.speed,
        favorite,
        chapter: u32::try_from(input.current_chapter).ok(),
        is_live: input.is_live,
        buffering: input.buffering,
    }
}

#[cfg(test)]
mod tests {
    use super::snapshot_from_input;
    use iris_api::{MediaKind, v2::Id};
    use iris_core::{library::Kind, media_uri::MediaUri, player::PlaybackSnapshotInput};

    #[test]
    fn snapshot_from_input_enforces_identity_privacy_and_maps_playback_fields() {
        #[cfg(windows)]
        let sample_path = std::path::PathBuf::from(r"C:\videos\sample.mkv");
        #[cfg(not(windows))]
        let sample_path = std::path::PathBuf::from("/videos/sample.mkv");
        let expected_uri = url::Url::from_file_path(&sample_path).unwrap().to_string();
        let local = PlaybackSnapshotInput {
            kind: Some(Kind::Video),
            media_uri: Some(MediaUri::Local(sample_path)),
            local_file_name: Some("sample.mkv".into()),
            current_title: Some("Fallback".into()),
            private_identity: false,
            position: 42.25,
            duration: 120.0,
            paused: true,
            volume: 85.0,
            muted: false,
            speed: 1.5,
            current_chapter: 3,
            is_live: false,
            buffering: true,
        };
        let snap = snapshot_from_input(&local, true, Id(7));
        assert_eq!(snap.media, Some(Id(7)));
        assert_eq!(snap.kind, Some(MediaKind::Video));
        assert_eq!(snap.uri.as_deref(), Some(expected_uri.as_str()));
        assert_eq!(snap.title.as_deref(), Some("sample.mkv"));
        assert_eq!(snap.chapter, Some(3));
        assert!(snap.favorite);
        assert!(snap.buffering);

        // Private identity (e.g. URL-resolver stream) never leaks URI or title.
        let mut private = local.clone();
        private.private_identity = true;
        let private_snap = snapshot_from_input(&private, false, Id(7));
        assert_eq!(private_snap.media, Some(Id(7)));
        assert_eq!(private_snap.uri, None);
        assert_eq!(private_snap.title, None);

        // HTTP URL with secret query token (`may_share_identity() == false`) strips URI and title.
        let tokenized_http = PlaybackSnapshotInput {
            kind: Some(Kind::Audio),
            media_uri: Some(MediaUri::HttpUrl(
                "https://cdn.example.com/track.flac?sig=secret".into(),
            )),
            local_file_name: None,
            current_title: Some("Secret Stream".into()),
            private_identity: false,
            current_chapter: -1,
            ..Default::default()
        };
        let http_snap = snapshot_from_input(&tokenized_http, false, Id(9));
        assert_eq!(http_snap.media, Some(Id(9)));
        assert_eq!(http_snap.kind, Some(MediaKind::Audio));
        assert_eq!(http_snap.uri, None);
        assert_eq!(http_snap.title, None);
        assert_eq!(http_snap.chapter, None);

        // Idle player (`kind: None`) clears media Id as well.
        let idle = snapshot_from_input(&PlaybackSnapshotInput::default(), false, Id(9));
        assert_eq!(idle.media, None);
        assert_eq!(idle.kind, None);
        assert_eq!(idle.uri, None);
        assert_eq!(idle.title, None);
    }
}
