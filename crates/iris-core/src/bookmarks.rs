//! Persistent pinned location bookmarks (`<data_dir>/locations.json`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs_util;
use crate::legacy::webdav;
use crate::media_uri::MediaUri;

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
        let mut store = fs_util::retry_sharing_violation(|| std::fs::read_to_string(path))
            .ok()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .unwrap_or_default();
        store.backing_path = Some(path.to_path_buf());
        store
    }

    pub fn reload(&mut self) {
        let target = self.backing_path.clone().or_else(Self::file_path);
        let Some(path) = target else { return };
        match fs_util::retry_sharing_violation(|| std::fs::read_to_string(&path)) {
            Ok(text) => {
                if let Ok(disk) = serde_json::from_str::<Self>(&text) {
                    self.bookmarks = disk.bookmarks;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.bookmarks.clear();
            }
            Err(_) => {}
        }
        if self.backing_path.is_none() {
            self.backing_path = Some(path);
        }
    }

    fn sync_legacy_webdav(&mut self, incoming: Option<&mut MediaUri>) {
        let target = self.backing_path.clone().or_else(Self::file_path);
        let Some(path) = target else { return };
        let Ok(text) = fs_util::retry_sharing_violation(|| std::fs::read_to_string(&path)) else {
            return;
        };
        let Ok(disk) = serde_json::from_str::<Self>(&text) else {
            return;
        };
        let mut remapped = BTreeMap::new();
        for local in &mut self.bookmarks {
            let Some(on_disk) = disk.bookmarks.iter().find(|b| b.id == local.id) else {
                continue;
            };
            if let (
                MediaUri::Plugin {
                    plugin_id: lp,
                    source_id: ls,
                    entry_id: le,
                },
                MediaUri::Plugin {
                    plugin_id: dp,
                    source_id: ds,
                    entry_id: de,
                },
            ) = (&mut local.uri, &on_disk.uri)
            {
                webdav::reconcile_migrated_source(lp, ls, le, dp, ds, de, &mut remapped);
            }
        }
        if let Some(MediaUri::Plugin {
            plugin_id,
            source_id,
            ..
        }) = incoming
        {
            webdav::apply_remapped_source(plugin_id, source_id, &remapped);
        }
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

    pub fn add_bookmark(&mut self, mut bookmark: PinnedBookmark) {
        self.sync_legacy_webdav(Some(&mut bookmark.uri));
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
        self.sync_legacy_webdav(None);
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
    use super::{LocationStore, MediaUri, PinnedBookmark};

    #[test]
    fn location_store_load_from_roundtrips_and_upgrades_legacy_webdav_bookmarks() {
        let dir = crate::test_util::TestDir::new("loc-store");
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
    }

    #[test]
    fn location_store_reload_and_mutations_preserve_background_webdav_migration() {
        let dir = crate::test_util::TestDir::new("loc-store-webdav-sync");
        let file = dir.join("locations.json");
        let legacy_id = "legacy-overlong-webdav-server-identifier-exceeding-48-bytes";
        let migrated_id = "v1-0123456789abcdef0123456789abcdef";

        std::fs::write(
            &file,
            serde_json::json!({
                "bookmarks": [
                    {
                        "id": "bm-dav",
                        "name": "Legacy NAS",
                        "uri": format!("iris-plugin://iris.webdav/{legacy_id}/%2FMovies")
                    },
                    {
                        "id": "bm-local",
                        "name": "Local",
                        "uri": MediaUri::from(dir.join("local"))
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let mut window_store = LocationStore::load_from(&file);
        let mut window_store_dav_pin = LocationStore::load_from(&file);
        let mut window_store_remove = LocationStore::load_from(&file);

        // Simulate background Bootstrap::scan rewriting locations.json on disk.
        let mut disk_migrated = LocationStore::load_from(&file);
        disk_migrated.bookmarks[0].uri = MediaUri::Plugin {
            plugin_id: "iris.webdav".into(),
            source_id: migrated_id.into(),
            entry_id: "/Movies".into(),
        };
        disk_migrated.save_to(&file).unwrap();

        // 1. Adding a bookmark before UI reload must not clobber the migrated WebDAV source_id.
        window_store.add_bookmark(PinnedBookmark {
            id: "bm-extra".into(),
            name: "Extra".into(),
            uri: MediaUri::Plugin {
                plugin_id: "acme.cloud".into(),
                source_id: "main".into(),
                entry_id: "root".into(),
            },
            folder_id: None,
        });
        let after_add = LocationStore::load_from(&file);
        assert_eq!(after_add.bookmarks.len(), 3);
        assert_eq!(
            after_add.bookmarks[0].uri,
            MediaUri::Plugin {
                plugin_id: "iris.webdav".into(),
                source_id: migrated_id.into(),
                entry_id: "/Movies".into(),
            }
        );

        // 1b. Pinning another folder on the same pre-reload WebDAV connection aligns its source_id.
        window_store_dav_pin.add_bookmark(PinnedBookmark {
            id: "bm-dav-2".into(),
            name: "Legacy NAS Shows".into(),
            uri: MediaUri::Plugin {
                plugin_id: "iris.webdav".into(),
                source_id: legacy_id.into(),
                entry_id: "/Shows".into(),
            },
            folder_id: Some("/Shows".into()),
        });
        let after_dav_pin = LocationStore::load_from(&file);
        assert_eq!(
            after_dav_pin
                .bookmarks
                .iter()
                .find(|b| b.id == "bm-dav-2")
                .map(|b| &b.uri),
            Some(&MediaUri::Plugin {
                plugin_id: "iris.webdav".into(),
                source_id: migrated_id.into(),
                entry_id: "/Shows".into(),
            })
        );

        // 2. Reloading refreshes bookmarks while preserving backing_path for future saves.
        window_store_remove.reload();
        assert_eq!(window_store_remove.bookmarks.len(), 3);
        assert!(window_store_remove.remove_bookmark("bm-local"));
        let after_remove = LocationStore::load_from(&file);
        assert_eq!(after_remove.bookmarks.len(), 2);
        assert_eq!(
            after_remove.bookmarks[0].uri,
            MediaUri::Plugin {
                plugin_id: "iris.webdav".into(),
                source_id: migrated_id.into(),
                entry_id: "/Movies".into(),
            }
        );
    }
}
