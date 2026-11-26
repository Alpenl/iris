//! Personal metadata lives only in the application Store, never in media files.

use std::any::Any;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::bounded_json::{self, BoundedJsonError};
use crate::media_uri::MediaUri;

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
        let mut buf = [0u8; 20];
        let mut n = *value;
        let mut pos = buf.len();
        loop {
            pos -= 1;
            buf[pos] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        let text = std::str::from_utf8(&buf[pos..]).expect("ASCII decimal digits");
        serializer.serialize_str(text)
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

mod background;
pub use background::{AnnotationSnapshot, AnnotationTarget};

type Records = BTreeMap<String, Annotations>;

#[derive(Default)]
pub(super) struct AnnotationStore {
    records: Arc<Records>,
    revision: u64,
    lease: Option<Arc<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for AnnotationStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnnotationStore")
            .field("records", &self.records.len())
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
            ..Default::default()
        }
    }
    pub fn get(&self, uri: &MediaUri) -> Option<&Annotations> {
        self.records.get(&uri.to_uri_string())
    }
}

pub(super) fn read(path: &Path) -> Result<Records, AnnotationError> {
    let file = match crate::fs_util::retry_sharing_violation(|| std::fs::File::open(path)) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(_) => return Err(AnnotationError::Io),
    };
    let mut bytes = Vec::new();
    file.take(BYTE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AnnotationError::Io)?;
    // All annotation structs derive `#[serde(deny_unknown_fields)]` and hold no floats,
    // so 2-pass `bounded_json::decode` enforces size, depth, duplicate keys, and exact schema.
    let value: Records = bounded_json::decode(&bytes, BYTE_LIMIT)?;
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AnnotationEdit, AnnotationError, AnnotationStore, AnnotationTarget, ColorLabel, TimeMarker,
        read,
    };
    use crate::fs_util;
    use crate::media_uri::MediaUri;
    use crate::test_util::TestDir;
    use std::any::Any;
    use std::sync::Arc;

    fn noop_reserve(_: &str, _: u64) -> Result<Arc<dyn Any + Send + Sync>, AnnotationError> {
        Ok(Arc::new(()))
    }

    #[test]
    fn annotations_merge_via_transact_reject_corrupt_and_never_write_media() {
        let dir = TestDir::new("annotations");
        let source = dir.path().join("video.mp4");
        std::fs::write(&source, b"source bytes").unwrap();
        let uri = MediaUri::from(source.clone());
        let uris = [uri.clone()];
        let target = AnnotationTarget::new(dir.path().to_path_buf());

        let snap1 = target
            .transact(
                Some((&uris, &AnnotationEdit::Rating { rating: 4 })),
                "a",
                noop_reserve,
                || Ok(()),
            )
            .unwrap();
        let snap2 = target
            .transact(
                Some((
                    &uris,
                    &AnnotationEdit::UpsertMarker {
                        marker: TimeMarker {
                            id: "note".into(),
                            position_ms: 1250,
                            label: "Review".into(),
                            color: Some(ColorLabel::Blue),
                        },
                    },
                )),
                "b",
                noop_reserve,
                || Ok(()),
            )
            .unwrap();
        let snap3 = target
            .transact(
                Some((
                    &uris,
                    &AnnotationEdit::Color {
                        color: Some(ColorLabel::Green),
                    },
                )),
                "a",
                noop_reserve,
                || Ok(()),
            )
            .unwrap();

        let mut store = AnnotationStore::load(dir.path());
        store.accept(snap3);
        store.accept(snap1);
        store.accept(snap2);
        let value = store.get(&uri).unwrap();
        assert_eq!(value.rating, 4);
        assert_eq!(value.color, Some(ColorLabel::Green));
        assert_eq!(value.markers["note"].position_ms, 1250);
        assert_eq!(std::fs::read(&source).unwrap(), b"source bytes");

        // File lock contention fails with Backpressure.
        let lock = fs_util::lock_file(&dir.path().join("annotations.lock")).unwrap();
        assert!(matches!(
            target.transact(
                Some((&uris, &AnnotationEdit::Rating { rating: 5 })),
                "a",
                noop_reserve,
                || Ok(()),
            ),
            Err(AnnotationError::Backpressure)
        ));
        drop(lock);

        // Corrupt JSON is rejected without replacing on-disk contents.
        let json_path = dir.path().join("annotations.json");
        std::fs::write(&json_path, b"broken").unwrap();
        assert!(
            target
                .transact(
                    Some((&uris, &AnnotationEdit::Rating { rating: 1 })),
                    "a",
                    noop_reserve,
                    || Ok(()),
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&json_path).unwrap(), b"broken");

        // Unknown fields and duplicate keys remain rejected under 2-pass `bounded_json::decode`.
        let key = uri.to_uri_string();
        std::fs::write(
            &json_path,
            serde_json::json!({ &key: { "rating": 3, "color": null, "unknown": 1 } }).to_string(),
        )
        .unwrap();
        assert_eq!(read(&json_path), Err(AnnotationError::InvalidArgument));

        std::fs::write(
            &json_path,
            format!(r#"{{"{key}":{{"rating":1,"rating":2,"color":null}}}}"#),
        )
        .unwrap();
        assert_eq!(read(&json_path), Err(AnnotationError::InvalidArgument));
    }
}
