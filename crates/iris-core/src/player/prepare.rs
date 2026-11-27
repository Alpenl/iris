//! Off-UI-thread folder scanning and photo header preparation (`safe_resolve_path` + `scan_local_or_share`).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::library::{self, Entry, FolderEntry, Kind, ScanResult};
use crate::photo::{self, Photo};

/// A folder scan and photo probe done off the UI thread, ready to load.
pub struct Prepared {
    pub(super) folder: PathBuf,
    pub(super) entries: Vec<Entry>,
    pub(super) subfolders: Vec<FolderEntry>,
    pub(super) target: Option<PathBuf>,
    pub(super) kind: Option<Kind>,
    pub(super) photo: Option<Photo>,
}

impl Prepared {
    /// The file that will load, if any; records are keyed by this path.
    pub fn target(&self) -> Option<&Path> {
        self.target.as_deref()
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn subfolders(&self) -> &[FolderEntry] {
        &self.subfolders
    }

    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }
}

/// Resolves `path` to its folder listing and photo header metadata without blocking
/// on slow network share `canonicalize` or full per-file `stat` scans.
pub fn prepare(path: &Path, cached: Option<(PathBuf, ScanResult)>) -> Result<Prepared> {
    let path = library::safe_resolve_path(path);
    let meta = std::fs::metadata(&path)?;
    let (folder, file) = if meta.is_dir() {
        (path, None)
    } else if meta.is_file() {
        (
            path.parent().unwrap_or(Path::new(".")).to_path_buf(),
            Some(path),
        )
    } else {
        anyhow::bail!("media path is not a regular file or directory");
    };
    let ScanResult {
        entries,
        subfolders,
    } = match cached {
        Some((dir, scan)) if dir == folder => scan,
        _ => library::scan_local_or_share(&folder),
    };
    let (target, kind) = match file {
        Some(file) => {
            let kind = library::kind_of(&file);
            (Some(file), kind)
        }
        None => entries
            .first()
            .map_or((None, None), |e| (Some(e.path.clone()), Some(e.kind))),
    };
    if let Some(path) = &target
        && !std::fs::metadata(path)?.is_file()
    {
        anyhow::bail!("media path is not a regular file");
    }
    let photo = (kind == Some(Kind::Photo))
        .then(|| target.as_deref().map(photo::probe))
        .flatten();
    Ok(Prepared {
        folder,
        entries,
        subfolders,
        target,
        kind,
        photo,
    })
}

/// A multi-file drop queue prepared off the UI thread, ready to load.
pub struct PreparedQueue {
    pub(super) folder: PathBuf,
    pub(super) entries: Vec<Entry>,
    pub(super) photo: Option<Photo>,
}

impl PreparedQueue {
    pub fn target(&self) -> Option<&Path> {
        self.entries.first().map(|e| e.path.as_path())
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

/// Expands selected folders in place and preserves the first occurrence of each media file.
pub fn prepare_queue(paths: Vec<PathBuf>) -> Option<PreparedQueue> {
    let mut entries = Vec::new();
    for path in paths {
        let path = library::safe_resolve_path(&path);
        let Ok(metadata) = path.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            entries.extend(library::scan_local_or_share(&path).entries);
        } else if metadata.is_file()
            && let Some(entry) = library::entry_from_path(path)
        {
            entries.push(entry);
        }
    }
    crate::catalog::dedup_suffix(&mut entries, 0, |entry| &entry.path);
    let first = entries.first()?;
    let folder = first
        .path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let photo = (first.kind == Kind::Photo).then(|| photo::probe(&first.path));
    Some(PreparedQueue {
        folder,
        entries,
        photo,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use iris_mpv::Mpv;

    use super::{prepare, prepare_queue};
    use crate::library::{self, Kind, ScanResult};
    use crate::media_uri::MediaUri;
    use crate::player::{
        Command, RemoteMediaRequest, RemoteSubtitle, Session, SessionEvent, Track, TrackKind,
        step_speed,
    };
    use crate::settings::Settings;
    use crate::test_util::TestDir;

    #[test]
    fn prepare_targets_file_or_first_entry_and_reuses_listing() {
        let dir = TestDir::new("prepare");
        for name in ["ep10.mp4", "ep2.mp4", "notes.txt"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let folder = library::safe_resolve_path(&dir);

        let opened = prepare(&dir, None).unwrap();
        assert_eq!(opened.folder(), folder);
        assert_eq!(opened.entries().len(), 2);
        assert_eq!(opened.target(), Some(folder.join("ep2.mp4").as_path()));
        assert_eq!(opened.kind(), Some(Kind::Video));

        let cached = ScanResult {
            entries: vec![opened.entries[1].clone()],
            subfolders: Vec::new(),
        };
        let file = prepare(&dir.join("ep10.mp4"), Some((folder.clone(), cached))).unwrap();
        assert_eq!(file.target(), Some(folder.join("ep10.mp4").as_path()));
        assert_eq!(file.entries().len(), 1);

        let other = prepare(
            &dir.join("ep10.mp4"),
            Some((PathBuf::from("/elsewhere"), ScanResult::default())),
        )
        .unwrap();
        assert_eq!(other.entries().len(), 2);

        assert!(prepare(&dir.join("missing.mp4"), None).is_err());
    }

    #[test]
    fn multi_open_expands_folders_in_place_without_repeating_files() {
        let dir = TestDir::new("queue-batch");
        for name in ["b.mp4", "a.mkv", "title.srt"] {
            std::fs::write(dir.join(name), []).unwrap();
        }
        let queue = prepare_queue(vec![
            dir.join("b.mp4"),
            dir.to_path_buf(),
            dir.join("a.mkv"),
        ])
        .unwrap();
        assert_eq!(
            queue
                .entries()
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["b.mp4", "a.mkv"]
        );
    }

    #[test]
    fn speed_steps_and_subtitle_safe_area_update_backend() {
        assert_eq!(step_speed(1.0, 1), 1.25);
        assert_eq!(step_speed(1.0, -1), 0.75);
        assert_eq!(step_speed(3.0, 1), 4.0);
        assert_eq!(step_speed(4.0, 1), 4.0);
        assert_eq!(step_speed(0.25, -1), 0.25);

        let mut session = Session::new().unwrap();
        let _render = session.render_handle();
        // `sub-margin-y-offset` is new in mpv 0.42. Homebrew has no 0.42 bottle
        // for Intel macs, so that CI runner links 0.41, which lacks the option.
        // Skip only on MPV_ERROR_PROPERTY_NOT_FOUND (-8); other errors still fail.
        match session.mpv.get_property_f64("sub-margin-y-offset") {
            Ok(_) => {}
            Err(e) if e.code == -8 => {
                eprintln!(
                    "skipping subtitle safe area: libmpv lacks sub-margin-y-offset (mpv < 0.42)"
                );
                session.shutdown();
                return;
            }
            Err(e) => panic!("{e}"),
        }
        let wait_for = |mpv: &Mpv, value| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if mpv.get_property_f64("sub-margin-y-offset").ok() == Some(value as f64) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("subtitle safe margin never became {value}");
        };
        session.subtitle_inset(132., 900.);
        wait_for(&session.mpv, 106);
        session.subtitle_inset(0., 900.);
        wait_for(&session.mpv, 0);
        session.shutdown();
    }

    #[test]
    fn session_validates_local_uri_and_remote_subtitles() {
        let dir = TestDir::new("session-uri");
        let clip = dir.join("demo.mp4");
        std::fs::write(&clip, b"stub").unwrap();

        let mut session = Session::new().unwrap();
        let prepared = prepare(&clip, None).unwrap();
        let target_path = prepared.target().unwrap().to_path_buf();
        session.open(prepared, 0.).unwrap();
        assert_eq!(session.current(), Some(target_path.as_path()));
        assert_eq!(
            session.current_uri(),
            Some(MediaUri::Local(target_path).to_uri_string().as_str())
        );
        assert!(session.current_uri().unwrap().starts_with("file:///"));

        let bad_scheme = RemoteMediaRequest {
            uri: MediaUri::HttpUrl("https://cdn.example.com/v.mp4".into()),
            title: "Bad".into(),
            playback_url: "lavfi://testsrc".into(),
            headers: BTreeMap::new(),
            kind: Kind::Video,
            is_live: false,
            photo: None,
            subtitles: Vec::new(),
        };
        assert!(
            session
                .open_remote(bad_scheme, &Settings::default(), 0.)
                .is_err()
        );

        let mut auth_headers = BTreeMap::new();
        auth_headers.insert("Authorization".into(), "Bearer top-secret".into());
        let invalid_attachment = RemoteMediaRequest {
            uri: MediaUri::HttpUrl("http://127.0.0.1:19999/media".into()),
            title: "Caption validation".into(),
            playback_url: "http://127.0.0.1:19999/media".into(),
            headers: BTreeMap::new(),
            kind: Kind::Video,
            is_live: false,
            photo: None,
            subtitles: vec![RemoteSubtitle::InlineText("\u{feff}#EXTM3U\n".into())],
        };
        assert!(
            session
                .open_remote(invalid_attachment, &Settings::default(), 0.)
                .is_err()
        );

        let safe_remote = RemoteMediaRequest {
            uri: MediaUri::Plugin {
                plugin_id: "iris.webdav".into(),
                source_id: "nas".into(),
                entry_id: "/ep1.mkv".into(),
            },
            title: "EP1".into(),
            playback_url: "http://127.0.0.1:19999/dav/ep1.mkv".into(),
            headers: auth_headers,
            kind: Kind::Video,
            is_live: false,
            photo: None,
            subtitles: vec![
                RemoteSubtitle::Url("http://127.0.0.1:19999/dav/ep1.chs.ass".into()),
                RemoteSubtitle::InlineText("1\n00:00:01,000 --> 00:00:02,000\nHi\n".into()),
            ],
        };
        session
            .open_remote(safe_remote, &Settings::default(), 12.5)
            .unwrap();
        assert_eq!(session.current(), None);
        assert_eq!(session.current_title(), Some("EP1"));

        session.machine.state.tracks = vec![Track {
            id: 1,
            kind: TrackKind::Audio,
            title: String::new(),
            lang: "jpn".into(),
            codec: "flac".into(),
            selected: false,
            external: false,
        }];
        session.apply(&SessionEvent::Audio("1".into()));
        assert!(session.state().tracks[0].selected);
        session
            .execute(Command::SetVideoEq {
                brightness: 150,
                contrast: -150,
                saturation: 10,
                gamma: 0,
            })
            .unwrap();
        assert_eq!(session.state().eq_brightness, 100);
        assert_eq!(session.state().eq_contrast, -100);

        // Video mirror filters automatically switch AutoSafe -> auto-copy and restore auto-safe when cleared.
        session.execute(Command::ToggleHFlip).unwrap();
        assert!(session.state().hflip);
        let wait_hwdec = |mpv: &Mpv, want: &str| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if mpv.get_property_string("hwdec").ok().as_deref() == Some(want) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!(
                "expected hwdec={want}, got {:?}",
                mpv.get_property_string("hwdec")
            );
        };
        wait_hwdec(&session.mpv, "auto-copy");
        session.execute(Command::ToggleHFlip).unwrap();
        assert!(!session.state().hflip);
        wait_hwdec(&session.mpv, "auto-safe");

        session.shutdown();
    }

    #[test]
    fn loads_real_track_list_json_from_libmpv_if_test_media_present() {
        let media_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-media");
        let mkv = ["03-bars-scope-subs.mkv", "02-tracks.mkv"]
            .into_iter()
            .map(|n| media_dir.join(n))
            .find(|p| p.is_file());
        let Some(mkv) = mkv else {
            return;
        };
        let mut session = Session::new().unwrap();
        let mut events = session.events().unwrap();
        assert!(session.events().is_none());
        let prepared = prepare(&mkv, None).unwrap();
        session.open(prepared, 0.).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline && !session.state().loaded {
            if let Ok(update) = events.try_recv() {
                session.apply_update(&update);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        session.flush_deferred();
        assert!(session.state().loaded);
        assert!(!session.tracks_of(TrackKind::Sub).is_empty());
        session.shutdown();
    }
}
