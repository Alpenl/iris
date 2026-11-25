use super::super::{IrisWindow, kit::bordered_button};
use crate::theme::ThemeTokens;
use gpui::{
    Context, IntoElement, ParentElement, PathPromptOptions, StatefulInteractiveElement, Styled,
    div, px,
};
use iris_api::v2::{ErrorCode, HostEvent, Id, task::MediaInputAction};
use iris_core::{
    plugin_v2::{
        hub::Binding,
        product::tool_catalog,
        runtime::WindowAccess,
        tools::{
            binding::ToolKind,
            catalog::{Receipt, Selection},
        },
    },
    vfs::HttpCancel,
};
use std::{path::PathBuf, sync::Arc, time::Instant};

pub(super) struct InputReview {
    pub binding: Arc<Binding>,
    pub access: WindowAccess,
    pub request: Id,
    pub media: Id,
    pub path: PathBuf,
    pub action: MediaInputAction,
    pub created: Instant,
}
#[derive(Default)]
pub(super) struct ToolView {
    pub busy: bool,
    pub review: Option<(Receipt, Instant)>,
    pub input: Option<InputReview>,
    pub cancel: HttpCancel,
}
impl Drop for ToolView {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl IrisWindow {
    pub(in crate::ui) fn settings_tools(
        &self,
        t: ThemeTokens,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let (records, error) = tool_catalog().summary();
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .pt(px(12.))
            .border_t_1()
            .border_color(t.line)
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(t.text)
                    .child("本地外部工具"),
            )
            .child(div().text_size(px(11.)).text_color(t.text_dim).child(
                "由你提供并确认可信程序。插件仅可请求固定任务；程序本身以本地用户权限运行。",
            ));
        if let Some(error) = error {
            body = body.child(
                div()
                    .text_size(px(11.))
                    .text_color(t.accent)
                    .child(format!("部分工具未加载：{error:?}。重新选择可修复绑定。")),
            );
        }
        for (index, selection) in [
            Selection::Tool(ToolKind::Ffmpeg),
            Selection::Tool(ToolKind::Ffprobe),
            Selection::Tool(ToolKind::WhisperCpp),
            Selection::Tool(ToolKind::YtDlp),
            Selection::WhisperModel,
        ]
        .into_iter()
        .enumerate()
        {
            let record = records.iter().find(|r| r.selection == selection);
            let mut row = div().flex().items_center().gap(px(8.)).child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(px(11.))
                    .text_color(t.text)
                    .child(selection.label()),
            );
            if let Some(record) = record {
                row = row
                    .child(
                        div()
                            .max_w(px(170.))
                            .truncate()
                            .text_size(px(10.))
                            .text_color(t.text_dim)
                            .child(
                                record
                                    .path
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .into_owned(),
                            ),
                    )
                    .child(
                        bordered_button(("remove-tool", index), "移除", false, t).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.remove_tool_binding(selection, cx)
                            }),
                        ),
                    );
            }
            row = row.child(
                bordered_button(
                    ("choose-tool", index),
                    if self.plugin_view.tools.busy {
                        "处理中"
                    } else {
                        "选择文件"
                    },
                    false,
                    t,
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.choose_tool_binding(selection, cx)),
                ),
            );
            body = body.child(row);
        }
        let exports = &self.plugin_view.enhance.exports.records;
        if !exports.is_empty() {
            body = body.child(
                div()
                    .text_size(px(12.))
                    .text_color(t.text)
                    .child("本窗口最近导出"),
            );
            for record in exports.iter().rev().take(8) {
                body = body.child(
                    div()
                        .text_size(px(10.))
                        .text_color(t.text_dim)
                        .child(format!(
                            "{} · {:?} · {}",
                            record.plugin,
                            record.state,
                            record.path.display()
                        )),
                );
            }
        }
        body
    }
    fn choose_tool_binding(&mut self, selection: Selection, cx: &mut Context<Self>) {
        if self.has_host_consent() || self.plugin_view.tools.busy {
            return;
        }
        self.plugin_view.tools.busy = true;
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(format!("选择 {}", selection.label()).into()),
        });
        let cancel = self.plugin_view.tools.cancel.clone();
        cx.spawn(async move |this, cx| {
            let path = prompt
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .and_then(|p| p.into_iter().next());
            let result = if let Some(path) = path {
                cx.background_executor()
                    .spawn(
                        async move { tool_catalog().inspect(selection, &path, &cancel).map(Some) },
                    )
                    .await
            } else {
                Ok(None)
            };
            let _ = this.update(cx, |this, cx| {
                this.plugin_view.tools.busy = false;
                match result {
                    Ok(Some(receipt)) => {
                        this.plugin_view.tools.review = Some((receipt, Instant::now()));
                        this.overlays.close_all();
                    }
                    Ok(None) => {}
                    Err(error) => this.show_toast(format!("工具文件未通过校验 · {error:?}"), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn remove_tool_binding(&mut self, selection: Selection, cx: &mut Context<Self>) {
        if self.plugin_view.tools.busy {
            return;
        }
        self.plugin_view.tools.busy = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { tool_catalog().remove(selection) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.plugin_view.tools.busy = false;
                this.show_toast(
                    match result {
                        Ok(()) => "已移除工具绑定并取消相关任务".into(),
                        Err(e) => format!("未能移除 · {e:?}"),
                    },
                    cx,
                );
            });
        })
        .detach();
    }
    pub(super) fn approve_tool_binding(&mut self, cx: &mut Context<Self>) {
        if self.plugin_view.tools.busy {
            return;
        }
        let Some((receipt, created)) = self.plugin_view.tools.review.take() else {
            return;
        };
        if created.elapsed().as_secs() >= 300 {
            self.show_toast("审核已过期，请重新选择", cx);
            return;
        }
        let Some(root) = iris_core::settings::data_dir() else {
            self.show_toast("工具配置目录不可用", cx);
            return;
        };
        let cancel = self.plugin_view.tools.cancel.clone();
        self.plugin_view.tools.busy = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    tool_catalog().approve(
                        receipt,
                        &root.join("plugin-v2-temp/tool-check"),
                        &cancel,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.plugin_view.tools.busy = false;
                this.show_toast(
                    match result {
                        Ok(()) => "工具已绑定，可供已授权插件使用".into(),
                        Err(e) => format!("工具未绑定 · {e:?}"),
                    },
                    cx,
                );
            });
        })
        .detach();
    }
    pub(super) fn finish_input_consent(&mut self, approve: bool, cx: &mut Context<Self>) {
        let Some(review) = self.plugin_view.tools.input.take() else {
            return;
        };
        let generation = review.access.generation;
        let result = if approve
            && review.created.elapsed().as_secs() < 300
            && review.binding.is_live()
            && generation == self.plugins.generation()
        {
            let path = review.path.clone();
            let media = review.media;
            let action = match review.action {
                MediaInputAction::Slice => iris_core::plugin_v2::files::InputAction::Slice,
                MediaInputAction::ExtractAudio => {
                    iris_core::plugin_v2::files::InputAction::ExtractAudio
                }
            };
            review
                .access
                .background(move |access| {
                    access.check(generation, iris_api::v2::Capability::ExternalTool)?;
                    access.files.input_grants().approve_input_scoped(
                        &path,
                        media,
                        generation,
                        action,
                        &access.cancel,
                    )
                })
                .map(Some)
        } else {
            Ok(None)
        };
        cx.spawn(async move |this, cx| {
            let grant = match result {
                Ok(Some(rx)) => rx
                    .await
                    .unwrap_or(Err(ErrorCode::Cancelled))
                    .map(|r| Some(r.value)),
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            };
            let delivered = this.update(cx, |this, cx| {
                if !review.binding.is_live() || generation != this.plugins.generation() {
                    return false;
                }
                let grant = match grant {
                    Ok(grant) => grant,
                    Err(e) => {
                        this.show_toast(format!("文件未授权 · {e:?}"), cx);
                        None
                    }
                };
                let sent = this.plugins.client().send(
                    &review.binding.installed.plugin_id,
                    HostEvent::MediaInputGranted {
                        request_id: review.request,
                        media: review.media,
                        grant,
                    },
                );
                if let Err(error) = sent {
                    this.show_toast(format!("授权未交付 · {error:?}"), cx);
                }
                sent.is_ok()
            });
            if !delivered.unwrap_or(false)
                && let Ok(Some(id)) = grant
            {
                review.access.files.input_grants().close_input(id);
            }
        })
        .detach();
        cx.notify();
    }
}
