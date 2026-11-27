//! The host grants annotations only over an explicit current/selected media set.
use super::{
    instance::next_id,
    ledger::{Ledger, Reservation, Resource},
};
use iris_api::v2::{
    Capability, ErrorCode, Id, WireU64,
    enhance::{AnnotationEdit, AnnotationPage, ColorLabel, TimeMarker},
};
use iris_core::media_uri::MediaUri;
use iris_core::store::{self, AnnotationError};

pub fn annotation_error_code(error: AnnotationError) -> ErrorCode {
    match error {
        AnnotationError::InvalidArgument => ErrorCode::InvalidArgument,
        AnnotationError::Denied => ErrorCode::Denied,
        AnnotationError::QuotaExceeded => ErrorCode::QuotaExceeded,
        AnnotationError::Backpressure => ErrorCode::Backpressure,
        AnnotationError::RangeInvalid => ErrorCode::RangeInvalid,
        AnnotationError::Cancelled => ErrorCode::Cancelled,
        AnnotationError::Io => ErrorCode::IoError,
    }
}

pub fn error_code_to_annotation(code: ErrorCode) -> AnnotationError {
    match code {
        ErrorCode::InvalidArgument => AnnotationError::InvalidArgument,
        ErrorCode::Denied => AnnotationError::Denied,
        ErrorCode::QuotaExceeded => AnnotationError::QuotaExceeded,
        ErrorCode::Backpressure => AnnotationError::Backpressure,
        ErrorCode::RangeInvalid => AnnotationError::RangeInvalid,
        ErrorCode::Cancelled => AnnotationError::Cancelled,
        _ => AnnotationError::Io,
    }
}

fn color_to_store(c: ColorLabel) -> store::ColorLabel {
    match c {
        ColorLabel::Red => store::ColorLabel::Red,
        ColorLabel::Orange => store::ColorLabel::Orange,
        ColorLabel::Yellow => store::ColorLabel::Yellow,
        ColorLabel::Green => store::ColorLabel::Green,
        ColorLabel::Blue => store::ColorLabel::Blue,
        ColorLabel::Purple => store::ColorLabel::Purple,
        ColorLabel::Gray => store::ColorLabel::Gray,
    }
}

fn color_to_api(c: store::ColorLabel) -> ColorLabel {
    match c {
        store::ColorLabel::Red => ColorLabel::Red,
        store::ColorLabel::Orange => ColorLabel::Orange,
        store::ColorLabel::Yellow => ColorLabel::Yellow,
        store::ColorLabel::Green => ColorLabel::Green,
        store::ColorLabel::Blue => ColorLabel::Blue,
        store::ColorLabel::Purple => ColorLabel::Purple,
        store::ColorLabel::Gray => ColorLabel::Gray,
    }
}

pub fn edit_to_store(edit: &AnnotationEdit) -> store::AnnotationEdit {
    match edit {
        AnnotationEdit::Rating { rating } => store::AnnotationEdit::Rating { rating: *rating },
        AnnotationEdit::Color { color } => store::AnnotationEdit::Color {
            color: color.map(color_to_store),
        },
        AnnotationEdit::UpsertMarker { marker } => store::AnnotationEdit::UpsertMarker {
            marker: store::TimeMarker {
                id: marker.id.clone(),
                position_ms: marker.position_ms.0,
                label: marker.label.clone(),
                color: marker.color.map(color_to_store),
            },
        },
        AnnotationEdit::RemoveMarker { id } => {
            store::AnnotationEdit::RemoveMarker { id: id.clone() }
        }
    }
}

pub fn page_to_api(page: store::AnnotationPage) -> AnnotationPage {
    AnnotationPage {
        index: page.index,
        rating: page.rating,
        color: page.color.map(color_to_api),
        markers: page
            .markers
            .into_iter()
            .map(|m| TimeMarker {
                id: m.id,
                position_ms: WireU64(m.position_ms),
                label: m.label,
                color: m.color.map(color_to_api),
            })
            .collect(),
        next_marker: page.next_marker,
    }
}
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
    cancel: iris_base::net::http_client::HttpCancel,
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
                cancel: iris_base::net::http_client::HttpCancel::default(),
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
        assert!(matches!(
            scope.permit("b", Id(3), Id(1), id),
            Err(ErrorCode::Denied)
        ));
        assert!(matches!(
            scope.permit("a", Id(4), Id(1), id),
            Err(ErrorCode::Denied)
        ));
        assert!(matches!(
            scope.permit("a", Id(3), Id(2), id),
            Err(ErrorCode::StaleHandle)
        ));
        let permit = scope.permit("a", Id(3), Id(1), id).unwrap();
        assert_eq!(permit.uris().unwrap(), std::slice::from_ref(&uri));
        assert_eq!(
            edit_to_store(&AnnotationEdit::Rating { rating: 4 }),
            store::AnnotationEdit::Rating { rating: 4 }
        );
        scope.revoke();
        assert_eq!(permit.check(), Err(ErrorCode::StaleHandle));
        assert!(permit.uris().is_err());
        assert_eq!(ledger.used(None, Resource::Handle), 1);
        drop(permit);
        assert!(scope.permit("a", Id(3), Id(1), id).is_err());
        assert_eq!(ledger.used(None, Resource::Handle), 0);
        assert_eq!(ledger.used(None, Resource::HostBytes), 0);
    }
}
