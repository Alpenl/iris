//! The host grants annotations only over an explicit current/selected media set.
use super::{
    instance::next_id,
    ledger::{Ledger, Reservation, Resource},
};
use crate::{media_uri::MediaUri, store::Store};
use iris_api::v2::{Capability, ErrorCode, Id, enhance::AnnotationEdit};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};
struct Selection {
    instance: Id,
    generation: Id,
    uris: Vec<MediaUri>,
    expires: Instant,
    cancel: crate::vfs::HttpCancel,
    _lease: Reservation,
}
pub struct SelectionGrants {
    owner: String,
    allowed: bool,
    ledger: Ledger,
    values: BTreeMap<Id, Arc<Selection>>,
}
#[derive(Clone)]
pub struct SelectionPermit(Arc<Selection>);
impl SelectionPermit {
    pub fn check(&self) -> Result<(), ErrorCode> {
        if self.0.cancel.is_cancelled() || self.0.expires <= Instant::now() {
            Err(ErrorCode::StaleHandle)
        } else {
            Ok(())
        }
    }
    pub fn uris(&self) -> Result<&[MediaUri], ErrorCode> {
        self.check()?;
        Ok(&self.0.uris)
    }
}
impl SelectionGrants {
    pub fn new(owner: String, capabilities: &BTreeSet<Capability>, ledger: Ledger) -> Self {
        Self {
            owner,
            allowed: capabilities.contains(&Capability::Annotations),
            ledger,
            values: BTreeMap::new(),
        }
    }
    /// Only host selection actions call this. Guest fields cannot choose URIs.
    pub fn approve(
        &mut self,
        instance: Id,
        generation: Id,
        uris: Vec<MediaUri>,
    ) -> Result<Id, ErrorCode> {
        if !self.allowed {
            return Err(ErrorCode::Denied);
        }
        self.values
            .retain(|_, value| value.expires > Instant::now());
        if self.values.len() >= 8 || uris.is_empty() || uris.len() > 1000 {
            return Err(ErrorCode::QuotaExceeded);
        }
        let mut keys = BTreeSet::new();
        let mut bytes = 0;
        for uri in &uris {
            let key = uri.to_uri_string();
            if !uri.may_share_identity() || key.len() > 8192 || !keys.insert(key.clone()) {
                return Err(ErrorCode::InvalidArgument);
            }
            bytes += key.len() + 512;
        }
        if bytes > 1 << 20 {
            return Err(ErrorCode::QuotaExceeded);
        }
        let lease = self.ledger.reserve(
            &self.owner,
            &[(Resource::Handle, 1), (Resource::HostBytes, bytes as u64)],
        )?;
        let id = Id(next_id()?);
        self.values.insert(
            id,
            Arc::new(Selection {
                instance,
                generation,
                uris,
                expires: Instant::now() + Duration::from_secs(300),
                cancel: crate::vfs::HttpCancel::default(),
                _lease: lease,
            }),
        );
        Ok(id)
    }
    fn get(
        &self,
        owner: &str,
        instance: Id,
        generation: Id,
        id: Id,
    ) -> Result<&Selection, ErrorCode> {
        if owner != self.owner {
            return Err(ErrorCode::Denied);
        }
        let value = self.values.get(&id).ok_or(ErrorCode::StaleHandle)?;
        if value.instance != instance {
            return Err(ErrorCode::Denied);
        }
        if value.generation != generation || value.expires <= Instant::now() {
            return Err(ErrorCode::StaleHandle);
        }
        Ok(value)
    }
    pub fn permit(
        &self,
        owner: &str,
        instance: Id,
        generation: Id,
        id: Id,
    ) -> Result<SelectionPermit, ErrorCode> {
        self.get(owner, instance, generation, id)?;
        Ok(SelectionPermit(self.values[&id].clone()))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn edit(
        &self,
        owner: &str,
        instance: Id,
        generation: Id,
        id: Id,
        edit: &AnnotationEdit,
        store: &mut Store,
    ) -> Result<(), ErrorCode> {
        let selected = self.get(owner, instance, generation, id)?;
        // Store operations are bounded but may clone persisted state. The host
        // must dispatch this work away from Store callbacks and UI render paths.
        let _scratch = self
            .ledger
            .reserve(owner, &[(Resource::HostBytes, 64 << 20)])?;
        store.edit_annotations(&selected.uris, edit)
    }
    pub fn snapshot(
        &self,
        owner: &str,
        instance: Id,
        generation: Id,
        id: Id,
        store: &Store,
    ) -> Result<std::sync::Arc<super::instance::Blob>, ErrorCode> {
        let selected = self.get(owner, instance, generation, id)?;
        // Serialize borrowed records under a bounded scratch lease. Returned
        // bytes retain their own reservation, including while delivery is queued.
        let _scratch = self
            .ledger
            .reserve(owner, &[(Resource::HostBytes, 4 << 20)])?;
        let empty = crate::store::Annotations::default();
        let values: Vec<_> = selected
            .uris
            .iter()
            .map(|uri| store.annotations(uri).unwrap_or(&empty))
            .collect();
        let bytes = iris_plugin_sdk::json::encode(&values, iris_api::v2::CHUNK_LIMIT)?;
        Ok(std::sync::Arc::new(super::instance::Blob::new(
            &self.ledger,
            owner,
            bytes,
        )?))
    }
    pub fn revoke(&mut self) {
        for value in self.values.values() {
            value.cancel.cancel();
        }
        self.values.clear();
    }
}
impl Drop for SelectionGrants {
    fn drop(&mut self) {
        self.revoke();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn annotation_scope_cannot_be_reused_by_another_installation_instance_or_media_generation() {
        let ledger = Ledger::default();
        let mut scope = SelectionGrants::new(
            "a".into(),
            &BTreeSet::from([Capability::Annotations]),
            ledger.clone(),
        );
        let uri = MediaUri::Plugin {
            plugin_id: "source".into(),
            source_id: "nas".into(),
            entry_id: "one".into(),
        };
        let id = scope.approve(Id(3), Id(1), vec![uri.clone()]).unwrap();
        let mut store = Store::default();
        let edit = AnnotationEdit::Rating { rating: 4 };
        assert_eq!(
            scope.edit("b", Id(3), Id(1), id, &edit, &mut store),
            Err(ErrorCode::Denied)
        );
        assert_eq!(
            scope.edit("a", Id(4), Id(1), id, &edit, &mut store),
            Err(ErrorCode::Denied)
        );
        assert_eq!(
            scope.edit("a", Id(3), Id(2), id, &edit, &mut store),
            Err(ErrorCode::StaleHandle)
        );
        assert!(store.annotations(&uri).is_none());
        scope
            .edit("a", Id(3), Id(1), id, &edit, &mut store)
            .unwrap();
        assert_eq!(store.annotations(&uri).unwrap().rating, 4);
        let snapshot = scope.snapshot("a", Id(3), Id(1), id, &store).unwrap();
        let values: Vec<crate::store::Annotations> =
            serde_json::from_slice(&snapshot.bytes).unwrap();
        assert_eq!(values[0].rating, 4);
        let permit = scope.permit("a", Id(3), Id(1), id).unwrap();
        scope.revoke();
        assert_eq!(permit.check(), Err(ErrorCode::StaleHandle));
        assert_eq!(ledger.used(None, Resource::Handle), 1);
        drop(permit);
        assert!(
            scope
                .edit("a", Id(3), Id(1), id, &edit, &mut store)
                .is_err()
        );
        assert_eq!(ledger.used(None, Resource::Handle), 0);
        assert_eq!(
            ledger.used(None, Resource::HostBytes),
            snapshot.bytes.len() as u64
        );
        drop(snapshot);
        assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    }
}
