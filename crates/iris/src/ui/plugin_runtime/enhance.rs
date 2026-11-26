use super::super::IrisWindow;
use gpui::Context;
use iris_api::v2::{ErrorCode, Generation, HostCommand, HostEvent, Id};
use iris_core::plugin_v2::{
    enhancement::{Attachment, AttachmentSpec},
    hub::Binding,
    lut::MonitorLut,
    overlay::{OverlayBank, OverlayClock, OverlayFrame},
    runtime::WindowAccess,
    ui::Action,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub(super) struct EnhancementView {
    pub sequence: u64,
    pub media_announced: Option<Id>,
    pub announced: BTreeMap<String, Generation>,
    pub bindings: BTreeMap<String, Arc<Binding>>,
    pub pending: BTreeMap<(String, String), Id>,
    pub lut_owner: Option<String>,
    pub timeline: Option<(String, Arc<iris_core::plugin_v2::actor::Update>, usize)>,
    pub bank: Option<Arc<Mutex<OverlayBank>>>,
    pub clock: OverlayClock,
    pub overlay_generation: Generation,
    pub frame: Option<Arc<OverlayFrame>>,
    pub wake: Option<gpui::Task<()>>,
    pub glyphs: super::overlay::GlyphCache,
    pub ledger: Option<iris_core::plugin_v2::ledger::Ledger>,
    pub annotations: super::annotations::AnnotationView,
}
impl EnhancementView {
    fn cancel_lut(&mut self, owner: &str) -> bool {
        // A clear also supersedes preparation of a first LUT, or a replacement
        // while another plugin still owns the currently applied filter.
        self.pending.remove(&(owner.into(), "lut".into()));
        self.lut_owner.as_deref() == Some(owner)
    }

    fn finish_job(&mut self, key: &(String, String), id: Id) -> bool {
        if self.pending.get(key) != Some(&id) {
            return false;
        }
        self.pending.remove(key);
        true
    }
}
pub(super) enum Effect {
    Track(Arc<Attachment>),
    Lut(Arc<MonitorLut>),
    Done,
    Stream,
    Annotations(
        Option<iris_core::store::AnnotationSnapshot>,
        Box<iris_core::plugin_v2::ui::Event>,
    ),
}
impl IrisWindow {
    pub(in crate::ui) fn apply_plugin_enhancement(
        &mut self,
        action: &Action,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.apply_enhancement(action, cx) {
            self.annotation_error(action, error, cx);
            if matches!(
                error,
                ErrorCode::StaleHandle | ErrorCode::Cancelled | ErrorCode::OutOfWindow
            ) {
                log::debug!("ignoring superseded plugin enhancement action: {error:?}");
            } else {
                log::warn!("plugin enhancement action failed: {error:?}");
                self.show_toast(
                    format!(
                        "插件操作未完成 · {}",
                        super::notices::describe_error_code(error)
                    ),
                    cx,
                );
            }
        }
    }
    fn apply_enhancement(
        &mut self,
        action: &Action,
        cx: &mut Context<Self>,
    ) -> Result<(), ErrorCode> {
        let command = action.command().clone();
        let access = self
            .plugins
            .client()
            .access(&action.binding.installed.plugin_id)?;
        let owner = access.owner.clone();
        self.plugin_view
            .enhance
            .bindings
            .insert(owner.clone(), action.binding.clone());
        match command {
            HostCommand::SetTimelineMeta {
                media,
                generation,
                chapters,
                ..
            } => {
                self.check_enhancement_media(media, generation)?;
                let core_chapters: Vec<_> = chapters
                    .iter()
                    .map(|c| iris_core::player::Chapter {
                        time: c.start_ms.0 as f64 / 1000.,
                        title: c.title.clone(),
                    })
                    .collect();
                self.active_mut()
                    .session
                    .set_plugin_timeline(owner.clone(), &core_chapters)
                    .map_err(|_| ErrorCode::BadState)?;
                self.plugin_view.enhance.timeline =
                    Some((owner, action.update.clone(), action.index));
            }
            HostCommand::SetMonitorLut {
                media,
                generation,
                artifact,
            } => {
                self.check_enhancement_media(media, generation)?;
                if let Some(id) = artifact {
                    self.enhancement_job(
                        action,
                        access,
                        "lut".into(),
                        move |access| {
                            let artifact =
                                access
                                    .files
                                    .approved_artifact(&access.owner, id, generation)?;
                            Ok(Effect::Lut(MonitorLut::prepare(
                                artifact,
                                &access.resources.ledger,
                                &access.cancel,
                            )?))
                        },
                        cx,
                    )?;
                } else if self.plugin_view.enhance.cancel_lut(&owner) {
                    self.active_mut()
                        .session
                        .set_monitor_lut(None)
                        .map_err(|_| ErrorCode::IoError)?;
                    self.plugin_view.enhance.lut_owner = None;
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
                self.check_enhancement_media(media, generation)?;
                self.enhancement_job(
                    action,
                    access,
                    format!("track:{id}"),
                    move |access| {
                        Ok(Effect::Track(access.prepare_attachment(
                            AttachmentSpec {
                                id,
                                label,
                                lang,
                                subtitle: Some(format),
                                resource,
                            },
                        )?))
                    },
                    cx,
                )?;
            }
            HostCommand::AttachAudioTrack {
                media,
                generation,
                id,
                label,
                lang,
                resource,
            } => {
                self.check_enhancement_media(media, generation)?;
                self.enhancement_job(
                    action,
                    access,
                    format!("track:{id}"),
                    move |access| {
                        Ok(Effect::Track(access.prepare_attachment(
                            AttachmentSpec {
                                id,
                                label,
                                lang,
                                subtitle: None,
                                resource,
                            },
                        )?))
                    },
                    cx,
                )?;
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
                self.check_overlay_media(media, generation)?;
                let append = matches!(action.command(), HostCommand::AppendTimedCues { .. });
                let bank = self.ensure_overlay_bank(&access);
                let blob = action
                    .update
                    .blobs
                    .get(&cues)
                    .ok_or(ErrorCode::StaleHandle)?
                    .data
                    .clone();
                // Append order is preserved in `enhancement_job`: an outstanding mutation
                // of this track produces backpressure instead of racing another append.
                self.enhancement_job(
                    action,
                    access,
                    format!("overlay:{track}"),
                    move |access| {
                        bank.lock().unwrap_or_else(|e| e.into_inner()).submit(
                            &access.owner,
                            media,
                            generation,
                            &track,
                            window_revision,
                            &blob,
                            append,
                        )?;
                        Ok(Effect::Done)
                    },
                    cx,
                )?;
            }
            HostCommand::ClearTimedOverlay {
                media,
                generation,
                track,
            } => {
                self.check_overlay_media(media, generation)?;
                let bank = self.ensure_overlay_bank(&access);
                self.enhancement_job(
                    action,
                    access,
                    format!("overlay:{track}"),
                    move |access| {
                        bank.lock().unwrap_or_else(|e| e.into_inner()).clear(
                            &access.owner,
                            media,
                            generation,
                            &track,
                        )?;
                        Ok(Effect::Done)
                    },
                    cx,
                )?;
            }
            HostCommand::UpdateManifest {
                session,
                expected_revision,
                format,
                document,
            } => {
                let current =
                    iris_core::plugin_v2::stream::prepare::stream_session(&self.active().session)
                        .filter(|s| {
                            s.id == session
                                && s.factory.owner() == owner
                                && s.allows_guest_manifest_updates()
                        })
                        .ok_or(ErrorCode::Denied)?;
                let blob = action
                    .update
                    .blobs
                    .get(&document)
                    .ok_or(ErrorCode::StaleHandle)?
                    .data
                    .clone();
                self.enhancement_job(
                    action,
                    access,
                    "stream-manifest".into(),
                    move |_| {
                        current.commit(current.prepare_manifest(
                            expected_revision,
                            format,
                            &blob.bytes,
                        )?)?;
                        Ok(Effect::Stream)
                    },
                    cx,
                )?;
            }
            HostCommand::UpdateStreamContext {
                session,
                expected_version,
                context,
            } => {
                let current =
                    iris_core::plugin_v2::stream::prepare::stream_session(&self.active().session)
                        .filter(|s| s.id == session && s.factory.owner() == owner)
                        .ok_or(ErrorCode::Denied)?;
                let blob = action
                    .update
                    .blobs
                    .get(&context.blob)
                    .ok_or(ErrorCode::StaleHandle)?
                    .data
                    .clone();
                self.enhancement_job(
                    action,
                    access,
                    "stream-context".into(),
                    move |_| {
                        current.refresh_context(expected_version, &context, blob)?;
                        Ok(Effect::Stream)
                    },
                    cx,
                )?;
            }
            HostCommand::EditAnnotation {
                request_id,
                selection,
                edit,
            } => {
                self.plugin_annotation(
                    action,
                    access,
                    request_id,
                    selection,
                    Some(edit),
                    (0, 0),
                    cx,
                )?;
            }
            HostCommand::ReadAnnotation {
                request_id,
                selection,
                index,
                marker_offset,
            } => {
                self.plugin_annotation(
                    action,
                    access,
                    request_id,
                    selection,
                    None,
                    (index, marker_offset),
                    cx,
                )?;
            }
            // Source/Origin and declarative commands are consumed before this layer.
            _ => return Err(ErrorCode::BadState),
        }
        cx.notify();
        Ok(())
    }
    fn check_overlay_media(&self, media: Id, generation: Generation) -> Result<(), ErrorCode> {
        if media != self.plugin_media_id()
            || generation != self.plugin_view.enhance.overlay_generation
        {
            return Err(ErrorCode::StaleHandle);
        }
        Ok(())
    }
    fn check_enhancement_media(&self, media: Id, generation: Generation) -> Result<(), ErrorCode> {
        if media != self.plugin_media_id() || generation != self.plugins.generation() {
            return Err(ErrorCode::StaleHandle);
        }
        if !self.active().session.state().loaded {
            return Err(ErrorCode::BadState);
        }
        Ok(())
    }
    pub(super) fn ensure_overlay_bank(&mut self, access: &WindowAccess) -> Arc<Mutex<OverlayBank>> {
        let position = (self.active().session.state().time_pos.max(0.) * 1000.) as u64;
        self.plugin_view.enhance.ledger = Some(access.resources.ledger.clone());
        self.plugin_view
            .enhance
            .bank
            .get_or_insert_with(|| {
                Arc::new(Mutex::new(OverlayBank::new(
                    access.resources.ledger.clone(),
                    self.plugin_view.enhance.overlay_generation.media,
                    self.plugin_view.enhance.overlay_generation,
                    position,
                )))
            })
            .clone()
    }
    pub(super) fn enhancement_job(
        &mut self,
        action: &Action,
        access: WindowAccess,
        key: String,
        work: impl FnOnce(WindowAccess) -> Result<Effect, ErrorCode> + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Result<(), ErrorCode> {
        let key = (access.owner.clone(), key);
        if self.plugin_view.enhance.pending.len() >= 32
            && !self.plugin_view.enhance.pending.contains_key(&key)
        {
            return Err(ErrorCode::Backpressure);
        }
        if (key.1.starts_with("overlay:") || key.1.starts_with("stream-") || key.1 == "annotations")
            && self.plugin_view.enhance.pending.contains_key(&key)
        {
            return Err(ErrorCode::Backpressure);
        }
        let serial = self
            .plugin_view
            .enhance
            .sequence
            .checked_add(1)
            .ok_or(ErrorCode::QuotaExceeded)?;
        self.plugin_view.enhance.sequence = serial;
        let id = Id(serial);
        let binding = action.binding.clone();
        let generation = action.update.generation;
        let receipt = access.background(work)?;
        self.plugin_view.enhance.pending.insert(key.clone(), id);
        cx.spawn(async move |this, cx| {
            let result = receipt.await.unwrap_or(Err(ErrorCode::Cancelled));
            let _ = this.update(cx, |this, cx| {
                if !this.plugin_view.enhance.finish_job(&key, id) {
                    return;
                }
                if !binding.is_live() || generation != this.plugins.generation() {
                    return;
                }
                match result.map(|out| out.value) {
                    Ok(Effect::Track(track)) => {
                        if let Err(error) = this.active_mut().session.attach_plugin_track(track) {
                            this.show_toast(format!("媒体轨道添加失败 · {error}"), cx);
                        }
                    }
                    Ok(Effect::Lut(lut)) => {
                        match this.active_mut().session.set_monitor_lut(Some(lut)) {
                            Ok(()) => this.plugin_view.enhance.lut_owner = Some(key.0),
                            Err(error) => this.show_toast(format!("LUT 未应用 · {error}"), cx),
                        }
                    }
                    Ok(Effect::Stream) => {
                        this.notify_stream_status(&binding.installed.plugin_id, None, cx)
                    }
                    Ok(Effect::Annotations(snapshot, event)) => {
                        if let Some(snapshot) = snapshot {
                            let _ = this.store.apply_annotation_snapshot(snapshot);
                        }
                        let report = this.plugins.send_to(&binding.installed.plugin_id, *event);
                        this.apply_poll_report(report, None, cx);
                    }
                    Ok(Effect::Done) => {
                        this.plugin_view.enhance.wake = None;
                    }
                    Err(error)
                        if matches!(
                            error,
                            ErrorCode::StaleHandle | ErrorCode::Cancelled | ErrorCode::OutOfWindow
                        ) =>
                    {
                        log::debug!("ignoring superseded plugin enhancement job: {error:?}");
                    }
                    Err(error) => {
                        log::warn!("plugin enhancement job failed: {error:?}");
                        this.show_toast(
                            format!(
                                "插件操作未完成 · {}",
                                super::notices::describe_error_code(error)
                            ),
                            cx,
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
        Ok(())
    }
    pub(in crate::ui) fn notify_stream_status(
        &mut self,
        plugin: &str,
        request_id: Option<Id>,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) =
            iris_core::plugin_v2::stream::prepare::stream_session(&self.active().session)
        {
            let generation = session.generation();
            let event = HostEvent::StreamStatus {
                request_id,
                session: session.id,
                generation,
                manifest_revision: session.revision(),
                context_version: generation.context,
            };
            let report = self.plugins.send_to(plugin, event);
            self.apply_poll_report(report, None, cx);
        }
    }
}

#[cfg(test)]
mod tests;
