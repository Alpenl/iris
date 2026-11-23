//! Remote stream safety helpers (`validate_stream_url`, `fetch_head_bytes`)
//! and persistent pinned location bookmarks (`LocationStore`).

pub(crate) mod http_client;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs_util;
use crate::media_uri::MediaUri;

pub use http_client::{fetch_head_bytes, validate_stream_url};

/// User-pinned local folder, network share, or `MediaSource` plugin folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedBookmark {
    pub id: String,
    pub name: String,
    pub uri: MediaUri,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
}

/// Persistent store for pinned bookmarks (`<data_dir>/locations.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LocationStore {
    #[serde(default)]
    pub bookmarks: Vec<PinnedBookmark>,
    #[serde(skip)]
    backing_path: Option<PathBuf>,
}

impl PartialEq for LocationStore {
    fn eq(&self, other: &Self) -> bool {
        self.bookmarks == other.bookmarks
    }
}

impl Eq for LocationStore {}

impl LocationStore {
    pub fn file_path() -> Option<PathBuf> {
        fs_util::data_dir().map(|d| d.join("locations.json"))
    }

    pub fn load() -> Self {
        Self::file_path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Self {
        let mut store = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .unwrap_or_default();
        store.backing_path = Some(path.to_path_buf());
        store
    }

    pub fn save(&self) {
        let target = self.backing_path.clone().or_else(Self::file_path);
        let Some(path) = target else { return };
        if let Err(e) = self.save_to(&path) {
            log::error!("saving locations to {} failed: {e}", path.display());
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs_util::atomic_write(path, json.as_bytes())
    }

    pub fn add_bookmark(&mut self, bookmark: PinnedBookmark) {
        if let Some(existing) = self.bookmarks.iter_mut().find(|b| {
            b.id == bookmark.id || (b.uri == bookmark.uri && b.folder_id == bookmark.folder_id)
        }) {
            *existing = bookmark;
        } else {
            self.bookmarks.push(bookmark);
        }
        self.save();
    }

    pub fn remove_bookmark(&mut self, id: &str) -> bool {
        let before = self.bookmarks.len();
        self.bookmarks.retain(|b| b.id != id);
        let changed = self.bookmarks.len() != before;
        if changed {
            self.save();
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_store_load_from_roundtrips_and_upgrades_legacy_webdav_bookmarks() {
        let dir = std::env::temp_dir().join(format!("iris-loc-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("locations.json");

        // Legacy locations.json containing `webdav://` string URI and retired `webdav_connections`.
        std::fs::write(
            &file,
            r#"{
                "bookmarks": [
                    { "id": "bm-1", "name": "NAS Movies", "uri": "webdav://home-nas/Movies" }
                ],
                "webdav_connections": [
                    { "id": "home-nas", "name": "Home", "endpoint": "https://nas.example.com" }
                ]
            }"#,
        )
        .unwrap();

        let mut store = LocationStore::load_from(&file);
        assert_eq!(store.bookmarks.len(), 1);
        assert_eq!(
            store.bookmarks[0].uri,
            MediaUri::Plugin {
                plugin_id: "iris.webdav".into(),
                source_id: "home-nas".into(),
                entry_id: "/Movies".into(),
            }
        );

        store.add_bookmark(PinnedBookmark {
            id: "bm-2".into(),
            name: "Cloud Anime".into(),
            uri: MediaUri::Plugin {
                plugin_id: "acme.cloud".into(),
                source_id: "drive".into(),
                entry_id: "root".into(),
            },
            folder_id: Some("folder-42".into()),
        });

        let reloaded = LocationStore::load_from(&file);
        assert_eq!(reloaded.bookmarks.len(), 2);
        assert_eq!(
            reloaded.bookmarks[1].folder_id.as_deref(),
            Some("folder-42")
        );

        let mut reloaded = reloaded;
        assert!(reloaded.remove_bookmark("bm-1"));
        assert_eq!(LocationStore::load_from(&file).bookmarks.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
