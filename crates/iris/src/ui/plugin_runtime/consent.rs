use super::super::{
    IrisWindow,
    kit::{
        form::checkbox,
        modal::{ModalCallback, ModalShellSpec, modal_shell},
    },
};
use crate::theme::ThemeTokens;
use gpui::{
    AnyElement, Context, IntoElement, ParentElement, PathPromptOptions, Styled, Window, div, px,
};
use iris_api::v2::NetworkDeclaration;
use std::{path::PathBuf, rc::Rc};

impl IrisWindow {
    pub(in crate::ui) fn has_host_consent(&self) -> bool {
        self.plugin_view.review.is_some()
            || self.plugin_view.origin.is_some()
            || self.plugin_view.tools.review.is_some()
            || self.plugin_view.tools.input.is_some()
    }
    pub(in crate::ui) fn review_plugin_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if path
            .extension()
            .is_none_or(|e| !e.eq_ignore_ascii_case("irisplugin"))
        {
            self.show_toast("请选择 .irisplugin 插件包", cx);
            return;
        }
        match self.plugins.client().review(path) {
            Ok(_) => self.show_toast("正在验证插件包…", cx),
            Err(error) => self.show_toast(format!("无法审核插件包 · {error:?}"), cx),
        }
    }
    pub(in crate::ui) fn choose_plugin_package(&mut self, cx: &mut Context<Self>) {
        if self.has_host_consent() {
            return;
        }
        let request = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择 .irisplugin".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = request.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| this.review_plugin_path(path, cx));
            }
        })
        .detach();
    }
    pub(in crate::ui) fn cancel_host_consent(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only the visible sheet is dismissed. A separate pending sheet stays pending.
        if self.plugin_view.tools.review.take().is_some() {
        } else if self.plugin_view.tools.input.is_some() {
            self.finish_input_consent(false, cx);
        } else if let Some(origin) = self.plugin_view.origin.take() {
            let _ = self
                .plugins
                .client()
                .confirm_origin(origin.id, false, false);
            if self.plugin_view.consent_busy == Some(origin.id) {
                self.plugin_view.consent_busy = None;
            }
        } else if let Some(review) = self.plugin_view.review.take() {
            let _ = self.plugins.client().dismiss_review(review.id);
            self.plugin_view.review_created = None;
            if self.plugin_view.consent_busy == Some(review.id) {
                self.plugin_view.consent_busy = None;
            }
        }
        self.focus.focus(window);
        cx.notify();
    }
    fn approve_host_consent(&mut self, cx: &mut Context<Self>) {
        let (target_id, result) = if let Some(origin) = &self.plugin_view.origin {
            if self.plugin_view.consent_busy == Some(origin.id) {
                return;
            }
            (
                origin.id,
                self.plugins.client().confirm_origin(
                    origin.id,
                    true,
                    self.plugin_view.insecure_auth,
                ),
            )
        } else if let Some(review) = &self.plugin_view.review {
            if self.plugin_view.consent_busy == Some(review.id) {
                return;
            }
            (
                review.id,
                self.plugins.client().approve(
                    review.id,
                    review.manifest.permissions.iter().copied().collect(),
                    self.plugin_view.keep_identity,
                ),
            )
        } else {
            return;
        };
        match result {
            Ok(()) => self.plugin_view.consent_busy = Some(target_id),
            Err(error) => self.show_toast(format!("授权未提交 · {error:?}"), cx),
        }
        cx.notify();
    }
    pub(in crate::ui) fn render_host_consent(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if let Some(consent) = self.render_tool_consent(window, cx) {
            return Some(consent);
        }
        if !self.has_host_consent() {
            return None;
        }
        // Authorization uses native colors, unaffected by a plugin's selected theme.
        let t = ThemeTokens::resolve("graphite", None);
        let line = |text: String| div().text_size(px(12.)).text_color(t.text).child(text);
        let mut body = div().flex().flex_col().gap(px(10.));
        let title;
        if let Some(origin) = &self.plugin_view.origin {
            title = "Iris · 确认网络服务";
            body = body.child(line(format!("插件：{} ({})", origin.binding.manifest.name, origin.binding.installed.plugin_id)))
                .child(line(format!("服务地址：{}", origin.origin)))
                .child(line(format!("固定连接地址：{}", origin.ips.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))))
                .child(line(if origin.private { "这是私有或本机服务。授权只适用于此 Origin 与列出的地址，不授予整个内网访问。" } else { "授权只适用于此 Origin 与列出的地址。地址变化后需要重新确认。" }.into()));
            if let Some(old) = &origin.previous {
                body = body.child(line(format!(
                    "原固定地址：{}。重新确认将撤销旧授权和在途请求。",
                    old.ips
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            if origin.plaintext {
                let origin_id = origin.id;
                body = body.child(checkbox(
                    "host-origin-http-auth",
                    self.plugin_view.insecure_auth,
                    "允许向此 HTTP 服务发送凭据（传输未加密）",
                    t,
                    cx.listener(move |this, _, _, cx| {
                        if this.plugin_view.consent_busy != Some(origin_id) {
                            this.plugin_view.insecure_auth = !this.plugin_view.insecure_auth;
                            cx.notify();
                        }
                    }),
                ));
            }
        } else {
            let review = self.plugin_view.review.as_ref()?;
            title = "Iris · 安装权限审核";
            body = body
                .child(line(format!(
                    "{} · {} · v{}",
                    review.manifest.name, review.manifest.id, review.manifest.version
                )))
                .child(line(format!(
                    "包摘要：{}",
                    review
                        .digest
                        .iter()
                        .map(|v| format!("{v:02x}"))
                        .collect::<String>()
                )))
                .child(line("安装将授予以下能力：".into()));
            for cap in &review.manifest.permissions {
                body = body.child(line(format!("• {}", cap.label())));
            }
            for rule in network_lines(&review.manifest.network) {
                body = body.child(line(rule));
            }
            if let Some(previous) = &review.previous {
                let review_id = review.id;
                body = body
                    .child(line(format!(
                        "将替换已安装的 {}。默认创建新的私有存储身份。",
                        previous.plugin_id
                    )))
                    .child(checkbox(
                        "host-plugin-keep-identity",
                        self.plugin_view.keep_identity,
                        "信任此更新继续使用原插件状态、凭据与已确认服务",
                        t,
                        cx.listener(move |this, _, _, cx| {
                            if this.plugin_view.consent_busy != Some(review_id) {
                                this.plugin_view.keep_identity = !this.plugin_view.keep_identity;
                                cx.notify();
                            }
                        }),
                    ));
            } else if let Some(archived) = &review.archived {
                let review_id = review.id;
                let prev_digest: String = archived
                    .digest
                    .iter()
                    .take(8)
                    .map(|v| format!("{v:02x}"))
                    .collect();
                body = body
                    .child(line(format!(
                        "检测到此前删除 {} 时保留的登录凭据与设置（原包摘要前缀：{prev_digest}…）。默认创建新的独立存储身份。",
                        archived.plugin_id
                    )))
                    .child(checkbox(
                        "host-plugin-keep-identity",
                        self.plugin_view.keep_identity,
                        "信任此插件包并恢复此前保留的登录信息与设置",
                        t,
                        cx.listener(move |this, _, _, cx| {
                            if this.plugin_view.consent_busy != Some(review_id) {
                                this.plugin_view.keep_identity = !this.plugin_view.keep_identity;
                                cx.notify();
                            }
                        }),
                    ));
            }
        }
        let active_id = self
            .plugin_view
            .origin
            .as_ref()
            .map(|o| o.id)
            .or_else(|| self.plugin_view.review.as_ref().map(|r| r.id));
        let busy = active_id.is_some() && self.plugin_view.consent_busy == active_id;
        let cancel: ModalCallback =
            Rc::new(|this, window, cx| this.cancel_host_consent(window, cx));
        let submit: ModalCallback = Rc::new(|this, _, cx| this.approve_host_consent(cx));
        let viewport = window.viewport_size();
        Some(modal_shell(
            ModalShellSpec::dialog(
                "host-plugin-consent",
                title,
                (f32::from(viewport.width) - 24.).clamp(0., 620.),
                (f32::from(viewport.height) - 24.).clamp(0., 620.),
                "取消",
                if busy {
                    "正在提交…"
                } else {
                    "确认授权"
                },
                body.into_any_element(),
                cancel,
                submit,
            ),
            t,
            cx,
        ))
    }
}

fn network_lines(network: &NetworkDeclaration) -> Vec<String> {
    let mut lines = Vec::new();
    if network.net_public {
        lines.push(format!(
            "任意公网访问；端口 {:?}。私网、回环和链路本地地址仍需单独确认。",
            network.public_ports
        ));
    }
    for rule in &network.net_hosts {
        lines.push(format!(
            "允许域名 {}；协议 {}；端口 {:?}",
            rule.host,
            rule.schemes.join(", "),
            rule.ports
        ));
    }
    for rule in &network.tcp_targets {
        lines.push(format!(
            "允许 TCP 目标 {}；端口 {:?}",
            rule.host, rule.ports
        ));
    }
    lines
}
