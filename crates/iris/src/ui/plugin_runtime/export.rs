use super::super::IrisWindow;
use gpui::Context;
use iris_api::v2::{Capability, ErrorCode, HostEvent, Id, enhance::ExportFormat};
use iris_core::plugin_v2::{
    actor::Executor,
    files::{ExportAudit, ExportGrant, ExportState, export_extension},
    runtime::WindowAccess,
    tools::ToolExecutor,
    ui::Action,
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

pub(super) struct ExportRecord {
    pub path: PathBuf,
    pub plugin: String,
    pub state: ExportState,
    audit: ExportAudit,
    grant: Id,
    executor: Option<ToolExecutor>,
    created: Instant,
}
#[derive(Default)]
pub(super) struct ExportView {
    pub records: Vec<ExportRecord>,
    pub dialog: bool,
    pub timer: Option<gpui::Task<()>>,
}
impl IrisWindow {
    pub(super) fn request_plugin_export(
        &mut self,
        action: &Action,
        access: WindowAccess,
        request: Id,
        format: ExportFormat,
        suggested: String,
        cx: &mut Context<Self>,
    ) -> Result<(), ErrorCode> {
        access.check(action.update.generation, Capability::ExportFiles)?;
        let view = &mut self.plugin_view.enhance.exports;
        if view.dialog
            || view
                .records
                .iter()
                .filter(|r| matches!(r.state, ExportState::Pending | ExportState::Writing))
                .count()
                >= 8
        {
            return Err(ErrorCode::Backpressure);
        }
        let name = safe_suggestion(&suggested, format);
        let directory = self
            .active()
            .session
            .current()
            .and_then(|p| p.parent())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let dialog = cx.prompt_for_new_path(&directory, Some(&name));
        self.plugin_view.enhance.exports.dialog = true;
        let binding = action.binding.clone();
        let generation = access.generation;
        cx.spawn(async move |this, cx| {
            let path = dialog.await.ok().and_then(Result::ok).flatten();
            let approved = if let Some(path) = path {
                let output_path = path.clone();
                match access.background(move |access| {
                    access.check(generation, Capability::ExportFiles)?;
                    let export = ExportGrant::approve(
                        &output_path,
                        format,
                        access.owner.clone(),
                        generation,
                        &access.resources.ledger,
                    )?;
                    let audit = export.audit();
                    let grant = access.files.add_export(export)?;
                    Ok((grant, audit, access.files.clone()))
                }) {
                    Ok(receipt) => receipt
                        .await
                        .unwrap_or(Err(ErrorCode::Cancelled))
                        .map(|out| (path, out.value)),
                    Err(error) => Err(error),
                }
                .map(Some)
            } else {
                Ok(None)
            };
            let _ = this.update(cx, |this, cx| {
                this.plugin_view.enhance.exports.dialog = false;
                if !binding.is_live() || generation != this.plugins.generation() {
                    if let Ok(Some((_, (grant, _, files)))) = approved {
                        let _ = files.close(grant.0);
                    }
                    return;
                }
                let grant = match approved {
                    Ok(Some((path, (grant, audit, executor)))) => {
                        let records = &mut this.plugin_view.enhance.exports.records;
                        if records.len() >= 32
                            && let Some(old) = records.iter().position(|r| {
                                !matches!(r.state, ExportState::Pending | ExportState::Writing)
                            })
                        {
                            records.remove(old);
                        }
                        records.push(ExportRecord {
                            path,
                            plugin: binding.manifest.name.clone(),
                            state: audit.state(),
                            audit,
                            grant,
                            executor: Some(executor),
                            created: Instant::now(),
                        });
                        this.watch_plugin_exports(cx);
                        Some(grant)
                    }
                    Ok(None) => None,
                    Err(error) => {
                        this.show_toast(format!("导出路径未授权 · {error:?}"), cx);
                        None
                    }
                };
                let report = this.plugins.send_to(
                    &binding.installed.plugin_id,
                    HostEvent::ExportGranted {
                        request_id: request,
                        grant,
                    },
                );
                if let Some(grant_id) = grant
                    && !report.notices.is_empty()
                    && let Some(record) = this
                        .plugin_view
                        .enhance
                        .exports
                        .records
                        .iter_mut()
                        .rfind(|r| r.grant == grant_id)
                    && let Some(executor) = record.executor.take()
                {
                    let _ = executor.close(grant_id.0);
                    record.state = record.audit.state();
                }
                this.apply_poll_report(report, None, cx);
                cx.notify();
            });
        })
        .detach();
        Ok(())
    }
    fn watch_plugin_exports(&mut self, cx: &mut Context<Self>) {
        if self.plugin_view.enhance.exports.timer.is_some() {
            return;
        }
        let executor = cx.background_executor().clone();
        self.plugin_view.enhance.exports.timer = Some(cx.spawn(async move |this, cx| {
            loop {
                executor.timer(Duration::from_millis(200)).await;
                let active = this
                    .update(cx, |this, cx| {
                        let mut messages = Vec::new();
                        let mut active = false;
                        for record in &mut this.plugin_view.enhance.exports.records {
                            if record.audit.state() == ExportState::Pending
                                && record.created.elapsed() >= Duration::from_secs(300)
                                && let Some(executor) = record.executor.take()
                            {
                                let _ = executor.close(record.grant.0);
                            }
                            let next = record.audit.state();
                            if record.state != next {
                                record.state = next;
                                if next == ExportState::Committed {
                                    messages.push(format!(
                                        "{} 导出完成 · {}",
                                        record.plugin,
                                        record
                                            .path
                                            .file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy()
                                    ));
                                } else if let ExportState::Failed(error) = next {
                                    messages.push(format!("导出未完成 · {error:?}"));
                                }
                                cx.notify();
                            }
                            if matches!(next, ExportState::Pending | ExportState::Writing) {
                                active = true;
                            } else {
                                record.executor = None;
                            }
                        }
                        for message in messages {
                            this.show_toast(message, cx);
                        }
                        if !active {
                            this.plugin_view.enhance.exports.timer = None;
                        }
                        active
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        }));
    }
}
fn safe_suggestion(name: &str, format: ExportFormat) -> String {
    let extension = export_extension(format);
    let stem: String = name
        .rsplit_once('.')
        .map_or(name, |(stem, _)| stem)
        .chars()
        .filter(|c| !c.is_control() && !":/\\<>\"|?*".contains(*c))
        .take(100)
        .collect();
    let stem = stem.trim_matches(['.', ' ']);
    format!(
        "{}.{}",
        if stem.is_empty() { "export" } else { stem },
        extension
    )
}
