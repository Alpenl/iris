//! Blocking preparation entry for the host's background executor. Only ready
//! loopback resources and typed demuxer profiles reach the application thread.
use super::{
    guest::StreamStore,
    manifest::Profile,
    reader::{MemoryReader, Reader},
    relay::{BoundRelay, Relay},
    remote::RemoteFactory,
    remote_manifest,
    session::{StreamSession, new_session_id},
};
use crate::{
    actor::Spawn,
    engine::{CompiledModule, RuntimeEngine},
    instance::DetachedBlob,
    ledger::{Ledger, Reservation},
    network::{NetworkExecutor, NetworkGrant},
    pool::IoPool,
};
use iris_api::{
    SourceEntryKind,
    v2::{Capability, ErrorCode, Generation, Id, Role, stream::*},
};
use iris_base::net::http_client::HttpCancel;
use iris_core::library::Kind;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub(crate) struct GuestFactory {
    pub engine: RuntimeEngine,
    pub module: Arc<CompiledModule>,
    pub capabilities: BTreeSet<Capability>,
    pub io: IoPool,
    pub wake: Arc<dyn Fn() + Send + Sync>,
}
pub(crate) struct Preparation {
    pub network: NetworkGrant,
    pub ledger: Ledger,
    pub generation: Generation,
    pub guest: Option<GuestFactory>,
}
#[derive(Clone)]
pub struct AttachmentView {
    pub id: String,
    pub url: String,
    pub label: String,
    pub lang: Option<String>,
    pub default: bool,
    pub audio: bool,
}
pub struct Playback {
    pub(crate) relay: Relay,
    pub url: String,
    pub(crate) profile: Option<Profile>,
    pub kind: Kind,
    pub title: String,
    pub source_id: Option<String>,
    pub entry_id: Option<String>,
    pub is_live: bool,
    pub attachments: Vec<AttachmentView>,
    pub variants: Vec<iris_api::StreamVariant>,
    pub active_variant: Option<String>,
    pub(crate) notice_lease: Option<Reservation>,
}

impl iris_core::player::ScopedStream for Playback {
    fn root_url(&self) -> anyhow::Result<String> {
        self.relay
            .session()
            .root_url()
            .map_err(|e| anyhow::anyhow!("scoped stream root url failed: {e:?}"))
    }
    fn is_cancelled(&self) -> bool {
        self.relay.session().cancellation().is_cancelled()
    }
    fn advance_seek(&self) -> anyhow::Result<()> {
        self.relay
            .session()
            .advance_seek()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("scoped stream seek failed: {e:?}"))
    }
    fn stop(&self) {
        self.relay.stop();
    }
}

impl Playback {
    /// Adapts a prepared protocol-2 playback into a decoupled [`iris_core::player::ScopedPlayback`].
    pub(crate) fn into_scoped(self: Arc<Self>) -> Arc<iris_core::player::ScopedPlayback> {
        let profile = self.profile.map(|p| match p {
            Profile::Hls => iris_core::player::StreamProfile::Hls,
        });
        let attachments = self
            .attachments
            .iter()
            .map(|a| iris_core::player::ScopedAttachment {
                id: a.id.clone(),
                url: a.url.clone(),
                label: a.label.clone(),
                lang: a.lang.clone(),
                default: a.default,
                audio: a.audio,
            })
            .collect();
        Arc::new(iris_core::player::ScopedPlayback {
            url: self.url.clone(),
            profile,
            kind: self.kind,
            title: self.title.clone(),
            is_live: self.is_live,
            attachments,
            stream: self,
        })
    }
}

impl Preparation {
    pub(crate) fn prepare(
        self,
        spec: &StreamSpec,
        blobs: &BTreeMap<Id, DetachedBlob>,
        cancel: &HttpCancel,
    ) -> Result<Playback, ErrorCode> {
        validate(spec)?;
        let id = new_session_id()?;
        let context = match &spec.source {
            StreamSource::RandomAccess { context, .. } => Some(context),
            StreamSource::VirtualManifest { context, .. } => context.as_ref(),
            _ => None,
        };
        let blob = |id: Id| {
            let blob = blobs.get(&id).ok_or(ErrorCode::StaleHandle)?.data.clone();
            if blob.owner != self.network.owner() {
                return Err(ErrorCode::Denied);
            }
            Ok::<_, ErrorCode>(blob)
        };
        let store = if let Some(context) = context {
            let origins = context.origin_grants.iter().copied().collect();
            self.network.validate_origins(&origins)?;
            let factory = self.guest.ok_or(ErrorCode::Unsupported)?;
            let executor = Arc::new(NetworkExecutor::new(
                self.network.clone(),
                self.ledger.clone(),
                origins,
            )?);
            Some(Arc::new(StreamStore::spawn(
                Spawn {
                    engine: factory.engine,
                    module: factory.module,
                    role: Role::Stream,
                    owner: self.network.owner().into(),
                    grants: factory.capabilities,
                    ledger: self.ledger.clone(),
                    io: factory.io,
                    executor,
                    generation: self.generation,
                    wake: factory.wake,
                },
                id,
                context,
                blob(context.blob)?,
            )?))
        } else {
            None
        };
        let factory = RemoteFactory::new(self.network.clone(), self.ledger)?;
        let bound = BoundRelay::bind()?;
        let session = StreamSession::new(
            id,
            self.generation,
            bound.authority()?,
            factory.clone(),
            store.clone(),
        )?;
        // The caller's preparation task does not own the admitted playback.
        // A short-lived bridge cancels work before return; the host separately
        // binds the ready relay to installation/window/media lifetime.
        let preparation_cancel = cancel.child().map_err(|_| ErrorCode::Cancelled)?;
        preparation_cancel
            .link(session.cancellation())
            .map_err(|_| ErrorCode::Cancelled)?;
        let mut is_live = spec.is_live;
        match &spec.source {
            StreamSource::DirectHttp {
                primary,
                fallback_urls,
                identity,
            } => {
                let proof = identity
                    .as_ref()
                    .map(|i| Ok::<_, ErrorCode>((i, blob(i.hashes)?)))
                    .transpose()?;
                let reader = factory.direct(primary, fallback_urls, proof)?;
                let (prefix, eof) = probe_prefix_eof(&reader, ADAPTIVE_PROBE_LIMIT, cancel)
                    .inspect_err(|err| {
                        log::warn!("plugin primary stream probe failed: {err:?}");
                    })?;
                if adaptive(&prefix) {
                    if identity.is_some() || !fallback_urls.is_empty() {
                        return Err(ErrorCode::Unsupported);
                    }
                    let document = remote_manifest::convert(&factory, primary, cancel)?;
                    session.commit(session.prepare_manifest(
                        Id(0),
                        document.format,
                        &document.bytes,
                    )?)?;
                    is_live = document.live;
                    remote_manifest::live::attach(&session, primary.clone(), &document)?;
                } else if !eof && adaptive_undecided(&prefix) {
                    return Err(ErrorCode::Unsupported);
                } else {
                    session.commit(session.prepare_reader(reader)?)?;
                }
            }
            StreamSource::RandomAccess {
                resource_id, size, ..
            } => {
                let reader = store
                    .as_ref()
                    .ok_or(ErrorCode::BadState)?
                    .reader(resource_id.clone(), size.map(|n| n.0))?;
                // RandomAccess is an opaque byte representation, not an escape
                // hatch for raw playlists. Adaptive output uses VirtualManifest.
                ensure_non_adaptive_prefix(&reader, cancel)?;
                session.commit(session.prepare_reader(reader)?)?;
            }
            StreamSource::VirtualManifest {
                format, document, ..
            } => {
                session.enable_guest_manifest_updates();
                session.commit(session.prepare_manifest(
                    Id(0),
                    *format,
                    &blob(*document)?.bytes,
                )?)?;
            }
        }
        let mut attachments = Vec::new();
        for (audio, list) in [(true, &spec.audio_tracks), (false, &spec.subtitles)] {
            for attachment in list {
                if cancel.is_cancelled() {
                    return Err(ErrorCode::Cancelled);
                }
                let route_id = format!("{}_{}", if audio { "audio" } else { "sub" }, attachment.id);
                let prepared: Result<(Arc<dyn Reader>, &str), ErrorCode> = if audio {
                    match factory.reader(&attachment.resource, None) {
                        Ok(reader) => match ensure_non_adaptive_prefix(&reader, cancel) {
                            Ok(()) => Ok((reader, "bin")),
                            Err(ErrorCode::Unsupported) => return Err(ErrorCode::Unsupported),
                            Err(err) => Err(err),
                        },
                        Err(err) => Err(err),
                    }
                } else {
                    match factory.fetch(&attachment.resource, cancel, 10 << 20) {
                        Ok(fetched) => match subtitle_format(&fetched.bytes) {
                            Ok(extension) => MemoryReader::new(
                                fetched.bytes.clone(),
                                factory.owner(),
                                factory.ledger(),
                            )
                            .map(|reader| (Arc::new(reader) as Arc<dyn Reader>, extension)),
                            Err(err) => Err(err),
                        },
                        Err(err) => Err(err),
                    }
                };
                let (reader, extension) = match prepared {
                    Ok(pair) => pair,
                    Err(ErrorCode::Cancelled) => return Err(ErrorCode::Cancelled),
                    Err(err) => {
                        log::warn!(
                            "skipping failed plugin {} attachment '{}' ({}): {err:?}",
                            if audio { "audio" } else { "subtitle" },
                            attachment.id,
                            attachment.label
                        );
                        continue;
                    }
                };
                let url = session.add_attachment(&route_id, extension, reader)?;
                attachments.push(AttachmentView {
                    id: attachment.id.clone(),
                    url,
                    label: attachment.label.clone(),
                    lang: attachment.lang.clone(),
                    default: attachment.default,
                    audio,
                });
            }
        }
        if !spec.audio_tracks.is_empty()
            && !attachments.iter().any(|t| t.audio && t.default)
            && let Some(first_audio) = attachments.iter_mut().find(|t| t.audio)
        {
            first_audio.default = true;
        }
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let url = session.root_url()?;
        let profile = session.profile();
        let kind = match spec.kind {
            Some(SourceEntryKind::Audio) => Kind::Audio,
            Some(SourceEntryKind::Photo) => Kind::Photo,
            _ => Kind::Video,
        };
        Ok(Playback {
            relay: bound.start(session)?,
            url,
            profile,
            kind,
            title: spec.title.clone().unwrap_or_default(),
            source_id: spec.source_id.clone(),
            entry_id: spec.entry_id.clone(),
            is_live,
            attachments,
            variants: spec.variants.clone(),
            active_variant: spec.active_variant.clone(),
            notice_lease: None,
        })
    }
}
pub(crate) const ADAPTIVE_PROBE_LIMIT: usize = 4096;
pub(crate) fn probe_prefix<R: Reader + ?Sized>(
    reader: &Arc<R>,
    limit: usize,
    cancel: &HttpCancel,
) -> Result<Vec<u8>, ErrorCode> {
    probe_prefix_eof(reader, limit, cancel).map(|(bytes, _)| bytes)
}
pub(crate) fn probe_prefix_eof<R: Reader + ?Sized>(
    reader: &Arc<R>,
    limit: usize,
    cancel: &HttpCancel,
) -> Result<(Vec<u8>, bool), ErrorCode> {
    let mut out = Vec::with_capacity(limit.min(ADAPTIVE_PROBE_LIMIT));
    let mut eof = false;
    while out.len() < limit {
        let step = reader.read_at(out.len() as u64, (limit - out.len()) as u32, cancel)?;
        if step.blob.bytes.is_empty() {
            eof = step.eof;
            break;
        }
        out.extend_from_slice(&step.blob.bytes);
        if step.eof {
            eof = true;
            break;
        }
    }
    Ok((out, eof))
}
pub(crate) fn ensure_non_adaptive_prefix<R: Reader + ?Sized>(
    reader: &Arc<R>,
    cancel: &HttpCancel,
) -> Result<(), ErrorCode> {
    let mut out = Vec::with_capacity(ADAPTIVE_PROBE_LIMIT);
    let mut eof = false;
    while out.len() < ADAPTIVE_PROBE_LIMIT && adaptive_undecided(&out) {
        let step = reader.read_at(
            out.len() as u64,
            (ADAPTIVE_PROBE_LIMIT - out.len()) as u32,
            cancel,
        )?;
        if step.blob.bytes.is_empty() {
            eof = step.eof;
            break;
        }
        out.extend_from_slice(&step.blob.bytes);
        if adaptive(&out) {
            return Err(ErrorCode::Unsupported);
        }
        if step.eof {
            eof = true;
            break;
        }
    }
    if adaptive(&out) || (!eof && adaptive_undecided(&out)) {
        return Err(ErrorCode::Unsupported);
    }
    Ok(())
}
pub(crate) fn adaptive(bytes: &[u8]) -> bool {
    iris_base::net::loopback_http::is_adaptive_manifest(bytes)
}
pub(crate) fn adaptive_undecided(bytes: &[u8]) -> bool {
    iris_base::net::loopback_http::is_adaptive_prefix_undecided(bytes)
}
pub(crate) fn subtitle_format(bytes: &[u8]) -> Result<&'static str, ErrorCode> {
    if bytes.len() > 10 << 20 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ErrorCode::InvalidArgument)?
        .trim_start_matches('\u{feff}')
        .trim_ascii();
    if text.is_empty() || text.contains('\0') || text.lines().any(|l| l.len() > 8192) {
        return Err(ErrorCode::InvalidArgument);
    }
    if text
        .strip_prefix("WEBVTT")
        .is_some_and(|tail| tail.starts_with(['\r', '\n', ' ', '\t']))
    {
        return Ok("vtt");
    }
    if text.starts_with("[Script Info]")
        && text.contains("[Events]")
        && !text.lines().any(|line| {
            let section = line.trim();
            section.eq_ignore_ascii_case("[Fonts]") || section.eq_ignore_ascii_case("[Graphics]")
        })
    {
        return Ok("ass");
    }
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    let second = lines.next().unwrap_or("");
    let timing = if first.bytes().all(|b| b.is_ascii_digit()) {
        second
    } else {
        first
    };
    if let Some((start, end)) = timing.split_once(" --> ")
        && timestamp(start)
        && timestamp(end)
    {
        return Ok("srt");
    }
    Err(ErrorCode::Unsupported)
}
fn timestamp(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 12
        && b[2] == b':'
        && b[5] == b':'
        && matches!(b[8], b',' | b'.')
        && b.iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 2 | 5 | 8) || c.is_ascii_digit())
}
fn validate(spec: &StreamSpec) -> Result<(), ErrorCode> {
    if spec.kind == Some(SourceEntryKind::Folder)
        || spec
            .title
            .as_ref()
            .is_some_and(|v| v.len() > 512 || v.chars().any(char::is_control))
        || spec.variants.len() > 64
        || spec.audio_tracks.len() > 16
        || spec.subtitles.len() > 32
    {
        return Err(ErrorCode::InvalidArgument);
    }
    for list in [&spec.audio_tracks, &spec.subtitles] {
        let mut ids = BTreeSet::new();
        if list.iter().filter(|t| t.default).count() > 1 {
            return Err(ErrorCode::InvalidArgument);
        }
        for attachment in list {
            super::manifest::logical(&attachment.id)?;
            if attachment.id.len() > 110
                || !ids.insert(&attachment.id)
                || attachment.label.len() > 256
                || attachment.label.chars().any(char::is_control)
                || attachment.lang.as_ref().is_some_and(|v| {
                    v.len() > 48 || !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
            {
                return Err(ErrorCode::InvalidArgument);
            }
        }
    }
    Ok(())
}
