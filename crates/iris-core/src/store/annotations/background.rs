use super::*;
use crate::plugin_v2::{
    instance::next_id,
    ledger::{Ledger, Reservation, Resource},
};
use iris_api::v2::enhance::AnnotationPage;

/// Immutable, accounted database view. Replacing it on the UI thread does not
/// clone the database. A late background completion cannot replace a newer one.
pub struct AnnotationSnapshot {
    revision: u64,
    records: Arc<Records>,
    lease: Arc<Reservation>,
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
        ledger: &Ledger,
        owner: &str,
        check: impl Fn() -> Result<(), ErrorCode>,
    ) -> Result<AnnotationSnapshot, ErrorCode> {
        let _scratch = ledger.reserve(owner, &[(Resource::HostBytes, 64 << 20)])?;
        check()?;
        std::fs::create_dir_all(&self.directory).map_err(|_| ErrorCode::IoError)?;
        let _lock = fs_util::lock_file(&self.directory.join("annotations.lock")).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                ErrorCode::Backpressure
            } else {
                ErrorCode::IoError
            }
        })?;
        let path = self.directory.join("annotations.json");
        let mut records = read(&path)?;
        if let Some((uris, edit)) = mutation {
            if uris.is_empty() || uris.len() > 1000 {
                return Err(ErrorCode::InvalidArgument);
            }
            for uri in uris {
                if !uri.may_share_identity() {
                    return Err(ErrorCode::Denied);
                }
                let key = uri.to_uri_string();
                if key.len() > 8192 {
                    return Err(ErrorCode::QuotaExceeded);
                }
                records.entry(key.clone()).or_default().edit(edit)?;
                if records[&key].empty() {
                    records.remove(&key);
                }
            }
        }
        validate(&records)?;
        // Store data survives uninstall; charge its retained memory to the host,
        // while parsing and mutation remain charged to the requesting plugin.
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
        let lease = Arc::new(ledger.reserve(
            "host.annotation-store",
            &[(Resource::HostBytes, retained as u64)],
        )?);
        let revision = next_id()?;
        if mutation.is_some() {
            let bytes = iris_plugin_sdk::json::encode(&records, BYTE_LIMIT)?;
            check()?;
            fs_util::atomic_write_private(&path, &bytes).map_err(|_| ErrorCode::IoError)?;
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
    ) -> Result<AnnotationPage, ErrorCode> {
        let uri = uris.get(index as usize).ok_or(ErrorCode::RangeInvalid)?;
        let empty = Annotations::default();
        let value = self.records.get(&uri.to_uri_string()).unwrap_or(&empty);
        if offset as usize > value.markers.len() {
            return Err(ErrorCode::RangeInvalid);
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
    pub fn accept(&mut self, snapshot: AnnotationSnapshot) -> Result<(), ErrorCode> {
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
    use std::cell::Cell;
    #[test]
    fn background_annotations_merge_and_reject_stale_delivery_or_cancelled_publication() {
        let directory =
            std::env::temp_dir().join(format!("iris-annotation-worker-{}", next_id().unwrap()));
        let target = AnnotationTarget::new(directory.clone());
        let ledger = Ledger::default();
        let media = directory.join("source.mp4");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(&media, b"untouched source").unwrap();
        let uris = [MediaUri::Local(media.clone())];
        let first = target
            .transact(
                Some((&uris, &AnnotationEdit::Rating { rating: 4 })),
                &ledger,
                "one",
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
                &ledger,
                "two",
                || Ok(()),
            )
            .unwrap();
        let mut store = AnnotationStore::default();
        store.accept(second).unwrap();
        store.accept(first).unwrap();
        assert_eq!(store.get(&uris[0]).unwrap().rating, 4);
        assert_eq!(store.get(&uris[0]).unwrap().color, Some(ColorLabel::Green));
        let previous = std::fs::read(directory.join("annotations.json")).unwrap();
        let checks = Cell::new(0);
        let result = target.transact(
            Some((&uris, &AnnotationEdit::Rating { rating: 1 })),
            &ledger,
            "one",
            || {
                checks.set(checks.get() + 1);
                if checks.get() > 1 {
                    Err(ErrorCode::Cancelled)
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(result, Err(ErrorCode::Cancelled)));
        assert_eq!(
            std::fs::read(directory.join("annotations.json")).unwrap(),
            previous
        );
        let reloaded = target.transact(None, &ledger, "one", || Ok(())).unwrap();
        let page = reloaded.page(&uris, 0, 0).unwrap();
        assert_eq!(page.rating, 4);
        assert!(reloaded.page(&uris, 1, 0).is_err());
        assert!(reloaded.page(&uris, 0, 1).is_err());
        let wire = serde_json::to_string(&page).unwrap();
        assert!(!wire.contains("source.mp4") && !wire.contains("path"));
        assert_eq!(std::fs::read(media).unwrap(), b"untouched source");
        drop((store, reloaded));
        assert_eq!(ledger.used(None, Resource::HostBytes), 0);
        assert!(directory.starts_with(std::env::temp_dir()));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
