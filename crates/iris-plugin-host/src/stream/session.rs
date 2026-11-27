//! Atomic manifest generations and reusable, unguessable session capabilities.
use super::{
    guest::StreamStore,
    manifest::{self, Profile},
    reader::{MemoryReader, Reader},
    remote::RemoteFactory,
};
use crate::{
    instance::{Blob, next_id},
    ledger::{Activity, ActivityLease, Reservation, Resource},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iris_api::v2::{ErrorCode, Generation, Id, stream::*};
use iris_base::net::http_client::HttpCancel;
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, PartialEq, Eq)]
enum Definition {
    Bound(ResourceBinding),
    Document(Profile),
    Direct,
    Attachment,
}
struct Entry {
    token: String,
    extension: String,
    definition: Definition,
    reader: Arc<dyn Reader>,
    probed: AtomicBool,
    cancel: HttpCancel,
    _memory: Reservation,
}
struct Snapshot {
    revision: Id,
    profile: Option<Profile>,
    resources: BTreeMap<String, Arc<Entry>>,
    tokens: BTreeMap<String, Arc<Entry>>,
}
pub(crate) struct Prepared {
    stamp: Id,
    expected: Id,
    snapshot: Arc<Snapshot>,
}
pub(crate) struct Access {
    pub reader: Arc<dyn Reader>,
    pub cancel: HttpCancel,
    pub profile: Option<Profile>,
    entry: Arc<Entry>,
    _activity: ActivityLease,
}
impl Access {
    pub(super) fn is_probed(&self) -> bool {
        self.entry.probed.load(Ordering::Acquire)
    }
    pub(super) fn mark_probed(&self) {
        self.entry.probed.store(true, Ordering::Release);
    }
}
impl Drop for Access {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(crate) struct StreamSession {
    pub id: Id,
    generation: Mutex<(Generation, HttpCancel)>,
    pub factory: RemoteFactory,
    authority: SocketAddr,
    state: Mutex<Arc<Snapshot>>,
    store: Option<Arc<StreamStore>>,
    cancel: HttpCancel,
    stamp: Id,
    activity: Activity,
    guest_manifest_updates: AtomicBool,
    refresh: Mutex<Option<Arc<dyn Refresh>>>,
    _reservation: Reservation,
}
impl StreamSession {
    pub(crate) fn new(
        id: Id,
        generation: Generation,
        authority: SocketAddr,
        factory: RemoteFactory,
        store: Option<Arc<StreamStore>>,
    ) -> Result<Arc<Self>, ErrorCode> {
        if !authority.ip().is_loopback() || authority.port() == 0 || id.0 == 0 {
            return Err(ErrorCode::InvalidArgument);
        }
        let reservation = factory.ledger().reserve(
            factory.owner(),
            &[
                (Resource::PlaybackSession, 1),
                (Resource::HostBytes, 64 << 10),
            ],
        )?;
        Ok(Arc::new(Self {
            id,
            generation: Mutex::new((generation, HttpCancel::default())),
            factory,
            authority,
            state: Mutex::new(Arc::new(Snapshot {
                revision: Id(0),
                profile: None,
                resources: BTreeMap::new(),
                tokens: BTreeMap::new(),
            })),
            store,
            cancel: HttpCancel::default(),
            stamp: Id(next_id()?),
            activity: Activity::default(),
            guest_manifest_updates: AtomicBool::new(false),
            refresh: Mutex::new(None),
            _reservation: reservation,
        }))
    }
    pub(crate) fn enable_guest_manifest_updates(&self) {
        self.guest_manifest_updates.store(true, Ordering::Release);
    }
    pub(crate) fn allows_guest_manifest_updates(&self) -> bool {
        self.guest_manifest_updates.load(Ordering::Acquire)
            && self
                .refresh
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
    }
    pub(crate) fn cancel(&self) {
        self.cancel.cancel();
        if let Some(store) = &self.store {
            store.stop();
        }
        // Free resident resources now; outstanding Access objects continue to
        // own their reservations until the consumers actually return.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *state = Arc::new(Snapshot {
            revision: state.revision,
            profile: state.profile,
            resources: BTreeMap::new(),
            tokens: BTreeMap::new(),
        });
    }
    pub(crate) fn cancellation(&self) -> &HttpCancel {
        &self.cancel
    }
    pub(crate) fn authority(&self) -> SocketAddr {
        self.authority
    }
    pub(crate) fn revision(&self) -> Id {
        self.snapshot().revision
    }
    pub(crate) fn root_url(&self) -> Result<String, ErrorCode> {
        let state = self.snapshot();
        let root = state
            .resources
            .get(manifest::ROOT)
            .ok_or(ErrorCode::BadState)?;
        Ok(self.url(&root.token, &root.extension))
    }
    pub(crate) fn profile(&self) -> Option<Profile> {
        self.snapshot().profile
    }
    fn snapshot(&self) -> Arc<Snapshot> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn url(&self, token: &str, ext: &str) -> String {
        format!("http://{}/v2/{}/{token}.{ext}", self.authority, self.id.0)
    }
    pub(crate) fn open(
        &self,
        path: &str,
        request_cancel: &HttpCancel,
    ) -> Result<Access, ErrorCode> {
        if path.len() > 256 || self.cancel.is_cancelled() || request_cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let activity = self.activity.start();
        let state = self.snapshot();
        let key = path
            .strip_prefix(&format!("/v2/{}/", self.id.0))
            .ok_or(ErrorCode::Denied)?;
        if state
            .tokens
            .get(key)
            .is_some_and(|entry| matches!(entry.definition, Definition::Document(_)))
        {
            let refresh = self
                .refresh
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(refresh) = refresh {
                let cancel = self.cancel.child().map_err(|_| ErrorCode::Cancelled)?;
                request_cancel
                    .link(&cancel)
                    .map_err(|_| ErrorCode::Cancelled)?;
                refresh.refresh(&cancel)?;
                if request_cancel.is_cancelled() {
                    return Err(ErrorCode::Cancelled);
                }
            }
        }
        let state = self.snapshot();
        let entry = state
            .tokens
            .get(key)
            .cloned()
            .ok_or(ErrorCode::StaleHandle)?;
        let cancel = entry.cancel.child().map_err(|_| ErrorCode::Cancelled)?;
        if self.store.is_some() {
            self.generation
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .1
                .link(&cancel)
                .map_err(|_| ErrorCode::Cancelled)?;
        }
        request_cancel
            .link(&cancel)
            .map_err(|_| ErrorCode::Cancelled)?;
        let profile = match entry.definition {
            Definition::Document(profile) => Some(profile),
            _ => None,
        };
        Ok(Access {
            reader: entry.reader.clone(),
            cancel,
            profile,
            entry,
            _activity: activity,
        })
    }
    pub(crate) fn prepare_manifest(
        &self,
        expected: Id,
        format: ManifestFormat,
        bytes: &[u8],
    ) -> Result<Prepared, ErrorCode> {
        if self.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let previous = self.snapshot();
        if previous.revision != expected {
            return Err(ErrorCode::StaleHandle);
        }
        let _scratch = self
            .factory
            .ledger()
            .reserve(self.factory.owner(), &[(Resource::HostBytes, 32 << 20)])?;
        let tokens = RefCell::new(BTreeMap::<String, (String, String)>::new());
        let local = |id: &str, ext: ResourceExtension| {
            let ext = extension(ext).to_owned();
            let mut tokens = tokens.borrow_mut();
            let value = tokens.entry(id.into()).or_insert_with(|| {
                previous
                    .resources
                    .get(id)
                    .filter(|e| e.extension == ext)
                    .map(|e| (e.token.clone(), ext.clone()))
                    .unwrap_or_default()
            });
            if value.0.is_empty() {
                *value = (token()?, ext);
            }
            Ok(self.url(&value.0, &value.1))
        };
        let mut compiled = manifest::compile(format, bytes, &local)?;
        let mut changed = false;
        for (id, binding) in &compiled.resources {
            if let Some(old) = previous.resources.get(id)
                && old.definition != Definition::Bound(binding.clone())
            {
                let ext = extension(binding.extension).to_owned();
                tokens.borrow_mut().insert(id.clone(), (token()?, ext));
                changed = true;
            }
        }
        if changed {
            compiled = manifest::compile(format, bytes, &local)?;
        }
        let mut resources: BTreeMap<_, _> = previous
            .resources
            .iter()
            .filter(|(_, e)| e.definition == Definition::Attachment)
            .map(|(id, e)| (id.clone(), e.clone()))
            .collect();
        for (id, binding) in compiled.resources {
            let definition = Definition::Bound(binding.clone());
            let entry = if let Some(old) = previous
                .resources
                .get(&id)
                .filter(|e| e.definition == definition)
            {
                old.clone()
            } else {
                let size = if binding.kind == ResourceKind::Key {
                    Some(16)
                } else {
                    binding.size.map(|n| n.0)
                };
                let reader = match &binding.source {
                    ResourceSource::Remote { request } => {
                        self.factory.reader(request, size)? as Arc<dyn Reader>
                    }
                    ResourceSource::Guest { resource_id } => self
                        .store
                        .as_ref()
                        .ok_or(ErrorCode::Denied)?
                        .reader(resource_id.clone(), size)?,
                };
                let (token, extension) = tokens.borrow()[&id].clone();
                self.entry(token, extension, definition, reader, None)?
            };
            resources.insert(id, entry);
        }
        for doc in compiled.documents {
            let (ext, kind) = match doc.profile {
                Profile::Hls => ("m3u8", ResourceExtension::M3u8),
            };
            local(&doc.id, kind)?;
            let (token, extension) = tokens
                .borrow_mut()
                .entry(doc.id.clone())
                .or_insert((token()?, ext.into()))
                .clone();
            let reader = Arc::new(MemoryReader::new(
                doc.bytes,
                self.factory.owner(),
                self.factory.ledger(),
            )?);
            let old = previous.resources.get(&doc.id).filter(|e| e.token == token);
            resources.insert(
                doc.id,
                self.entry(
                    token,
                    extension,
                    Definition::Document(doc.profile),
                    reader,
                    old.map(|e| e.cancel.clone()),
                )?,
            );
        }
        self.prepared(expected, Some(compiled.profile), resources)
    }
    /// Caller has already selected safe demuxer behavior or converted a detected
    /// remote manifest. Opaque sources retain access-references=no in mpv.
    pub(crate) fn prepare_reader(&self, reader: Arc<dyn Reader>) -> Result<Prepared, ErrorCode> {
        if self.revision() != Id(0) {
            return Err(ErrorCode::BadState);
        }
        let entry = self.entry(token()?, "bin".into(), Definition::Direct, reader, None)?;
        self.prepared(
            Id(0),
            None,
            BTreeMap::from([(manifest::ROOT.into(), entry)]),
        )
    }
    pub(crate) fn add_attachment(
        &self,
        id: &str,
        extension: &str,
        reader: Arc<dyn Reader>,
    ) -> Result<String, ErrorCode> {
        manifest::logical(id)?;
        if !matches!(extension, "bin" | "srt" | "vtt" | "ass") {
            return Err(ErrorCode::InvalidArgument);
        }
        let previous = self.snapshot();
        let mut resources = previous.resources.clone();
        let entry = self.entry(
            token()?,
            extension.into(),
            Definition::Attachment,
            reader,
            None,
        )?;
        let url = self.url(&entry.token, &entry.extension);
        resources.insert(format!("__iris_attachment_{id}"), entry);
        self.replace_attachments(resources)?;
        Ok(url)
    }
    pub(crate) fn set_refresh(&self, refresh: Arc<dyn Refresh>) -> Result<(), ErrorCode> {
        if self.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        *self.refresh.lock().unwrap_or_else(|e| e.into_inner()) = Some(refresh);
        Ok(())
    }
    pub(crate) fn remove_attachment(&self, id: &str) -> Result<(), ErrorCode> {
        let previous = self.snapshot();
        let mut resources = previous.resources.clone();
        if resources
            .remove(&format!("__iris_attachment_{id}"))
            .is_none()
        {
            return Ok(());
        }
        self.replace_attachments(resources)
    }
    fn replace_attachments(
        &self,
        mut resources: BTreeMap<String, Arc<Entry>>,
    ) -> Result<(), ErrorCode> {
        let old = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if self.cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            // Keep concurrent manifest updates while updating only attachment entries.
            resources.retain(|k, _| k.starts_with("__iris_attachment_"));
            for (k, v) in &state.resources {
                if !k.starts_with("__iris_attachment_") {
                    resources.insert(k.clone(), v.clone());
                }
            }
            if resources.len() > manifest::RESOURCE_LIMIT || !resources.contains_key(manifest::ROOT)
            {
                return Err(ErrorCode::QuotaExceeded);
            }
            let tokens: BTreeMap<_, _> = resources
                .values()
                .map(|e| (format!("{}.{}", e.token, e.extension), e.clone()))
                .collect();
            if tokens.len() != resources.len() {
                return Err(ErrorCode::Internal);
            }
            let next = Arc::new(Snapshot {
                revision: state.revision,
                profile: state.profile,
                resources,
                tokens,
            });
            std::mem::replace(&mut *state, next.clone())
        };
        let current = self.snapshot();
        for (key, entry) in &old.tokens {
            if !current.tokens.contains_key(key) {
                entry.cancel.cancel();
            }
        }
        Ok(())
    }
    fn entry(
        &self,
        token: String,
        extension: String,
        definition: Definition,
        reader: Arc<dyn Reader>,
        cancel: Option<HttpCancel>,
    ) -> Result<Arc<Entry>, ErrorCode> {
        let retained = match &definition {
            Definition::Bound(ResourceBinding {
                source: ResourceSource::Remote { request },
                ..
            }) => {
                request.url.len()
                    + request
                        .headers
                        .iter()
                        .map(|(k, v)| k.len() + v.len())
                        .sum::<usize>()
            }
            _ => 0,
        };
        let memory = self.factory.ledger().reserve(
            self.factory.owner(),
            &[
                (Resource::StreamEntry, 1),
                (Resource::HostBytes, 2048 + retained as u64),
            ],
        )?;
        let cancel =
            cancel.map_or_else(|| self.cancel.child().map_err(|_| ErrorCode::Cancelled), Ok)?;
        let skip_probe = matches!(
            definition,
            Definition::Document(_)
                | Definition::Direct
                | Definition::Bound(ResourceBinding {
                    kind: ResourceKind::Key,
                    ..
                })
        );
        Ok(Arc::new(Entry {
            token,
            extension,
            definition,
            reader,
            probed: AtomicBool::new(skip_probe),
            cancel,
            _memory: memory,
        }))
    }
    fn prepared(
        &self,
        expected: Id,
        profile: Option<Profile>,
        resources: BTreeMap<String, Arc<Entry>>,
    ) -> Result<Prepared, ErrorCode> {
        if resources.len() > manifest::RESOURCE_LIMIT || !resources.contains_key(manifest::ROOT) {
            return Err(ErrorCode::QuotaExceeded);
        }
        let revision = Id(expected.0.checked_add(1).ok_or(ErrorCode::QuotaExceeded)?);
        let tokens: BTreeMap<_, _> = resources
            .values()
            .map(|e| (format!("{}.{}", e.token, e.extension), e.clone()))
            .collect();
        if tokens.len() != resources.len() {
            return Err(ErrorCode::Internal);
        }
        Ok(Prepared {
            stamp: self.stamp,
            expected,
            snapshot: Arc::new(Snapshot {
                revision,
                profile,
                resources,
                tokens,
            }),
        })
    }
    pub(crate) fn commit(&self, prepared: Prepared) -> Result<Id, ErrorCode> {
        if prepared.stamp != self.stamp {
            return Err(ErrorCode::Denied);
        }
        let old = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if self.cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            if state.revision != prepared.expected
                || state.profile.is_some() && state.profile != prepared.snapshot.profile
            {
                return Err(ErrorCode::StaleHandle);
            }
            std::mem::replace(&mut *state, prepared.snapshot.clone())
        };
        for (key, old) in &old.tokens {
            if !prepared.snapshot.tokens.contains_key(key) {
                old.cancel.cancel();
            }
        }
        Ok(prepared.snapshot.revision)
    }
    pub(crate) fn refresh_context(
        &self,
        expected: Id,
        context: &StreamContext,
        blob: Arc<Blob>,
    ) -> Result<(), ErrorCode> {
        if self.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        self.store
            .as_ref()
            .ok_or(ErrorCode::BadState)?
            .refresh(expected, context, blob)
    }
    pub(crate) fn generation(&self) -> Generation {
        self.store.as_ref().map_or_else(
            || self.generation.lock().unwrap_or_else(|e| e.into_inner()).0,
            |s| s.generation(),
        )
    }
    pub(crate) fn advance_seek(&self) -> Result<Generation, ErrorCode> {
        let cancel = {
            let mut state = self.generation.lock().unwrap_or_else(|e| e.into_inner());
            let seek = Id(state
                .0
                .seek
                .0
                .checked_add(1)
                .ok_or(ErrorCode::QuotaExceeded)?);
            let cancel = if let Some(store) = &self.store {
                store.advance_seek(seek)?;
                Some(std::mem::take(&mut state.1))
            } else {
                None
            };
            state.0.seek = seek;
            cancel
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        Ok(self.generation())
    }
}
impl Drop for StreamSession {
    fn drop(&mut self) {
        self.cancel();
    }
}
/// Runs on relay workers, never inside a Store or an application event callback.
pub(crate) trait Refresh: Send + Sync {
    fn refresh(&self, cancel: &HttpCancel) -> Result<(), ErrorCode>;
}
fn token() -> Result<String, ErrorCode> {
    let mut bytes = [0; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| ErrorCode::Internal)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
pub(crate) fn new_session_id() -> Result<Id, ErrorCode> {
    next_id().map(Id)
}
fn extension(ext: ResourceExtension) -> &'static str {
    match ext {
        ResourceExtension::Mp4 => "mp4",
        ResourceExtension::M4a => "m4a",
        ResourceExtension::M4s => "m4s",
        ResourceExtension::Ts => "ts",
        ResourceExtension::Aac => "aac",
        ResourceExtension::Webm => "webm",
        ResourceExtension::Vtt => "vtt",
        ResourceExtension::Srt => "srt",
        ResourceExtension::Ass => "ass",
        ResourceExtension::Key => "key",
        ResourceExtension::M3u8 => "m3u8",
    }
}

#[cfg(test)]
mod tests {
    use super::StreamSession;

    impl StreamSession {
        pub(crate) fn is_quiescent(&self) -> bool {
            self.activity.is_idle()
                && self.factory.is_quiescent()
                && self.store.as_ref().is_none_or(|s| s.is_quiescent())
        }
    }
}
