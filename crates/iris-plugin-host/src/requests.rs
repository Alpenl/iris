//! Host-issued request identity, pagination and independently cancellable work.
use super::{
    instance::next_id,
    ledger::{Ledger, Reservation, Resource},
};
use iris_api::v2::{ErrorCode, Id, source::SourcePage};
use iris_net::http_client::HttpCancel;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Weak};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowseTarget {
    pub source: String,
    pub folder: Option<String>,
    pub query: Option<String>,
    pub filters: BTreeMap<String, String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestTarget {
    Browse {
        view: Id,
        cursor: Option<String>,
    },
    Resolve {
        source: String,
        entry: String,
        variant: Option<String>,
    },
    Url {
        url: String,
    },
}
pub struct RequestTicket {
    pub id: Id,
    pub owner: String,
    pub generation: Id,
    pub target: RequestTarget,
    pub cancel: HttpCancel,
    _lease: Reservation,
}
struct View {
    owner: String,
    generation: Id,
    target: BrowseTarget,
    pending: Option<Id>,
    next: Option<String>,
    entries: BTreeSet<String>,
    cursors: BTreeSet<String>,
    _lease: Reservation,
}
/// A delivered page retains accounting until its visible row data is dropped.
pub struct AcceptedPage {
    pub view: Id,
    pub append: bool,
    pub page: SourcePage,
    _lease: Reservation,
}
pub struct SourceRequests {
    ledger: Ledger,
    pending: BTreeMap<Id, Arc<RequestTicket>>,
    active: BTreeMap<Id, Weak<RequestTicket>>,
    views: BTreeMap<Id, View>,
}
impl SourceRequests {
    pub fn new(ledger: Ledger) -> Self {
        Self {
            ledger,
            pending: BTreeMap::new(),
            active: BTreeMap::new(),
            views: BTreeMap::new(),
        }
    }
    /// One table belongs to a host window. Source membership is checked by the
    /// manager before this method; plugins never allocate their own request IDs.
    pub fn open_view(
        &mut self,
        owner: &str,
        generation: Id,
        target: BrowseTarget,
    ) -> Result<(Id, Arc<RequestTicket>), ErrorCode> {
        if self.views.len() >= 16 {
            return Err(ErrorCode::Backpressure);
        }
        super::validation::identifier(&target.source)?;
        for value in target.folder.iter().chain(target.query.iter()) {
            super::validation::text(value, 4096)?;
        }
        if target.filters.len() > 16 {
            return Err(ErrorCode::QuotaExceeded);
        }
        for (key, value) in &target.filters {
            super::validation::identifier(key)?;
            super::validation::text(value, 1024)?;
        }
        let lease = self.ledger.reserve(
            owner,
            &[(Resource::Handle, 1), (Resource::HostBytes, 65536)],
        )?;
        let view = Id(next_id()?);
        let request = self.issue(
            owner,
            generation,
            RequestTarget::Browse { view, cursor: None },
        )?;
        self.views.insert(
            view,
            View {
                owner: owner.into(),
                generation,
                target,
                pending: Some(request.id),
                next: None,
                entries: BTreeSet::new(),
                cursors: BTreeSet::new(),
                _lease: lease,
            },
        );
        Ok((view, request))
    }
    pub fn target(&self, owner: &str, view: Id) -> Result<&BrowseTarget, ErrorCode> {
        let view = self.views.get(&view).ok_or(ErrorCode::StaleHandle)?;
        if view.owner != owner {
            return Err(ErrorCode::Denied);
        }
        Ok(&view.target)
    }
    pub fn next_page(
        &mut self,
        owner: &str,
        view: Id,
        cursor: &str,
    ) -> Result<Arc<RequestTicket>, ErrorCode> {
        let state = self.views.get(&view).ok_or(ErrorCode::StaleHandle)?;
        if state.owner != owner {
            return Err(ErrorCode::Denied);
        }
        if state.pending.is_some() {
            return Err(ErrorCode::Backpressure);
        }
        if state.next.as_deref() != Some(cursor) {
            return Err(ErrorCode::StaleHandle);
        }
        let request = self.issue(
            owner,
            state.generation,
            RequestTarget::Browse {
                view,
                cursor: Some(cursor.into()),
            },
        )?;
        self.views.get_mut(&view).unwrap().pending = Some(request.id);
        Ok(request)
    }
    pub fn issue(
        &mut self,
        owner: &str,
        generation: Id,
        target: RequestTarget,
    ) -> Result<Arc<RequestTicket>, ErrorCode> {
        self.active.retain(|_, request| request.strong_count() > 0);
        if self.active.len() >= 128
            || self
                .active
                .values()
                .filter_map(Weak::upgrade)
                .filter(|r| r.owner == owner)
                .count()
                >= 32
        {
            return Err(ErrorCode::Backpressure);
        }
        match &target {
            RequestTarget::Resolve {
                source,
                entry,
                variant,
            } => {
                super::validation::identifier(source)?;
                super::validation::text(entry, 4096)?;
                if let Some(value) = variant {
                    super::validation::text(value, 256)?;
                }
            }
            RequestTarget::Url { url } => {
                super::network::normalize_url(url, super::network::TransportKind::Http)?;
            }
            RequestTarget::Browse { .. } => {}
        }
        let lease = self.ledger.reserve(
            owner,
            &[(Resource::Handle, 1), (Resource::HostBytes, 65536)],
        )?;
        let ticket = Arc::new(RequestTicket {
            id: Id(next_id()?),
            owner: owner.into(),
            generation,
            target,
            cancel: HttpCancel::default(),
            _lease: lease,
        });
        self.pending.insert(ticket.id, ticket.clone());
        self.active.insert(ticket.id, Arc::downgrade(&ticket));
        Ok(ticket)
    }
    fn checked(
        &self,
        owner: &str,
        id: Id,
        generation: Id,
    ) -> Result<&Arc<RequestTicket>, ErrorCode> {
        let request = self.pending.get(&id).ok_or(ErrorCode::StaleHandle)?;
        if request.owner != owner {
            return Err(ErrorCode::Denied);
        }
        if request.generation != generation || request.cancel.is_cancelled() {
            return Err(ErrorCode::StaleHandle);
        }
        Ok(request)
    }
    pub fn accept_page(
        &mut self,
        owner: &str,
        id: Id,
        generation: Id,
        page: SourcePage,
    ) -> Result<AcceptedPage, ErrorCode> {
        super::validation::page(&page)?;
        let request = self.checked(owner, id, generation)?;
        let RequestTarget::Browse { view, cursor } = &request.target else {
            return Err(ErrorCode::BadState);
        };
        let (view, cursor) = (*view, cursor.clone());
        let append = cursor.is_some();
        let state = self.views.get_mut(&view).ok_or(ErrorCode::StaleHandle)?;
        if state.pending != Some(id)
            || state.target.source != page.source_id
            || state.target.folder != page.folder_id
        {
            return Err(ErrorCode::Denied);
        }
        if page
            .next_page
            .as_ref()
            .is_some_and(|s| state.cursors.contains(s) || Some(s) == cursor.as_ref())
        {
            return Err(ErrorCode::InvalidArgument);
        }
        if state.entries.len() + page.entries.len() > 10_000 {
            return Err(ErrorCode::QuotaExceeded);
        }
        if page
            .entries
            .iter()
            .any(|entry| state.entries.contains(&entry.id))
        {
            return Err(ErrorCode::InvalidArgument);
        }
        let estimated = estimate_page_bytes(&page)?;
        let lease = self
            .ledger
            .reserve(owner, &[(Resource::HostBytes, estimated * 32)])?;
        let cost: usize = page
            .entries
            .iter()
            .map(|entry| entry.id.len() + 128)
            .sum::<usize>()
            + page.next_page.as_ref().map_or(0, |s| s.len() + 128);
        state._lease.extend(&[(Resource::HostBytes, cost as u64)])?;
        state
            .entries
            .extend(page.entries.iter().map(|entry| entry.id.clone()));
        if let Some(cursor) = cursor {
            state.cursors.insert(cursor);
        }
        state.next = page.next_page.clone();
        state.pending = None;
        self.pending.remove(&id);
        Ok(AcceptedPage {
            view,
            append,
            page,
            _lease: lease,
        })
    }
    /// The prepared playback job keeps the returned ticket through its
    /// actual terminal state; removing an index does not release a worker's quota.
    pub fn take(
        &mut self,
        owner: &str,
        id: Id,
        generation: Id,
    ) -> Result<Arc<RequestTicket>, ErrorCode> {
        self.checked(owner, id, generation)?;
        if matches!(self.pending[&id].target, RequestTarget::Browse { .. }) {
            return Err(ErrorCode::BadState);
        }
        Ok(self.pending.remove(&id).unwrap())
    }
    pub fn fail(&mut self, owner: &str, id: Id, generation: Id) -> Result<(), ErrorCode> {
        self.checked(owner, id, generation)?;
        self.cancel(id);
        Ok(())
    }
    pub fn cancel_request(&mut self, owner: &str, id: Id) -> Result<(), ErrorCode> {
        let request = self
            .active
            .get(&id)
            .and_then(Weak::upgrade)
            .filter(|r| r.owner == owner)
            .ok_or(ErrorCode::StaleHandle)?;
        request.cancel.cancel();
        self.cancel(id);
        Ok(())
    }
    fn cancel(&mut self, id: Id) {
        if let Some(request) = self.active.get(&id).and_then(Weak::upgrade) {
            request.cancel.cancel();
        }
        if let Some(request) = self.pending.remove(&id) {
            request.cancel.cancel();
            if let RequestTarget::Browse { view, .. } = &request.target
                && let Some(view) = self.views.get_mut(view)
            {
                view.pending = None;
            }
        }
    }
    pub fn close_view(&mut self, owner: &str, view: Id) -> Result<Vec<Id>, ErrorCode> {
        self.target(owner, view)?;
        let ids: Vec<_> = self
            .active
            .iter()
            .filter_map(|(id, r)| r.upgrade().map(|r| (*id, r)))
            .filter_map(|(id, r)| match &r.target {
                RequestTarget::Browse { view: v, .. } if *v == view => Some(id),
                _ => None,
            })
            .collect();
        for id in &ids {
            self.cancel(*id);
        }
        self.views.remove(&view);
        Ok(ids)
    }
    pub fn cancel_resolves(&mut self, owner: &str) -> Vec<Id> {
        let ids: Vec<_> = self
            .active
            .iter()
            .filter_map(|(id, r)| {
                r.upgrade()
                    .filter(|r| {
                        r.owner == owner
                            && matches!(
                                r.target,
                                RequestTarget::Resolve { .. } | RequestTarget::Url { .. }
                            )
                    })
                    .map(|_| *id)
            })
            .collect();
        for id in &ids {
            self.cancel(*id);
        }
        ids
    }
    pub fn cancel_owner(&mut self, owner: &str) -> Vec<Id> {
        let ids: Vec<_> = self
            .active
            .iter()
            .filter_map(|(id, r)| r.upgrade().filter(|r| r.owner == owner).map(|_| *id))
            .collect();
        for id in &ids {
            self.cancel(*id);
        }
        self.views.retain(|_, view| view.owner != owner);
        ids
    }
}

fn estimate_page_bytes(page: &SourcePage) -> Result<u64, ErrorCode> {
    let mut total: usize = 128
        + page.source_id.len()
        + page.title.len()
        + page.folder_id.as_ref().map_or(0, String::len)
        + page.parent_id.as_ref().map_or(0, String::len)
        + page.next_page.as_ref().map_or(0, String::len)
        + page.auth_hint.as_ref().map_or(0, String::len);
    for entry in &page.entries {
        total = total
            .saturating_add(160)
            .saturating_add(entry.id.len())
            .saturating_add(entry.name.len())
            .saturating_add(entry.subtitle.as_ref().map_or(0, String::len));
        for v in &entry.variants {
            total = total
                .saturating_add(32)
                .saturating_add(v.id.len())
                .saturating_add(v.label.len());
        }
        if let Some(thumb) = &entry.thumbnail {
            total = total.saturating_add(64).saturating_add(thumb.url.len());
            for (k, v) in &thumb.headers {
                total = total
                    .saturating_add(8)
                    .saturating_add(k.len())
                    .saturating_add(v.len());
            }
        }
        if total > iris_api::v2::MESSAGE_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
    }
    for filter in &page.filters {
        total = total
            .saturating_add(64)
            .saturating_add(filter.id.len())
            .saturating_add(filter.label.len())
            .saturating_add(filter.selected.as_ref().map_or(0, String::len));
        for opt in &filter.options {
            total = total
                .saturating_add(32)
                .saturating_add(opt.value.len())
                .saturating_add(opt.label.len());
        }
        if total > iris_api::v2::MESSAGE_LIMIT {
            return Err(ErrorCode::QuotaExceeded);
        }
    }
    Ok(total as u64)
}
impl Drop for SourceRequests {
    fn drop(&mut self) {
        for request in self.active.values().filter_map(Weak::upgrade) {
            request.cancel.cancel();
        }
    }
}
impl Drop for RequestTicket {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests;
