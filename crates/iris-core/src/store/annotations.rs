//! Personal metadata lives only in the application Store, never in media files.
use crate::{fs_util, media_uri::MediaUri};
use iris_api::v2::{
    ErrorCode,
    enhance::{AnnotationEdit, ColorLabel, TimeMarker},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

const BYTE_LIMIT: usize = 16 << 20;
const MEDIA_LIMIT: usize = 4096;
const MARKER_LIMIT: usize = 10_000;
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Annotations {
    #[serde(default)]
    pub rating: u8,
    pub color: Option<ColorLabel>,
    #[serde(default)]
    pub markers: BTreeMap<String, TimeMarker>,
}
impl Annotations {
    fn edit(&mut self, edit: &AnnotationEdit) -> Result<(), ErrorCode> {
        crate::plugin_v2::validation::annotation(edit)?;
        match edit {
            AnnotationEdit::Rating { rating } => self.rating = *rating,
            AnnotationEdit::Color { color } => self.color = *color,
            AnnotationEdit::UpsertMarker { marker } => {
                if self.markers.len() >= 1000 && !self.markers.contains_key(&marker.id) {
                    return Err(ErrorCode::QuotaExceeded);
                }
                self.markers.insert(marker.id.clone(), marker.clone());
            }
            AnnotationEdit::RemoveMarker { id } => {
                self.markers.remove(id);
            }
        }
        Ok(())
    }
    fn empty(&self) -> bool {
        self.rating == 0 && self.color.is_none() && self.markers.is_empty()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Field {
    Rating,
    Color,
    Marker(String),
}
fn field(edit: &AnnotationEdit) -> Field {
    match edit {
        AnnotationEdit::Rating { .. } => Field::Rating,
        AnnotationEdit::Color { .. } => Field::Color,
        AnnotationEdit::UpsertMarker { marker } => Field::Marker(marker.id.clone()),
        AnnotationEdit::RemoveMarker { id } => Field::Marker(id.clone()),
    }
}
mod background;
pub use background::{AnnotationSnapshot, AnnotationTarget};
type Records = BTreeMap<String, Annotations>;
#[derive(Default)]
pub(super) struct AnnotationStore {
    records: Arc<Records>,
    revision: u64,
    lease: Option<Arc<crate::plugin_v2::ledger::Reservation>>,
    edits: BTreeMap<String, BTreeMap<Field, AnnotationEdit>>,
    directory: Option<PathBuf>,
}
impl std::fmt::Debug for AnnotationStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnnotationStore")
            .field("records", &self.records.len())
            .field("pending", &self.edits.len())
            .finish()
    }
}
impl AnnotationStore {
    pub fn load(directory: &Path) -> Self {
        let records = read(&directory.join("annotations.json")).unwrap_or_else(|_| {
            log::warn!("could not load personal annotations; existing data will not be replaced");
            BTreeMap::new()
        });
        Self {
            records: Arc::new(records),
            directory: Some(directory.into()),
            ..Default::default()
        }
    }
    pub fn get(&self, uri: &MediaUri) -> Option<&Annotations> {
        self.records.get(&uri.to_uri_string())
    }
    pub fn dirty(&self) -> bool {
        !self.edits.is_empty()
    }
    pub fn edit(&mut self, uris: &[MediaUri], edit: &AnnotationEdit) -> Result<(), ErrorCode> {
        if uris.is_empty() || uris.len() > 1000 {
            return Err(ErrorCode::InvalidArgument);
        }
        crate::plugin_v2::validation::annotation(edit)?;
        let mut next = (*self.records).clone();
        for uri in uris {
            if !uri.may_share_identity() {
                return Err(ErrorCode::Denied);
            }
            let key = uri.to_uri_string();
            if key.len() > 8192 {
                return Err(ErrorCode::QuotaExceeded);
            }
            next.entry(key.clone()).or_default().edit(edit)?;
            if next[&key].empty() {
                next.remove(&key);
            }
        }
        validate(&next)?;
        // Coalesce per field so a plugin cannot grow an unbounded mutation log.
        let mut pending = self.edits.clone();
        for uri in uris {
            pending
                .entry(uri.to_uri_string())
                .or_default()
                .insert(field(edit), edit.clone());
        }
        if pending.len() > MEDIA_LIMIT
            || pending.values().map(BTreeMap::len).sum::<usize>() > MARKER_LIMIT + 2 * MEDIA_LIMIT
        {
            return Err(ErrorCode::QuotaExceeded);
        }
        self.records = Arc::new(next);
        self.edits = pending;
        Ok(())
    }
    pub fn save(&mut self) -> Result<(), ErrorCode> {
        if !self.dirty() {
            return Ok(());
        }
        let directory = self
            .directory
            .clone()
            .or_else(fs_util::data_dir)
            .ok_or(ErrorCode::IoError)?;
        std::fs::create_dir_all(&directory).map_err(|_| ErrorCode::IoError)?;
        let _lock = fs_util::lock_file(&directory.join("annotations.lock")).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                ErrorCode::Backpressure
            } else {
                ErrorCode::IoError
            }
        })?;
        let path = directory.join("annotations.json");
        let mut latest = read(&path)?;
        for (uri, edits) in &self.edits {
            let value = latest.entry(uri.clone()).or_default();
            for edit in edits.values() {
                value.edit(edit)?;
            }
            if value.empty() {
                latest.remove(uri);
            }
        }
        validate(&latest)?;
        let revision = crate::plugin_v2::instance::next_id()?;
        let bytes = iris_plugin_sdk::json::encode(&latest, BYTE_LIMIT)?;
        fs_util::atomic_write_private(&path, &bytes).map_err(|_| ErrorCode::IoError)?;
        self.revision = revision;
        self.records = Arc::new(latest);
        self.edits.clear();
        Ok(())
    }
}
fn read(path: &Path) -> Result<Records, ErrorCode> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(_) => return Err(ErrorCode::IoError),
    };
    let mut bytes = Vec::new();
    file.take(BYTE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ErrorCode::IoError)?;
    let value: Records = iris_plugin_sdk::json::decode_exact(&bytes, BYTE_LIMIT)?;
    validate(&value)?;
    Ok(value)
}
fn validate(records: &Records) -> Result<(), ErrorCode> {
    if records.len() > MEDIA_LIMIT
        || records.values().map(|v| v.markers.len()).sum::<usize>() > MARKER_LIMIT
    {
        return Err(ErrorCode::QuotaExceeded);
    }
    for (key, value) in records {
        if key.len() > 8192
            || !MediaUri::parse(key).may_share_identity()
            || value.rating > 5
            || value.markers.len() > 1000
        {
            return Err(ErrorCode::InvalidArgument);
        }
        for (id, marker) in &value.markers {
            if id != &marker.id {
                return Err(ErrorCode::InvalidArgument);
            }
            crate::plugin_v2::validation::annotation(&AnnotationEdit::UpsertMarker {
                marker: marker.clone(),
            })?;
        }
    }
    iris_plugin_sdk::json::encode(records, BYTE_LIMIT).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_api::v2::Id;
    #[test]
    fn annotations_merge_fields_across_instances_and_never_write_media() {
        let dir = std::env::temp_dir().join(format!(
            "iris-annotations-{}",
            crate::plugin_v2::files::random_name().unwrap()
        ));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("video.mp4");
        std::fs::write(&source, b"source bytes").unwrap();
        let uri = MediaUri::Local(source.clone());
        let uris = [uri.clone()];
        let mut a = AnnotationStore::load(&dir);
        let mut b = AnnotationStore::load(&dir);
        a.edit(&uris, &AnnotationEdit::Rating { rating: 4 })
            .unwrap();
        a.save().unwrap();
        b.edit(
            &uris,
            &AnnotationEdit::UpsertMarker {
                marker: TimeMarker {
                    id: "note".into(),
                    position_ms: Id(1250),
                    label: "Review".into(),
                    color: Some(ColorLabel::Blue),
                },
            },
        )
        .unwrap();
        b.save().unwrap();
        a.edit(
            &uris,
            &AnnotationEdit::Color {
                color: Some(ColorLabel::Green),
            },
        )
        .unwrap();
        a.save().unwrap();
        let value = AnnotationStore::load(&dir);
        let value = value.get(&uri).unwrap();
        assert_eq!(value.rating, 4);
        assert_eq!(value.color, Some(ColorLabel::Green));
        assert_eq!(value.markers["note"].position_ms, Id(1250));
        assert_eq!(std::fs::read(&source).unwrap(), b"source bytes");
        let lock = fs_util::lock_file(&dir.join("annotations.lock")).unwrap();
        a.edit(&uris, &AnnotationEdit::Rating { rating: 5 })
            .unwrap();
        assert_eq!(a.save(), Err(ErrorCode::Backpressure));
        assert!(a.dirty());
        drop(lock);
        a.save().unwrap();
        assert!(!a.dirty());
        std::fs::write(dir.join("annotations.json"), b"broken").unwrap();
        a.edit(&uris, &AnnotationEdit::Rating { rating: 1 })
            .unwrap();
        assert!(a.save().is_err());
        assert_eq!(
            std::fs::read(dir.join("annotations.json")).unwrap(),
            b"broken"
        );
        for name in ["video.mp4", "annotations.json", "annotations.lock"] {
            std::fs::remove_file(dir.join(name)).unwrap();
        }
        std::fs::remove_dir(dir).unwrap();
    }
}
