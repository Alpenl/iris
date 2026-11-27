//! Bounded enhancement job execution, timed-overlay frame stepping, annotation
//! selection grants, and remote thumbnail preparation for [`super::WindowSession`].
use super::{
    Event, PollReport, WindowSession,
    effects::{
        EnhancementOutcome, EnhancementRequest, POLL_EFFECT_LIMIT, ReadyAnnotations,
        ReadyAttachment, ReadyEffect, ReadyMonitorLut, ReadyOverlayUpdate, ReadyStreamUpdate,
        TimelineLease, is_superseded_error,
    },
};
use crate::{
    enhancement::{Attachment, AttachmentSpec},
    hub::{Binding, Client},
    lut::MonitorLut,
    overlay::{OverlayBank, OverlayClock},
    runtime::{BackgroundResult, WindowAccess},
    selection::{
        SelectionGrants, annotation_error_code, edit_to_store, error_code_to_annotation,
        page_to_api, transact_annotations,
    },
};
use iris_api::v2::{
    Capability, ErrorCode, Generation, GuestError, HostCommand, HostEvent, Id, Reply,
    enhance::{HeatRange, SkipRange},
    source::PlaybackSnapshot,
};
use iris_core::{
    media_uri::MediaUri,
    store::{AnnotationError, AnnotationSnapshot, AnnotationTarget},
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, PoisonError},
};

enum WorkerPayload {
    Attachment(Arc<Attachment>),
    Lut(Arc<MonitorLut>),
    Overlay,
    Stream,
    Annotations(Option<AnnotationSnapshot>, Box<Event>),
}

struct QueuedReady {
    key: (String, String),
    job_id: Id,
    binding: Arc<Binding>,
    generation: Generation,
    outcome: Result<BackgroundResult<WorkerPayload>, ErrorCode>,
}

pub(super) struct ReadyQueue {
    closed: bool,
    generation: Generation,
    items: VecDeque<QueuedReady>,
}

impl ReadyQueue {
    pub(super) fn shutdown(&mut self) {
        self.closed = true;
        self.items.clear();
    }
}

pub(super) struct EnhancementState {
    pub sequence: u64,
    pub media_announced: Option<Id>,
    pub announced: BTreeMap<String, Generation>,
    pub bindings: BTreeMap<String, Arc<Binding>>,
    pub pending: BTreeMap<(String, String), Id>,
    pub lut_owner: Option<String>,
    pub timeline: Option<TimelineLease>,
    pub bank: Option<Arc<Mutex<OverlayBank>>>,
    pub clock: OverlayClock,
    pub overlay_generation: Generation,
    pub selections: BTreeMap<String, SelectionGrants>,
    pub ready: Arc<Mutex<ReadyQueue>>,
}

impl EnhancementState {
    pub(super) fn new(generation: Generation) -> Self {
        Self {
            sequence: 0,
            media_announced: None,
            announced: BTreeMap::new(),
            bindings: BTreeMap::new(),
            pending: BTreeMap::new(),
            lut_owner: None,
            timeline: None,
            bank: None,
            clock: OverlayClock::default(),
            overlay_generation: generation,
            selections: BTreeMap::new(),
            ready: Arc::new(Mutex::new(ReadyQueue {
                closed: false,
                generation,
                items: VecDeque::new(),
            })),
        }
    }

    pub(super) fn cancel_lut(&mut self, owner: &str) -> bool {
        // A clear also supersedes preparation of a first LUT, or a replacement
        // while another plugin still owns the currently applied filter.
        self.pending.remove(&(owner.into(), "lut".into()));
        self.lut_owner.as_deref() == Some(owner)
    }

    pub(super) fn finish_job(&mut self, key: &(String, String), id: Id) -> bool {
        if self.pending.get(key) != Some(&id) {
            return false;
        }
        self.pending.remove(key);
        true
    }

    pub(super) fn reset_overlays(&mut self) {
        self.bank = None;
        self.announced.clear();
    }

    pub(super) fn reset_for_media(&mut self, next_generation: Generation) {
        self.selections.clear();
        self.media_announced = None;
        self.overlay_generation = next_generation;
        self.reset_overlays();
        self.pending.clear();
        self.lut_owner = None;
        self.timeline = None;
        let mut ready = self.ready.lock().unwrap_or_else(PoisonError::into_inner);
        ready.generation = next_generation;
        ready.items.clear();
    }
}

impl WindowSession {
    pub(super) fn drain_ready_effects(&mut self, current: Generation) -> Vec<ReadyEffect> {
        let mut out = Vec::new();
        let ready_arc = self.enhance.ready.clone();
        let has_more = {
            let mut ready = ready_arc.lock().unwrap_or_else(PoisonError::into_inner);
            ready.generation = current;
            while out.len() < POLL_EFFECT_LIMIT
                && let Some(item) = ready.items.pop_front()
            {
                if !self.enhance.finish_job(&item.key, item.job_id) {
                    continue;
                }
                if !item.binding.is_live() || item.generation != current {
                    continue;
                }
                let plugin_id = item.binding.installed.plugin_id.clone();
                let owner = item.key.0;
                match item.outcome {
                    Ok(bg) => {
                        let (payload, lease) = bg.into_parts();
                        let effect = match payload {
                            WorkerPayload::Attachment(track) => {
                                ReadyEffect::Attachment(ReadyAttachment {
                                    track,
                                    _worker_lease: lease,
                                })
                            }
                            WorkerPayload::Lut(lut) => ReadyEffect::MonitorLut(ReadyMonitorLut {
                                owner,
                                lut,
                                _worker_lease: lease,
                            }),
                            WorkerPayload::Overlay => {
                                ReadyEffect::TimedOverlay(ReadyOverlayUpdate {
                                    _worker_lease: lease,
                                })
                            }
                            WorkerPayload::Stream => {
                                if self.live_active_stream().is_none_or(|active| {
                                    active.plugin_id != plugin_id
                                        || active.admitted_generation != current
                                }) {
                                    continue;
                                }
                                ReadyEffect::StreamUpdated(ReadyStreamUpdate {
                                    plugin_id,
                                    _worker_lease: lease,
                                })
                            }
                            WorkerPayload::Annotations(snapshot, event) => {
                                ReadyEffect::Annotations(Box::new(ReadyAnnotations {
                                    plugin_id,
                                    snapshot,
                                    event: *event,
                                    _worker_lease: lease,
                                }))
                            }
                        };
                        out.push(effect);
                    }
                    Err(code) if is_superseded_error(code) => {
                        log::debug!("ignoring superseded plugin enhancement job: {code:?}");
                    }
                    Err(code) => {
                        out.push(ReadyEffect::Failed { plugin_id, code });
                    }
                }
            }
            !ready.items.is_empty() && !ready.closed
        };
        if has_more {
            self.client.notify_wake();
        }
        out
    }

    pub fn commit_timeline(&mut self, lease: TimelineLease) {
        self.enhance.timeline = Some(lease);
    }

    pub fn timeline_ranges(&self) -> Option<(&[HeatRange], &[SkipRange])> {
        let lease = self.enhance.timeline.as_ref()?;
        if !self.enhance.bindings.get(lease.owner())?.is_live() {
            return None;
        }
        lease.ranges()
    }

    pub fn commit_lut_owner(&mut self, owner: String) {
        self.enhance.lut_owner = Some(owner);
    }

    pub fn confirm_lut_cleared(&mut self, owner: &str) {
        if self.enhance.lut_owner.as_deref() == Some(owner) {
            self.enhance.lut_owner = None;
        }
    }

    pub fn announce_media_opened(&mut self, snapshot: PlaybackSnapshot) -> Option<PollReport> {
        let media = self.generation().media;
        if self.enhance.media_announced == Some(media) {
            return None;
        }
        self.enhance.media_announced = Some(media);
        Some(self.broadcast(&HostEvent::MediaOpened {
            generation: self.generation(),
            snapshot,
        }))
    }

    /// Grants a bounded annotation selection over `uris` to `plugin` and sends `SelectionGranted`.
    pub fn grant_annotations(&mut self, plugin: &str, uris: Vec<MediaUri>) -> Option<PollReport> {
        let access = self.client.access(plugin).ok()?;
        if !access.capabilities.contains(&Capability::Annotations) {
            return None;
        }
        if let Some(binding) = self
            .client
            .catalog()
            .plugins
            .iter()
            .find(|p| p.id == plugin)
            .and_then(|p| p.binding.clone())
        {
            self.enhance.bindings.insert(access.owner.clone(), binding);
        }
        let grants = self
            .enhance
            .selections
            .entry(access.owner.clone())
            .or_insert_with(|| {
                SelectionGrants::new(
                    access.owner.clone(),
                    &access.capabilities,
                    access.resources.ledger.clone(),
                )
            });
        grants.revoke();
        let selection = grants
            .approve(access.instance, access.generation.media, uris)
            .ok()?;
        Some(self.send_to(
            plugin,
            HostEvent::SelectionGranted {
                selection,
                media: access.generation.media,
                count: 1,
            },
        ))
    }

    /// Validates and executes/schedules an [`EnhancementRequest`], preserving all
    /// `Ledger` quotas, cancel-token links, and stale-generation rejections.
    pub fn execute_enhancement(
        &mut self,
        request: EnhancementRequest,
        media_loaded: bool,
        position_ms: u64,
        annotation_target: impl FnOnce() -> Result<AnnotationTarget, AnnotationError>,
    ) -> EnhancementOutcome {
        match self.try_execute_enhancement(&request, media_loaded, position_ms, annotation_target) {
            Ok(Some(immediate)) => EnhancementOutcome::Immediate(immediate),
            Ok(None) => EnhancementOutcome::Queued,
            Err((_, true)) => EnhancementOutcome::Noop,
            Err((code, false)) => {
                let report = self.annotation_error_reply(&request, code);
                EnhancementOutcome::Rejected {
                    code,
                    superseded: is_superseded_error(code),
                    report,
                }
            }
        }
    }

    fn annotation_error_reply(&self, request: &EnhancementRequest, code: ErrorCode) -> PollReport {
        let (HostCommand::ReadAnnotation {
            request_id,
            selection,
            ..
        }
        | HostCommand::EditAnnotation {
            request_id,
            selection,
            ..
        }) = request.command()
        else {
            return PollReport::default();
        };
        self.send_to(
            &request.binding.installed.plugin_id,
            HostEvent::AnnotationResult {
                request_id: *request_id,
                selection: *selection,
                result: Reply::Error {
                    error: GuestError {
                        code: code as i32,
                        message: format!("{code:?}"),
                    },
                },
            },
        )
    }

    fn try_execute_enhancement(
        &mut self,
        request: &EnhancementRequest,
        media_loaded: bool,
        position_ms: u64,
        annotation_target: impl FnOnce() -> Result<AnnotationTarget, AnnotationError>,
    ) -> Result<Option<ReadyEffect>, (ErrorCode, bool)> {
        let current_gen = self.generation();
        if !request.binding.is_live() || request.update.is_cancelled() {
            return Err((ErrorCode::Cancelled, false));
        }
        if request.update.generation != current_gen {
            return Err((ErrorCode::StaleHandle, false));
        }
        let access = self
            .client
            .access(&request.binding.installed.plugin_id)
            .map_err(|e| (e, false))?;
        let owner = access.owner.clone();
        self.enhance
            .bindings
            .insert(owner.clone(), request.binding.clone());

        let check_media = |media: Id, generation: Generation| -> Result<(), (ErrorCode, bool)> {
            if media != current_gen.media || generation != current_gen {
                return Err((ErrorCode::StaleHandle, false));
            }
            if !media_loaded {
                return Err((ErrorCode::BadState, false));
            }
            Ok(())
        };
        let check_overlay = |media: Id, generation: Generation| -> Result<(), (ErrorCode, bool)> {
            if media != current_gen.media || generation != self.enhance.overlay_generation {
                return Err((ErrorCode::StaleHandle, false));
            }
            Ok(())
        };

        match request.command().clone() {
            HostCommand::SetTimelineMeta {
                media, generation, ..
            } => {
                check_media(media, generation)?;
                Ok(Some(ReadyEffect::Timeline(TimelineLease {
                    binding: request.binding.clone(),
                    update: request.update.clone(),
                    index: request.index,
                })))
            }
            HostCommand::SetMonitorLut {
                media,
                generation,
                artifact,
            } => {
                check_media(media, generation)?;
                if let Some(id) = artifact {
                    self.submit_enhancement_job(request, access, "lut".into(), move |access| {
                        access.check(generation, Capability::MediaEnhance)?;
                        let artifact =
                            access
                                .files
                                .approved_artifact(&access.owner, id, generation)?;
                        let lut = MonitorLut::prepare(
                            artifact,
                            &access.resources.ledger,
                            &access.cancel,
                        )?;
                        Ok(WorkerPayload::Lut(lut))
                    })
                    .map(|()| None)
                    .map_err(|e| (e, false))
                } else if self.enhance.cancel_lut(&owner) {
                    Ok(Some(ReadyEffect::ClearMonitorLut { owner }))
                } else {
                    Err((ErrorCode::Cancelled, true))
                }
            }
            HostCommand::AttachSubtitle {
                media,
                generation,
                id,
                label,
                lang,
                format,
                resource,
            } => {
                check_media(media, generation)?;
                self.submit_enhancement_job(request, access, format!("track:{id}"), move |access| {
                    let track = access.prepare_attachment_for(
                        generation,
                        AttachmentSpec {
                            id,
                            label,
                            lang,
                            subtitle: Some(format),
                            resource,
                        },
                    )?;
                    Ok(WorkerPayload::Attachment(track))
                })
                .map(|()| None)
                .map_err(|e| (e, false))
            }
            HostCommand::AttachAudioTrack {
                media,
                generation,
                id,
                label,
                lang,
                resource,
            } => {
                check_media(media, generation)?;
                self.submit_enhancement_job(request, access, format!("track:{id}"), move |access| {
                    let track = access.prepare_attachment_for(
                        generation,
                        AttachmentSpec {
                            id,
                            label,
                            lang,
                            subtitle: None,
                            resource,
                        },
                    )?;
                    Ok(WorkerPayload::Attachment(track))
                })
                .map(|()| None)
                .map_err(|e| (e, false))
            }
            HostCommand::SetTimedOverlay {
                media,
                generation,
                track,
                window_revision,
                cues,
            }
            | HostCommand::AppendTimedCues {
                media,
                generation,
                track,
                window_revision,
                cues,
            } => {
                check_overlay(media, generation)?;
                let append = matches!(request.command(), HostCommand::AppendTimedCues { .. });
                let bank = self.ensure_overlay_bank(&access, position_ms);
                let blob = request
                    .update
                    .blobs
                    .get(&cues)
                    .ok_or((ErrorCode::StaleHandle, false))?
                    .data
                    .clone();
                self.submit_enhancement_job(
                    request,
                    access,
                    format!("overlay:{track}"),
                    move |access| {
                        bank.lock().unwrap_or_else(PoisonError::into_inner).submit(
                            &access.owner,
                            media,
                            generation,
                            &track,
                            window_revision,
                            &blob,
                            append,
                        )?;
                        Ok(WorkerPayload::Overlay)
                    },
                )
                .map(|()| None)
                .map_err(|e| (e, false))
            }
            HostCommand::ClearTimedOverlay {
                media,
                generation,
                track,
            } => {
                check_overlay(media, generation)?;
                let bank = self.ensure_overlay_bank(&access, position_ms);
                self.submit_enhancement_job(
                    request,
                    access,
                    format!("overlay:{track}"),
                    move |access| {
                        bank.lock().unwrap_or_else(PoisonError::into_inner).clear(
                            &access.owner,
                            media,
                            generation,
                            &track,
                        )?;
                        Ok(WorkerPayload::Overlay)
                    },
                )
                .map(|()| None)
                .map_err(|e| (e, false))
            }
            HostCommand::UpdateManifest {
                session,
                expected_revision,
                format,
                document,
            } => {
                let current = self
                    .live_active_stream()
                    .filter(|s| {
                        s.session.id == session
                            && s.owner == owner
                            && s.session.allows_guest_manifest_updates()
                    })
                    .map(|s| s.session)
                    .ok_or((ErrorCode::Denied, false))?;
                let blob = request
                    .update
                    .blobs
                    .get(&document)
                    .ok_or((ErrorCode::StaleHandle, false))?
                    .data
                    .clone();
                self.submit_enhancement_job(request, access, "stream-manifest".into(), move |_| {
                    current.commit(current.prepare_manifest(
                        expected_revision,
                        format,
                        &blob.bytes,
                    )?)?;
                    Ok(WorkerPayload::Stream)
                })
                .map(|()| None)
                .map_err(|e| (e, false))
            }
            HostCommand::UpdateStreamContext {
                session,
                expected_version,
                context,
            } => {
                let current = self
                    .live_active_stream()
                    .filter(|s| s.session.id == session && s.owner == owner)
                    .map(|s| s.session)
                    .ok_or((ErrorCode::Denied, false))?;
                let blob = request
                    .update
                    .blobs
                    .get(&context.blob)
                    .ok_or((ErrorCode::StaleHandle, false))?
                    .data
                    .clone();
                self.submit_enhancement_job(request, access, "stream-context".into(), move |_| {
                    current.refresh_context(expected_version, &context, blob)?;
                    Ok(WorkerPayload::Stream)
                })
                .map(|()| None)
                .map_err(|e| (e, false))
            }
            HostCommand::EditAnnotation {
                request_id,
                selection,
                edit,
            } => self
                .submit_plugin_annotation(
                    request,
                    access,
                    request_id,
                    selection,
                    Some(edit),
                    (0, 0),
                    annotation_target,
                )
                .map(|()| None)
                .map_err(|e| (e, false)),
            HostCommand::ReadAnnotation {
                request_id,
                selection,
                index,
                marker_offset,
            } => self
                .submit_plugin_annotation(
                    request,
                    access,
                    request_id,
                    selection,
                    None,
                    (index, marker_offset),
                    annotation_target,
                )
                .map(|()| None)
                .map_err(|e| (e, false)),
            _ => Err((ErrorCode::BadState, false)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_plugin_annotation(
        &mut self,
        request: &EnhancementRequest,
        access: WindowAccess,
        request_id: Id,
        selection: Id,
        edit: Option<iris_api::v2::enhance::AnnotationEdit>,
        page: (u32, u32),
        annotation_target: impl FnOnce() -> Result<AnnotationTarget, AnnotationError>,
    ) -> Result<(), ErrorCode> {
        access.check(request.update.generation, Capability::Annotations)?;
        let permit = self
            .enhance
            .selections
            .get(&access.owner)
            .ok_or(ErrorCode::StaleHandle)?
            .permit(
                &access.owner,
                access.instance,
                access.generation.media,
                selection,
            )?;
        let target = annotation_target().map_err(annotation_error_code)?;
        let store_edit = edit.as_ref().map(edit_to_store);
        let binding = request.binding.clone();
        self.submit_enhancement_job(request, access, "annotations".into(), move |access| {
            let result: Result<_, ErrorCode> = (|| {
                let uris = permit.uris()?;
                let snapshot = transact_annotations(
                    &target,
                    store_edit.as_ref().map(|e| (uris, e)),
                    &access.owner,
                    &access.resources.ledger,
                    || {
                        permit.check().map_err(error_code_to_annotation)?;
                        if !binding.is_live() || access.cancel.is_cancelled() {
                            Err(AnnotationError::Cancelled)
                        } else {
                            Ok(())
                        }
                    },
                )?;
                let values = if store_edit.is_some() {
                    Vec::new()
                } else {
                    vec![page_to_api(
                        snapshot
                            .page(uris, page.0, page.1)
                            .map_err(annotation_error_code)?,
                    )]
                };
                Ok((snapshot, values))
            })();
            let (snapshot, reply) = match result {
                Ok((snapshot, values)) => (Some(snapshot), Reply::Ok { ok: values }),
                Err(error) => (
                    None,
                    Reply::Error {
                        error: GuestError {
                            code: error as i32,
                            message: format!("{error:?}"),
                        },
                    },
                ),
            };
            Ok(WorkerPayload::Annotations(
                snapshot,
                Box::new(HostEvent::AnnotationResult {
                    request_id,
                    selection,
                    result: reply,
                }),
            ))
        })
    }

    fn submit_enhancement_job(
        &mut self,
        request: &EnhancementRequest,
        access: WindowAccess,
        key: String,
        work: impl FnOnce(WindowAccess) -> Result<WorkerPayload, ErrorCode> + Send + 'static,
    ) -> Result<(), ErrorCode> {
        let key = (access.owner.clone(), key);
        if self.enhance.pending.len() >= 32 && !self.enhance.pending.contains_key(&key) {
            return Err(ErrorCode::Backpressure);
        }
        if (key.1.starts_with("overlay:") || key.1.starts_with("stream-") || key.1 == "annotations")
            && self.enhance.pending.contains_key(&key)
        {
            return Err(ErrorCode::Backpressure);
        }
        let serial = self
            .enhance
            .sequence
            .checked_add(1)
            .ok_or(ErrorCode::QuotaExceeded)?;
        self.enhance.sequence = serial;
        let job_id = Id(serial);
        let binding = request.binding.clone();
        let generation = request.update.generation;
        let ready = self.enhance.ready.clone();
        let client_wake = ClientWake::from_client(&self.client);
        let job_key = key.clone();
        access.background_with(work, move |outcome| {
            let should_wake = {
                let mut guard = ready.lock().unwrap_or_else(PoisonError::into_inner);
                if guard.closed || guard.generation != generation || !binding.is_live() {
                    false
                } else {
                    guard.items.push_back(QueuedReady {
                        key: job_key,
                        job_id,
                        binding,
                        generation,
                        outcome,
                    });
                    true
                }
            };
            if should_wake {
                client_wake.notify();
            }
        })?;
        self.enhance.pending.insert(key, job_id);
        Ok(())
    }
}

struct ClientWake(Option<Arc<dyn Fn() + Send + Sync>>);

impl ClientWake {
    fn from_client(client: &Client) -> Self {
        Self(client.waker())
    }

    fn notify(&self) {
        if let Some(wake) = &self.0 {
            wake();
        }
    }
}
