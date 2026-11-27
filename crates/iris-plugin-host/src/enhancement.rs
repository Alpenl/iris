//! Enhancement-owned transport. It never borrows the playing source's network
//! grant, and native decoders receive only sealed loopback resource URLs.
use super::{
    actor::ReadResult,
    files::Artifact,
    instance::{Blob, next_id},
    ledger::{Ledger, Reservation, Resource},
    runtime::WindowAccess,
    stream::{
        prepare::subtitle_format,
        reader::{MemoryReader, Reader, check_read},
        relay::{BoundRelay, Relay},
        remote::RemoteFactory,
        session::StreamSession,
    },
};
use iris_api::v2::{
    Capability, ErrorCode, Generation, Id,
    enhance::{EnhancementResource, SubtitleFormat},
};
use iris_net::http_client::HttpCancel;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct AttachmentRegistry {
    relay: Mutex<Option<Arc<Relay>>>,
}
pub struct Attachment {
    pub owner: String,
    pub id: String,
    pub label: String,
    pub lang: Option<String>,
    pub audio: bool,
    pub generation: Generation,
    url: String,
    route: String,
    registry: Arc<AttachmentRegistry>,
    relay: Arc<Relay>,
    _lease: Reservation,
}
impl Attachment {
    pub fn is_live(&self) -> bool {
        !self.relay.session().cancellation().is_cancelled()
    }
}

impl iris_core::player::ExternalTrack for Attachment {
    fn owner(&self) -> &str {
        &self.owner
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn lang(&self) -> Option<&str> {
        self.lang.as_deref()
    }
    fn is_audio(&self) -> bool {
        self.audio
    }
    fn url(&self) -> &str {
        &self.url
    }
    fn is_live(&self) -> bool {
        Attachment::is_live(self)
    }
}
impl Drop for Attachment {
    fn drop(&mut self) {
        let _guard = self
            .registry
            .relay
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ = self.relay.session().remove_attachment(&self.route);
    }
}
pub struct AttachmentSpec {
    pub id: String,
    pub label: String,
    pub lang: Option<String>,
    /// None means an audio track; subtitle formats must agree with parsed content.
    pub subtitle: Option<SubtitleFormat>,
    pub resource: EnhancementResource,
}
impl WindowAccess {
    /// Background worker only. The returned lease must live until mpv has
    /// acknowledged removal, including cancelled/replaced pending additions.
    pub fn prepare_attachment(&self, spec: AttachmentSpec) -> Result<Arc<Attachment>, ErrorCode> {
        self.check(self.generation, Capability::MediaEnhance)?;
        super::validation::identifier(&spec.id)?;
        super::validation::text(&spec.label, 256)?;
        if let Some(lang) = &spec.lang {
            super::validation::text(lang, 64)?;
        }
        let lease = self.resources.ledger.reserve(
            &self.owner,
            &[
                (Resource::EnhancementTrack, 1),
                (Resource::HostBytes, 16384),
            ],
        )?;
        let factory = RemoteFactory::new(self.network.clone(), self.resources.ledger.clone())?;
        let mut reader: Arc<dyn Reader> = match &spec.resource {
            EnhancementResource::Remote { request } => factory.reader(request, None)?,
            EnhancementResource::Artifact { handle } => Arc::new(ArtifactReader {
                artifact: self
                    .files
                    .approved_artifact(&self.owner, *handle, self.generation)?,
                ledger: self.resources.ledger.clone(),
            }),
        };
        let extension = if let Some(format) = spec.subtitle {
            let _scratch = self
                .resources
                .ledger
                .reserve(&self.owner, &[(Resource::HostBytes, 24 << 20)])?;
            let bytes = match &spec.resource {
                EnhancementResource::Remote { request } => {
                    factory.fetch(request, &self.cancel, 10 << 20)?.bytes
                }
                _ => read_document(&reader, &self.cancel)?,
            };
            let extension = subtitle_format(&bytes)?;
            let expected = match format {
                SubtitleFormat::SubRip => "srt",
                SubtitleFormat::WebVtt => "vtt",
                SubtitleFormat::Ass => "ass",
            };
            if extension != expected {
                return Err(ErrorCode::InvalidArgument);
            }
            reader = Arc::new(MemoryReader::new(
                bytes,
                &self.owner,
                &self.resources.ledger,
            )?);
            extension
        } else {
            let prefix = super::stream::prepare::probe_prefix(&reader, 4096, &self.cancel)?;
            if super::stream::prepare::adaptive(&prefix) {
                return Err(ErrorCode::Unsupported);
            }
            "bin"
        };
        let mut registry = self
            .attachments
            .relay
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let relay = if let Some(relay) = registry.as_ref() {
            relay.clone()
        } else {
            let bound = BoundRelay::bind()?;
            let session = StreamSession::new(
                Id(next_id()?),
                self.generation,
                bound.authority()?,
                factory,
                None,
            )?;
            session.commit(session.prepare_reader(Arc::new(MemoryReader::new(
                Vec::new(),
                &self.owner,
                &self.resources.ledger,
            )?))?)?;
            self.cancel
                .link(session.cancellation())
                .map_err(|_| ErrorCode::Cancelled)?;
            let relay = Arc::new(bound.start(session)?);
            *registry = Some(relay.clone());
            relay
        };
        let route = format!("enh_{}", next_id()?);
        let url = relay.session().add_attachment(&route, extension, reader)?;
        Ok(Arc::new(Attachment {
            owner: self.owner.clone(),
            id: spec.id,
            label: spec.label,
            lang: spec.lang,
            audio: spec.subtitle.is_none(),
            generation: self.generation,
            url,
            route,
            registry: self.attachments.clone(),
            relay,
            _lease: lease,
        }))
    }
}
struct ArtifactReader {
    artifact: Arc<Artifact>,
    ledger: Ledger,
}
impl Reader for ArtifactReader {
    fn size(&self, cancel: &HttpCancel) -> Result<Option<u64>, ErrorCode> {
        if cancel.is_cancelled() {
            Err(ErrorCode::Cancelled)
        } else {
            Ok(Some(self.artifact.size()))
        }
    }
    fn read_at(&self, offset: u64, len: u32, cancel: &HttpCancel) -> Result<ReadResult, ErrorCode> {
        check_read(offset, len, cancel)?;
        if offset > self.artifact.size() {
            return Err(ErrorCode::RangeInvalid);
        }
        let len = (self.artifact.size() - offset).min(u64::from(len)) as usize;
        let _scratch = self
            .ledger
            .reserve(self.artifact.owner(), &[(Resource::HostBytes, len as u64)])?;
        let mut bytes = vec![0; len];
        let read = self.artifact.read_at(offset, &mut bytes, cancel)?;
        if read == 0 && len != 0 {
            return Err(ErrorCode::IoError);
        }
        bytes.truncate(read);
        Ok(ReadResult {
            blob: Arc::new(Blob::new(&self.ledger, self.artifact.owner(), bytes)?),
            eof: offset + read as u64 == self.artifact.size(),
        })
    }
}
fn read_document(reader: &Arc<dyn Reader>, cancel: &HttpCancel) -> Result<Vec<u8>, ErrorCode> {
    let len = reader.size(cancel)?.ok_or(ErrorCode::Unsupported)?;
    if len > 10 << 20 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut bytes = Vec::with_capacity(len as usize);
    while bytes.len() < len as usize {
        let chunk = reader.read_at(bytes.len() as u64, iris_api::v2::CHUNK_LIMIT as u32, cancel)?;
        if chunk.blob.bytes.is_empty() {
            return Err(ErrorCode::IoError);
        }
        if bytes.len() + chunk.blob.bytes.len() > len as usize {
            return Err(ErrorCode::RangeInvalid);
        }
        bytes.extend_from_slice(&chunk.blob.bytes);
    }
    Ok(bytes)
}
