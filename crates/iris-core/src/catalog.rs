//! Unified directory catalog model (`CatalogItem`), local directory view (`LocalDirectory`),
//! and decoupled playback queue (`PlayQueue`).

use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::library::{self, Entry, FolderEntry, Kind, ScanResult, SortBy, SortOrder, natural_cmp};
use crate::media_uri::MediaUri;

/// Playable quality/variant descriptor independent of plugin transport schemas.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogVariant {
    pub id: String,
    pub label: String,
}

/// Active local folder browsing state, decoupled from playback [`crate::player::Session`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalDirectory {
    pub folder: Option<PathBuf>,
    pub entries: Vec<Entry>,
    pub subfolders: Vec<FolderEntry>,
}

impl LocalDirectory {
    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn subfolders(&self) -> &[FolderEntry] {
        &self.subfolders
    }

    pub fn set(&mut self, folder: Option<PathBuf>, entries: Vec<Entry>, subfolders: Vec<FolderEntry>) {
        self.folder = folder;
        self.entries = entries;
        self.subfolders = subfolders;
    }

    pub fn sort(&mut self, by: SortBy, order: SortOrder) {
        library::sort_entries(&mut self.entries, by, order);
    }

    pub fn enqueue(&mut self, extra: Vec<Entry>) {
        for entry in extra {
            if !self.entries.iter().any(|e| e.path == entry.path) {
                self.entries.push(entry);
            }
        }
    }

    pub fn remove_entry(&mut self, path: &Path) -> bool {
        if let Some(ix) = self.entries.iter().position(|e| e.path == path) {
            self.entries.remove(ix);
            true
        } else {
            false
        }
    }

    pub fn clear_entries(&mut self) {
        self.entries.clear();
    }

    pub fn to_catalog_items(&self) -> Vec<CatalogItem> {
        self.subfolders
            .iter()
            .map(CatalogItem::from_local_folder)
            .chain(self.entries.iter().map(CatalogItem::from_local_entry))
            .collect()
    }
}

/// Distinguishes navigable folders from playable/viewable media items.
#[derive(Clone, Debug, PartialEq)]
pub enum CatalogItemKind {
    Folder { folder_id: Option<String> },
    Media(Kind),
}

/// Unified directory item across local folders, `MediaSource` plugins, and favorites.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogItem {
    pub uri: MediaUri,
    pub name: String,
    pub kind: CatalogItemKind,
    pub size: u64,
    pub modified_secs: u64,
    pub duration: Option<f64>,
    pub subtitle: Option<String>,
    pub is_live: bool,
    pub variants: Vec<CatalogVariant>,
}

impl CatalogItem {
    pub fn from_local_folder(folder: &FolderEntry) -> Self {
        Self {
            uri: MediaUri::Local(folder.path.clone()),
            name: folder.name.clone(),
            kind: CatalogItemKind::Folder { folder_id: None },
            size: 0,
            modified_secs: 0,
            duration: None,
            subtitle: None,
            is_live: false,
            variants: Vec::new(),
        }
    }

    pub fn from_local_entry(entry: &Entry) -> Self {
        Self {
            uri: MediaUri::Local(entry.path.clone()),
            name: entry.name.clone(),
            kind: CatalogItemKind::Media(entry.kind),
            size: entry.size,
            modified_secs: entry.modified_secs,
            duration: None,
            subtitle: None,
            is_live: false,
            variants: Vec::new(),
        }
    }

    pub fn from_scan_result(scan: &ScanResult) -> Vec<Self> {
        let mut out = Vec::with_capacity(scan.subfolders.len() + scan.entries.len());
        out.extend(scan.subfolders.iter().map(Self::from_local_folder));
        out.extend(scan.entries.iter().map(Self::from_local_entry));
        out
    }

    pub fn is_folder(&self) -> bool {
        matches!(self.kind, CatalogItemKind::Folder { .. })
    }

    pub fn media_kind(&self) -> Option<Kind> {
        match self.kind {
            CatalogItemKind::Media(k) => Some(k),
            CatalogItemKind::Folder { .. } => None,
        }
    }
}

impl From<&FolderEntry> for CatalogItem {
    fn from(folder: &FolderEntry) -> Self {
        Self::from_local_folder(folder)
    }
}

impl From<&Entry> for CatalogItem {
    fn from(entry: &Entry) -> Self {
        Self::from_local_entry(entry)
    }
}

/// Sorts `items` in-place, always keeping folders above media items.
/// `ascending = true` sorts in ascending order (`SortOrder::Asc`); `false` sorts descending.
pub fn sort_catalog_items(items: &mut [CatalogItem], by: SortBy, ascending: bool) {
    items.sort_by(|a, b| match (a.is_folder(), b.is_folder()) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (true, true) => {
            let ord = natural_cmp(&a.name, &b.name);
            if by == SortBy::Name && !ascending {
                ord.reverse()
            } else {
                ord
            }
        }
        (false, false) => {
            let ord = match by {
                SortBy::Name => natural_cmp(&a.name, &b.name),
                SortBy::Modified => a
                    .modified_secs
                    .cmp(&b.modified_secs)
                    .then_with(|| natural_cmp(&a.name, &b.name)),
                SortBy::Size => a
                    .size
                    .cmp(&b.size)
                    .then_with(|| natural_cmp(&a.name, &b.name)),
            };
            if ascending { ord } else { ord.reverse() }
        }
    });
}

/// Convenience wrapper accepting [`SortOrder`] directly.
pub fn sort_catalog_items_with_order(items: &mut [CatalogItem], by: SortBy, order: SortOrder) {
    sort_catalog_items(items, by, order == SortOrder::Asc);
}

/// Category / favorite filter applied to [`CatalogItem`] lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MediaFilter {
    #[default]
    All,
    Video,
    Photo,
    Audio,
    FavoritesOnly,
}

/// Pure predicate checking whether `item` passes keyword `query`, `media_filter`, and `is_favorite`.
pub fn matches_catalog_filter(
    item: &CatalogItem,
    query: &str,
    media_filter: MediaFilter,
    is_favorite: impl Fn(&MediaUri) -> bool,
) -> bool {
    let type_ok = match media_filter {
        MediaFilter::All => true,
        MediaFilter::Video => item.media_kind() == Some(Kind::Video),
        MediaFilter::Photo => item.media_kind() == Some(Kind::Photo),
        MediaFilter::Audio => item.media_kind() == Some(Kind::Audio),
        MediaFilter::FavoritesOnly => is_favorite(&item.uri),
    };
    if !type_ok {
        return false;
    }

    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    let needle = q.to_lowercase();
    item.name.to_lowercase().contains(&needle)
        || item
            .subtitle
            .as_deref()
            .is_some_and(|sub| sub.to_lowercase().contains(&needle))
}

/// Filters `items` by keyword `query`, `media_filter`, and `is_favorite`.
pub fn filter_catalog_items(
    items: &[CatalogItem],
    query: &str,
    media_filter: MediaFilter,
    is_favorite: impl Fn(&MediaUri) -> bool,
) -> Vec<CatalogItem> {
    items
        .iter()
        .filter(|item| matches_catalog_filter(item, query, media_filter, &is_favorite))
        .cloned()
        .collect()
}

/// Compacts a newly appended suffix in input order, without cloning any keys.
pub(crate) fn dedup_suffix<T, K: Eq + std::hash::Hash + ?Sized>(
    items: &mut Vec<T>,
    start: usize,
    key: impl Fn(&T) -> &K,
) {
    if start == items.len() {
        return;
    }
    let mut seen = HashSet::with_capacity(items.len());
    seen.extend(items[..start].iter().map(&key));
    let duplicates: Vec<usize> = items[start..]
        .iter()
        .enumerate()
        .filter_map(|(index, item)| (!seen.insert(key(item))).then_some(start + index))
        .collect();
    if let Some(&first) = duplicates.first() {
        let mut duplicates = duplicates.into_iter().peekable();
        let mut index = first;
        items
            .extract_if(first.., |_| {
                let duplicate = duplicates.next_if_eq(&index).is_some();
                index += 1;
                duplicate
            })
            .for_each(drop);
    }
}

/// Single playable/viewable item inside [`PlayQueue`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    pub uri: MediaUri,
    pub title: String,
    pub kind: Kind,
}

impl QueueItem {
    pub fn new(uri: MediaUri, title: impl Into<String>, kind: Kind) -> Self {
        Self {
            uri,
            title: title.into(),
            kind,
        }
    }

    pub fn from_catalog_item(item: &CatalogItem) -> Option<Self> {
        let kind = item.media_kind()?;
        Some(Self {
            uri: item.uri.clone(),
            title: item.name.clone(),
            kind,
        })
    }

    pub fn from_local_entry(entry: &Entry) -> Self {
        Self {
            uri: MediaUri::Local(entry.path.clone()),
            title: entry.name.clone(),
            kind: entry.kind,
        }
    }
}

/// Independent playback queue decoupled from the currently browsed sidebar directory.
/// Supports mixed local, HTTP(S), and `MediaSource` plugin items.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayQueue {
    items: Vec<QueueItem>,
    /// Insertion points left by recent removals, preserving the active item's anchor across consecutive deletes.
    #[serde(skip)]
    removed: Vec<(MediaUri, usize)>,
}

impl PlayQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&QueueItem> {
        self.items.get(index)
    }

    pub fn position_of(&self, uri: &MediaUri) -> Option<usize> {
        self.items.iter().position(|it| &it.uri == uri)
    }

    pub fn contains(&self, uri: &MediaUri) -> bool {
        self.position_of(uri).is_some()
    }

    /// Appends `item` if its `uri` is not already in the queue. Returns `true` if inserted.
    pub fn push(&mut self, item: QueueItem) -> bool {
        if self.contains(&item.uri) {
            return false;
        }
        self.items.push(item);
        true
    }

    /// Appends multiple `QueueItem`s, deduplicating by `uri`. Returns count inserted.
    pub fn extend(&mut self, extra: impl IntoIterator<Item = QueueItem>) -> usize {
        let start = self.items.len();
        self.items.extend(extra);
        dedup_suffix(&mut self.items, start, |item| &item.uri);
        self.items.len() - start
    }

    /// Appends all media items from `catalog_items` (skipping folders), deduplicating by `uri`.
    pub fn extend_catalog(&mut self, catalog_items: &[CatalogItem]) -> usize {
        self.extend(
            catalog_items
                .iter()
                .filter_map(QueueItem::from_catalog_item),
        )
    }

    /// Replaces the entire queue with media items from `catalog_items`.
    pub fn replace_from_catalog(&mut self, catalog_items: &[CatalogItem]) {
        self.clear();
        self.extend_catalog(catalog_items);
    }

    /// Replaces the entire queue from local [`Entry`] items.
    pub fn replace_from_entries(&mut self, entries: &[Entry]) {
        self.clear();
        self.extend(entries.iter().map(QueueItem::from_local_entry));
    }

    /// Removes the item matching `uri`. Returns `true` if removed.
    pub fn remove(&mut self, uri: &MediaUri) -> bool {
        self.position_of(uri)
            .and_then(|ix| self.remove_at(ix))
            .is_some()
    }

    pub fn remove_at(&mut self, index: usize) -> Option<QueueItem> {
        if index >= self.items.len() {
            return None;
        }
        let item = self.items.remove(index);
        for (_, saved_ix) in &mut self.removed {
            if index < *saved_ix {
                *saved_ix -= 1;
            }
        }
        self.removed.retain(|(u, _)| u != &item.uri);
        if self.removed.len() >= 16 {
            self.removed.remove(0);
        }
        self.removed.push((item.uri.clone(), index));
        Some(item)
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.removed.clear();
    }

    pub fn dedup(&mut self) {
        dedup_suffix(&mut self.items, 0, |item| &item.uri);
        self.removed.clear();
    }

    pub fn sort_by_title(&mut self, ascending: bool) {
        self.items.sort_by(|a, b| {
            let ord = natural_cmp(&a.title, &b.title);
            if ascending { ord } else { ord.reverse() }
        });
        self.removed.clear();
    }

    /// Steps relative to the current item or to the gap left by its removal.
    /// With no known position, starts from the first (next) or last (previous) item.
    pub fn step(&self, current: &MediaUri, delta: isize, wrap: bool) -> Option<&QueueItem> {
        let len = self.items.len();
        if len == 0 {
            return None;
        }
        let (index, offset) = if let Some(index) = self.position_of(current) {
            (index, delta)
        } else if delta == 0 {
            return None;
        } else if let Some((_, index)) = self.removed.iter().rfind(|(uri, _)| uri == current) {
            (*index, delta - isize::from(delta > 0))
        } else if delta < 0 {
            return self.items.last();
        } else {
            return self.items.first();
        };
        let target = if wrap {
            (index + offset.rem_euclid(len as isize) as usize) % len
        } else {
            index.checked_add_signed(offset)?
        };
        self.items.get(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn catalog_sort_pins_folders_first_and_filters_cleanly() {
        let mut items = vec![
            CatalogItem::from_local_entry(&Entry {
                path: PathBuf::from("/v/ep10.mkv"),
                name: "ep10.mkv".into(),
                kind: Kind::Video,
                size: 500,
                modified_secs: 20,
            }),
            CatalogItem::from_local_folder(&FolderEntry {
                path: PathBuf::from("/v/Extras"),
                name: "Extras".into(),
            }),
            CatalogItem::from_local_entry(&Entry {
                path: PathBuf::from("/v/ep2.mkv"),
                name: "ep2.mkv".into(),
                kind: Kind::Video,
                size: 100,
                modified_secs: 50,
            }),
            CatalogItem {
                uri: MediaUri::Plugin {
                    plugin_id: "acme.cloud".into(),
                    source_id: "drive".into(),
                    entry_id: "ost-1".into(),
                },
                name: "OST Track 1.flac".into(),
                kind: CatalogItemKind::Media(Kind::Audio),
                size: 300,
                modified_secs: 15,
                duration: Some(210.0),
                subtitle: Some("Lossless".into()),
                is_live: false,
                variants: Vec::new(),
            },
        ];

        sort_catalog_items(&mut items, SortBy::Name, true);
        assert_eq!(items[0].name, "Extras");
        assert_eq!(items[1].name, "ep2.mkv");
        assert_eq!(items[2].name, "ep10.mkv");
        assert_eq!(items[3].name, "OST Track 1.flac");

        // Descending size sort still keeps folders pinned on top.
        sort_catalog_items(&mut items, SortBy::Size, false);
        assert!(items[0].is_folder());
        assert_eq!(items[1].name, "ep10.mkv");
        assert_eq!(items[2].name, "OST Track 1.flac");
        assert_eq!(items[3].name, "ep2.mkv");

        let fav_uri = MediaUri::Local(PathBuf::from("/v/ep2.mkv"));
        let only_audio = filter_catalog_items(&items, "lossless", MediaFilter::Audio, |_| false);
        assert_eq!(only_audio.len(), 1);
        assert_eq!(only_audio[0].name, "OST Track 1.flac");

        let only_fav =
            filter_catalog_items(&items, "", MediaFilter::FavoritesOnly, |u| u == &fav_uri);
        assert_eq!(only_fav.len(), 1);
        assert_eq!(only_fav[0].name, "ep2.mkv");
    }

    #[test]
    fn bulk_queue_keeps_first_occurrence_and_input_order() {
        let make_item = |i: usize, title: &str| {
            QueueItem::new(
                MediaUri::Local(format!("{i}.mp4").into()),
                title,
                Kind::Video,
            )
        };
        let mut queue = PlayQueue::new();
        queue.push(make_item(0, "existing"));
        assert_eq!(
            queue.extend(
                (0..10_000).flat_map(|i| [make_item(i, "first"), make_item(i, "duplicate")])
            ),
            9_999
        );
        assert_eq!(queue.len(), 10_000);
        assert_eq!(queue.items()[0].title, "existing");
        for (i, item) in queue.items().iter().enumerate().skip(1) {
            assert_eq!(item, &make_item(i, "first"));
        }
    }

    #[test]
    fn play_queue_supports_mixed_sources_dedup_and_step_wrap() {
        let local_uri = MediaUri::Local(PathBuf::from("/local/ep1.mp4"));
        let cloud_uri = MediaUri::Plugin {
            plugin_id: "iris.webdav".into(),
            source_id: "nas".into(),
            entry_id: "/Anime/ep2.mkv".into(),
        };
        let http_uri = MediaUri::HttpUrl("https://cdn.example.com/ep3.mp4".into());

        let mut q = PlayQueue::new();
        assert!(q.push(QueueItem::new(local_uri.clone(), "ep10.mp4", Kind::Video)));
        assert!(!q.push(QueueItem::new(local_uri.clone(), "ep10.mp4", Kind::Video)));
        assert!(q.push(QueueItem::new(cloud_uri.clone(), "ep2.mkv", Kind::Video)));
        assert!(q.push(QueueItem::new(http_uri.clone(), "ep3.mp4", Kind::Video)));
        assert_eq!(q.len(), 3);

        q.sort_by_title(true);
        assert_eq!(q.items()[0].uri, cloud_uri);
        assert_eq!(q.items()[1].uri, http_uri);
        assert_eq!(q.items()[2].uri, local_uri);

        assert_eq!(q.step(&cloud_uri, -1, false), None);
        assert_eq!(
            q.step(&cloud_uri, -1, true).map(|i| &i.uri),
            Some(&local_uri)
        );
        assert_eq!(q.step(&local_uri, 1, false), None);
        assert_eq!(
            q.step(&local_uri, 1, true).map(|i| &i.uri),
            Some(&cloud_uri)
        );
        assert_eq!(
            q.step(&cloud_uri, 1, false).map(|i| &i.uri),
            Some(&http_uri)
        );

        assert!(q.remove(&http_uri));
        assert_eq!(q.len(), 2);
        assert_eq!(
            q.step(&http_uri, 1, false).map(|q| &q.uri),
            Some(&local_uri)
        );
        assert_eq!(
            q.step(&http_uri, -1, false).map(|q| &q.uri),
            Some(&cloud_uri)
        );
        q.clear();
        assert!(q.is_empty());
    }
}
