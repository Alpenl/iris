use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use crate::bounded_json;
use crate::{fs_util, media_uri::MediaUri};

use super::{
    AnnotationEdit, AnnotationError, AnnotationPage, AnnotationStore, Annotations, BYTE_LIMIT,
    Records, next_revision, read, validate,
};

/// Immutable, accounted database view. Replacing it on the UI thread does not
/// clone the database. A late background completion cannot replace a newer one.
pub struct AnnotationSnapshot {
    revision: u64,
    records: Arc<Records>,
    lease: Arc<dyn Any + Send + Sync>,
}

#[derive(Clone)]
pub struct AnnotationTarget {
    directory: PathBuf,
}

impl AnnotationTarget {
    pub(in crate::store) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Background-only. The file lock serializes merge/commit and revision
    /// allocation; cancellation is checked immediately before atomic publication.
    /// After publication a cancellation does not undo the user's stored edit.
    pub fn transact(
        &self,
        mutation: Option<(&[MediaUri], &AnnotationEdit)>,
        owner: &str,
        reserve_bytes: impl Fn(&str, u64) -> Result<Arc<dyn Any + Send + Sync>, AnnotationError>,
        check: impl Fn() -> Result<(), AnnotationError>,
    ) -> Result<AnnotationSnapshot, AnnotationError> {
        let _scratch = reserve_bytes(owner, 64 << 20)?;
        check()?;
        std::fs::create_dir_all(&self.directory).map_err(|_| AnnotationError::Io)?;
        let _lock = fs_util::lock_file(&self.directory.join("annotations.lock")).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                AnnotationError::Backpressure
            } else {
                AnnotationError::Io
            }
        })?;
        let path = self.directory.join("annotations.json");
        let mut records = read(&path)?;
        if let Some((uris, edit)) = mutation {
            if uris.is_empty() || uris.len() > 1000 {
                return Err(AnnotationError::InvalidArgument);
            }
            for uri in uris {
                if !uri.may_share_identity() {
                    return Err(AnnotationError::Denied);
                }
                let key = uri.to_uri_string();
                if key.len() > 8192 {
                    return Err(AnnotationError::QuotaExceeded);
                }
                records.entry(key.clone()).or_default().edit(edit)?;
                if records[&key].empty() {
                    records.remove(&key);
                }
            }
        }
        validate(&records)?;
        // Store data survives uninstall; charge its retained memory to the host,
        // while parsing and mutation remain charged to the requesting owner.
        let retained = 4096
            + records
                .iter()
                .map(|(key, v)| {
                    key.capacity()
                        + 512
                        + v.markers
                            .iter()
                            .map(|(id, m)| {
                                id.capacity() + m.id.capacity() + m.label.capacity() + 256
                            })
                            .sum::<usize>()
                })
                .sum::<usize>();
        let lease = reserve_bytes("host.annotation-store", retained as u64)?;
        let revision = next_revision()?;
        if mutation.is_some() {
            let bytes = bounded_json::encode(&records, BYTE_LIMIT)?;
            check()?;
            fs_util::atomic_write_private(&path, &bytes).map_err(|_| AnnotationError::Io)?;
        } else {
            check()?;
        }
        Ok(AnnotationSnapshot {
            revision,
            records: Arc::new(records),
            lease,
        })
    }
}

impl AnnotationSnapshot {
    pub fn page(
        &self,
        uris: &[MediaUri],
        index: u32,
        offset: u32,
    ) -> Result<AnnotationPage, AnnotationError> {
        let uri = uris
            .get(index as usize)
            .ok_or(AnnotationError::RangeInvalid)?;
        let empty = Annotations::default();
        let value = self.records.get(&uri.to_uri_string()).unwrap_or(&empty);
        if offset as usize > value.markers.len() {
            return Err(AnnotationError::RangeInvalid);
        }
        let markers: Vec<_> = value
            .markers
            .values()
            .skip(offset as usize)
            .take(32)
            .cloned()
            .collect();
        let next = offset + markers.len() as u32;
        Ok(AnnotationPage {
            index,
            rating: value.rating,
            color: value.color,
            markers,
            next_marker: ((next as usize) < value.markers.len()).then_some(next),
        })
    }
}

impl AnnotationStore {
    pub fn accept(&mut self, snapshot: AnnotationSnapshot) -> Result<(), AnnotationError> {
        if snapshot.revision <= self.revision {
            return Ok(());
        }
        if self.dirty() {
            let mut merged = (*snapshot.records).clone();
            for (uri, edits) in &self.edits {
                let value = merged.entry(uri.clone()).or_default();
                for edit in edits.values() {
                    value.edit(edit)?;
                }
                if value.empty() {
                    merged.remove(uri);
                }
            }
            validate(&merged)?;
            self.records = Arc::new(merged);
        } else {
            self.records = snapshot.records;
        }
        self.revision = snapshot.revision;
        self.lease = Some(snapshot.lease);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ColorLabel;
    use crate::test_util::TestDir;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn background_annotations_merge_and_reject_stale_delivery_or_cancelled_publication() {
        struct ByteLease(Arc<AtomicU64>, u64);
        impl Drop for ByteLease {
            fn drop(&mut self) {
                self.0.fetch_sub(self.1, Ordering::Relaxed);
            }
        }

        let dir = TestDir::new("annotation-worker");
        let target = AnnotationTarget::new(dir.path().to_path_buf());
        let used = Arc::new(AtomicU64::new(0));
        let reserve =
            |_: &str, bytes: u64| -> Result<Arc<dyn Any + Send + Sync>, AnnotationError> {
                used.fetch_add(bytes, Ordering::Relaxed);
                Ok(Arc::new(ByteLease(Arc::clone(&used), bytes)))
            };
        let media = dir.path().join("source.mp4");
        std::fs::write(&media, b"untouched source").unwrap();
        let uris = [MediaUri::Local(media.clone())];
        let first = target
            .transact(
                Some((&uris, &AnnotationEdit::Rating { rating: 4 })),
                "one",
                reserve,
                || Ok(()),
            )
            .unwrap();
        let second = target
            .transact(
                Some((
                    &uris,
                    &AnnotationEdit::Color {
                        color: Some(ColorLabel::Green),
                    },
                )),
                "two",
                reserve,
                || Ok(()),
            )
            .unwrap();
        let mut store = AnnotationStore::default();
        store.accept(second).unwrap();
        store.accept(first).unwrap();
        assert_eq!(store.get(&uris[0]).unwrap().rating, 4);
        assert_eq!(store.get(&uris[0]).unwrap().color, Some(ColorLabel::Green));
        let previous = std::fs::read(dir.path().join("annotations.json")).unwrap();
        let checks = Cell::new(0);
        let result = target.transact(
            Some((&uris, &AnnotationEdit::Rating { rating: 1 })),
            "one",
            reserve,
            || {
                checks.set(checks.get() + 1);
                if checks.get() > 1 {
                    Err(AnnotationError::Cancelled)
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(result, Err(AnnotationError::Cancelled)));
        assert_eq!(
            std::fs::read(dir.path().join("annotations.json")).unwrap(),
            previous
        );
        let reloaded = target.transact(None, "one", reserve, || Ok(())).unwrap();
        let page = reloaded.page(&uris, 0, 0).unwrap();
        assert_eq!(page.rating, 4);
        assert!(reloaded.page(&uris, 1, 0).is_err());
        assert!(reloaded.page(&uris, 0, 1).is_err());
        let wire = serde_json::to_string(&page).unwrap();
        assert!(!wire.contains("source.mp4") && !wire.contains("path"));
        assert_eq!(std::fs::read(media).unwrap(), b"untouched source");
        drop((store, reloaded));
        assert_eq!(used.load(Ordering::Relaxed), 0);
    }
}
