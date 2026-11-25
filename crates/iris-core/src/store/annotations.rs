//! Personal metadata lives only in the application Store, never in media files.

use std::any::Any;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::bounded_json::{self, BoundedJsonError};
use crate::{fs_util, media_uri::MediaUri};

const BYTE_LIMIT: usize = 16 << 20;
const MEDIA_LIMIT: usize = 4096;
const MARKER_LIMIT: usize = 10_000;

static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

pub(super) fn next_revision() -> Result<u64, AnnotationError> {
    NEXT_REVISION
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map_err(|_| AnnotationError::QuotaExceeded)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnotationError {
    InvalidArgument,
    Denied,
    QuotaExceeded,
    Backpressure,
    RangeInvalid,
    Cancelled,
    Io,
}

impl std::fmt::Display for AnnotationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArgument => f.write_str("invalid annotation argument"),
            Self::Denied => f.write_str("annotation identity denied"),
            Self::QuotaExceeded => f.write_str("annotation quota exceeded"),
            Self::Backpressure => f.write_str("annotation store busy"),
            Self::RangeInvalid => f.write_str("annotation range invalid"),
            Self::Cancelled => f.write_str("annotation operation cancelled"),
            Self::Io => f.write_str("annotation I/O error"),
        }
    }
}

impl std::error::Error for AnnotationError {}

impl From<BoundedJsonError> for AnnotationError {
    fn from(value: BoundedJsonError) -> Self {
        match value {
            BoundedJsonError::QuotaExceeded => Self::QuotaExceeded,
            BoundedJsonError::Invalid => Self::InvalidArgument,
        }
    }
}

/// Serializes `u64` as a canonical decimal JSON string matching on-disk format.
mod decimal_u64 {
    use serde::{Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = u64;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("canonical unsigned decimal string")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<u64, E> {
                if value.is_empty()
                    || !value.bytes().all(|b| b.is_ascii_digit())
                    || (value.len() > 1 && value.starts_with('0'))
                {
                    return Err(E::custom("non-canonical integer string"));
                }
                value
                    .parse::<u64>()
                    .map_err(|_| E::custom("integer out of range"))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorLabel {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Gray,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeMarker {
    pub id: String,
    #[serde(with = "decimal_u64")]
    pub position_ms: u64,
    pub label: String,
    pub color: Option<ColorLabel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnnotationEdit {
    Rating { rating: u8 },
    Color { color: Option<ColorLabel> },
    UpsertMarker { marker: TimeMarker },
    RemoveMarker { id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotationPage {
    pub index: u32,
    pub rating: u8,
    pub color: Option<ColorLabel>,
    pub markers: Vec<TimeMarker>,
    pub next_marker: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Annotations {
    #[serde(default)]
    pub rating: u8,
    pub color: Option<ColorLabel>,
    #[serde(default)]
    pub markers: BTreeMap<String, TimeMarker>,
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

fn validate_label(value: &str) -> Result<(), AnnotationError> {
    if value.len() > 256 {
        return Err(AnnotationError::QuotaExceeded);
    }
    if value
        .chars()
        .any(|c| (c.is_control() && c != '\n' && c != '\t') || is_bidi_control(c))
    {
        return Err(AnnotationError::InvalidArgument);
    }
    Ok(())
}

pub(crate) fn validate_edit(edit: &AnnotationEdit) -> Result<(), AnnotationError> {
    match edit {
        AnnotationEdit::Rating { rating } if *rating > 5 => Err(AnnotationError::InvalidArgument),
        AnnotationEdit::UpsertMarker { marker } => {
            if !is_valid_id(&marker.id) {
                return Err(AnnotationError::InvalidArgument);
            }
            validate_label(&marker.label)
        }
        AnnotationEdit::RemoveMarker { id } => {
            if !is_valid_id(id) {
                return Err(AnnotationError::InvalidArgument);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

impl Annotations {
    pub(super) fn edit(&mut self, edit: &AnnotationEdit) -> Result<(), AnnotationError> {
        validate_edit(edit)?;
        match edit {
            AnnotationEdit::Rating { rating } => self.rating = *rating,
            AnnotationEdit::Color { color } => self.color = *color,
            AnnotationEdit::UpsertMarker { marker } => {
                if self.markers.len() >= 1000 && !self.markers.contains_key(&marker.id) {
                    return Err(AnnotationError::QuotaExceeded);
                }
                self.markers.insert(marker.id.clone(), marker.clone());
            }
            AnnotationEdit::RemoveMarker { id } => {
                self.markers.remove(id);
            }
        }
        Ok(())
    }
    pub(super) fn empty(&self) -> bool {
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
    lease: Option<Arc<dyn Any + Send + Sync>>,
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
    pub fn edit(&mut self, uris: &[MediaUri], edit: &AnnotationEdit) -> Result<(), AnnotationError> {
        if uris.is_empty() || uris.len() > 1000 {
            return Err(AnnotationError::InvalidArgument);
        }
        validate_edit(edit)?;
        let mut next = (*self.records).clone();
        for uri in uris {
            if !uri.may_share_identity() {
                return Err(AnnotationError::Denied);
            }
            let key = uri.to_uri_string();
            if key.len() > 8192 {
                return Err(AnnotationError::QuotaExceeded);
            }
            next.entry(key.clone()).or_default().edit(edit)?;
            if next[&key].empty() {
                next.remove(&key);
            }
        }
        validate(&next)?;
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
            return Err(AnnotationError::QuotaExceeded);
        }
        self.records = Arc::new(next);
        self.edits = pending;
        Ok(())
    }
    pub fn save(&mut self) -> Result<(), AnnotationError> {
        if !self.dirty() {
            return Ok(());
        }
        let directory = self
            .directory
            .clone()
            .or_else(fs_util::data_dir)
            .ok_or(AnnotationError::Io)?;
        std::fs::create_dir_all(&directory).map_err(|_| AnnotationError::Io)?;
        let _lock = fs_util::lock_file(&directory.join("annotations.lock")).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                AnnotationError::Backpressure
            } else {
                AnnotationError::Io
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
        let revision = next_revision()?;
        let bytes = bounded_json::encode(&latest, BYTE_LIMIT)?;
        fs_util::atomic_write_private(&path, &bytes).map_err(|_| AnnotationError::Io)?;
        self.revision = revision;
        self.records = Arc::new(latest);
        self.edits.clear();
        Ok(())
    }
}

pub(super) fn read(path: &Path) -> Result<Records, AnnotationError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(_) => return Err(AnnotationError::Io),
    };
    let mut bytes = Vec::new();
    file.take(BYTE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AnnotationError::Io)?;
    let value: Records = bounded_json::decode_exact(&bytes, BYTE_LIMIT)?;
    validate(&value)?;
    Ok(value)
}

pub(super) fn validate(records: &Records) -> Result<(), AnnotationError> {
    if records.len() > MEDIA_LIMIT
        || records.values().map(|v| v.markers.len()).sum::<usize>() > MARKER_LIMIT
    {
        return Err(AnnotationError::QuotaExceeded);
    }
    for (key, value) in records {
        if key.len() > 8192
            || !MediaUri::parse(key).may_share_identity()
            || value.rating > 5
            || value.markers.len() > 1000
        {
            return Err(AnnotationError::InvalidArgument);
        }
        for (id, marker) in &value.markers {
            if id != &marker.id {
                return Err(AnnotationError::InvalidArgument);
            }
            validate_edit(&AnnotationEdit::UpsertMarker {
                marker: marker.clone(),
            })?;
        }
    }
    bounded_json::encode(records, BYTE_LIMIT).map(|_| ()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;

    #[test]
    fn annotations_merge_fields_across_instances_and_never_write_media() {
        let dir = TestDir::new("annotations");
        let source = dir.path().join("video.mp4");
        std::fs::write(&source, b"source bytes").unwrap();
        let uri = MediaUri::Local(source.clone());
        let uris = [uri.clone()];
        let mut a = AnnotationStore::load(dir.path());
        let mut b = AnnotationStore::load(dir.path());
        a.edit(&uris, &AnnotationEdit::Rating { rating: 4 })
            .unwrap();
        a.save().unwrap();
        b.edit(
            &uris,
            &AnnotationEdit::UpsertMarker {
                marker: TimeMarker {
                    id: "note".into(),
                    position_ms: 1250,
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
        let value = AnnotationStore::load(dir.path());
        let value = value.get(&uri).unwrap();
        assert_eq!(value.rating, 4);
        assert_eq!(value.color, Some(ColorLabel::Green));
        assert_eq!(value.markers["note"].position_ms, 1250);
        assert_eq!(std::fs::read(&source).unwrap(), b"source bytes");
        let lock = fs_util::lock_file(&dir.path().join("annotations.lock")).unwrap();
        a.edit(&uris, &AnnotationEdit::Rating { rating: 5 })
            .unwrap();
        assert_eq!(a.save(), Err(AnnotationError::Backpressure));
        assert!(a.dirty());
        drop(lock);
        a.save().unwrap();
        assert!(!a.dirty());
        std::fs::write(dir.path().join("annotations.json"), b"broken").unwrap();
        a.edit(&uris, &AnnotationEdit::Rating { rating: 1 })
            .unwrap();
        assert!(a.save().is_err());
        assert_eq!(
            std::fs::read(dir.path().join("annotations.json")).unwrap(),
            b"broken"
        );
    }
}
