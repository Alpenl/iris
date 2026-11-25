use super::{
    super::{
        IrisWindow,
        kit::modal::{ModalCallback, ModalShellSpec, modal_shell},
    },
    byob::InputReview,
};
use crate::theme::ThemeTokens;
use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, Window, div, px};
use iris_api::v2::{Capability, ErrorCode, HostEvent, Id, task::MediaInputAction};
use iris_core::plugin_v2::{runtime::WindowAccess, ui::Action};
use std::{rc::Rc, time::Instant};
impl IrisWindow {
    pub(super) fn render_tool_consent(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let t = ThemeTokens::resolve("graphite", None);
        let line = |s: String| div().text_size(px(12.)).text_color(t.text).child(s);
        let mut body = div().flex().flex_col().gap(px(10.));
        let title;
        let submit: ModalCallback;
        if let Some((receipt, _)) = &self.plugin_view.tools.review {
            title = "Iris · 信任本地工具";
            body=body.child(line(receipt.selection.label().into()))
                .child(line(receipt.path.display().to_string()))
                .child(line(format!("SHA256：{}",receipt.sha256.iter().map(|b|format!("{b:02x}")).collect::<String>())))
                .child(line(format!("同时绑定 {} 个辅助文件。摘要仅用于识别文件，请自行核对程序来源及版本。",receipt.components.len())))
                .child(line("确认后允许 Iris 执行版本检查和固定媒体任务；这些程序具有本地用户权限，不受 Wasm 沙箱隔离。文件变化后需要重新确认。".into()));
            submit = Rc::new(|this, _, cx| this.approve_tool_binding(cx));
        } else if let Some(input) = &self.plugin_view.tools.input {
            title = "Iris · 授权当前媒体";
            body = body
                .child(line(format!("插件：{}", input.binding.manifest.name)))
                .child(line(input.path.display().to_string()))
                .child(line(format!(
                    "操作：{}。授权只可使用一次，五分钟后过期；切换媒体或停用插件会撤销。",
                    match input.action {
                        MediaInputAction::Slice => "无损切片",
                        MediaInputAction::ExtractAudio => "提取音轨",
                    }
                )));
            submit = Rc::new(|this, _, cx| this.finish_input_consent(true, cx));
        } else {
            return None;
        }
        let cancel: ModalCallback =
            Rc::new(|this, window, cx| this.cancel_host_consent(window, cx));
        let vp = window.viewport_size();
        Some(modal_shell(
            ModalShellSpec::dialog(
                "host-tool-consent",
                title,
                (f32::from(vp.width) - 24.).clamp(0., 620.),
                (f32::from(vp.height) - 24.).clamp(0., 620.),
                "取消",
                "确认授权",
                body.into_any_element(),
                cancel,
                submit,
            ),
            t,
            cx,
        ))
    }
    pub(super) fn request_media_input(
        &mut self,
        action: &Action,
        access: WindowAccess,
        request: Id,
        input_action: MediaInputAction,
        cx: &mut Context<Self>,
    ) -> Result<(), ErrorCode> {
        access.check(action.update.generation, Capability::ExternalTool)?;
        let path = self
            .active()
            .session
            .current()
            .map(std::path::Path::to_owned);
        let media = self.plugin_media_id();
        if self.has_host_consent() || path.is_none() || self.plugin_view.tools.busy {
            let report = self.plugins.send_to(
                &action.binding.installed.plugin_id,
                HostEvent::MediaInputGranted {
                    request_id: request,
                    media,
                    grant: None,
                },
            );
            self.apply_poll_report(report, None, cx);
            return Ok(());
        }
        self.plugin_view.tools.input = Some(InputReview {
            binding: action.binding.clone(),
            access,
            request,
            media,
            path: path.ok_or(ErrorCode::Denied)?,
            action: input_action,
            created: Instant::now(),
        });
        self.overlays.close_all();
        cx.notify();
        Ok(())
    }
    pub(in crate::ui) fn bind_plugin_current_file(&self) {
        let path = self
            .active()
            .session
            .current()
            .map(std::path::Path::to_owned);
        for info in self.plugins.list() {
            if let Ok(access) = self.plugins.client().access(&info.id) {
                let _ = access.files.set_current_path(
                    self.plugin_media_id(),
                    access.generation,
                    path.clone(),
                );
            }
        }
    }
}
