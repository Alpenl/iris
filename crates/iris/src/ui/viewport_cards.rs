//! Viewport child cards: pure-audio playback card, unified empty state with
//! recent `MediaUri` history, and bottom photo thumbnail filmstrip.

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ObjectFit, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, StyledImage, div, img, prelude::FluentBuilder, px,
};
use iris_core::library::{self, Kind};
use iris_core::media_uri::MediaUri;
use iris_core::player::format_time;
use iris_core::preset::SidebarMode;

use super::IrisWindow;
use super::actions::OpenFile;
use super::kit::{badge_pill, icon};
use super::state::BuiltinDialog;
use crate::assets::Icon;
use crate::theme::{FONT_MONO, ThemeTokens};

impl IrisWindow {
    pub(super) fn render_audio_card(&self, t: ThemeTokens) -> impl IntoElement {
        let session = &self.active().session;
        let st = session.state();
        let name = session
            .current()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| session.current_title().map(str::to_string))
            .unwrap_or_else(|| "音频".into());
        let status = if st.is_live {
            format!("● LIVE · {}", if st.paused { "已暂停" } else { "直播中" })
        } else {
            let dur = if st.duration > 0. {
                format_time(st.duration)
            } else {
                "--:--".into()
            };
            format!(
                "{} / {} · {}",
                format_time(st.time_pos),
                dur,
                if st.paused { "已暂停" } else { "播放中" }
            )
        };
        div()
            .absolute()
            .inset_0()
            .bg(t.window_bg)
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(360.))
                    .p(px(20.))
                    .rounded(t.radius_lg)
                    .bg(t.panel)
                    .border_1()
                    .border_color(t.line)
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .size(px(48.))
                            .rounded(t.radius_md)
                            .bg(t.window_bg)
                            .border_1()
                            .border_color(t.line_strong)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(Icon::Music, 22., t.accent)),
                    )
                    .child(
                        div()
                            .max_w(px(320.))
                            .truncate()
                            .text_size(t.text_lg)
                            .text_color(t.text)
                            .child(name),
                    )
                    .child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(t.text_md)
                            .text_color(if st.is_live { t.accent } else { t.text_dim })
                            .child(status),
                    ),
            )
    }

    pub(super) fn render_empty_state(
        &self,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let recents: Vec<(MediaUri, Option<String>, String)> = self
            .store
            .recent()
            .iter()
            .take(6)
            .map(|uri| {
                let stored_title = self.store.recent_title(uri).map(str::to_owned);
                let label = self.store.recent_display_name(uri);
                (uri.clone(), stored_title, label)
            })
            .collect();
        let open_local_key = self.key_hint("open_file");
        let open_url_key = self.key_hint("open_url");
        let confirm_recent = self.is_confirming("clear-recent");

        let cta_btn = |id: &'static str,
                       glyph: Icon,
                       accent_icon: bool,
                       label: &'static str,
                       key: Option<SharedString>| {
            div()
                .id(id)
                .h(t.control_h_md)
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(t.radius_md)
                .bg(t.panel)
                .border_1()
                .border_color(t.line_strong)
                .text_size(t.text_md)
                .cursor_pointer()
                .hover(|s| s.bg(t.hover).border_color(t.accent))
                .active(|s| s.bg(t.selected))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(icon(
                    glyph,
                    13.,
                    if accent_icon { t.accent } else { t.text_dim },
                ))
                .child(label)
                .when_some(key, |d, k| {
                    d.child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(t.text_sm)
                            .text_color(t.text_dim)
                            .child(k),
                    )
                })
        };

        div()
            .absolute()
            .size_full()
            .bg(t.window_bg)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(14.))
            .child(
                div()
                    .text_size(t.text_lg)
                    .text_color(t.text_dim)
                    .child("拖入本地媒体 / 文件夹，或连接网络流与远程数据源"),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_center()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        cta_btn(
                            "open-file",
                            Icon::FolderOpen,
                            false,
                            "打开本地文件…",
                            open_local_key,
                        )
                        .on_click(cx.listener(|this, _, w, cx| this.open_file(&OpenFile, w, cx))),
                    )
                    .child(
                        cta_btn(
                            "open-remote-stream",
                            Icon::Globe,
                            true,
                            super::strings::chrome::OPEN_STREAM_LABEL,
                            open_url_key,
                        )
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.open_builtin_dialog(BuiltinDialog::OpenUrl, w, cx);
                        })),
                    ),
            )
            .child(
                div()
                    .id("browse-sources-link")
                    .px(px(8.))
                    .py(px(2.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .cursor_pointer()
                    .text_size(t.text_sm)
                    .text_color(t.text_faint)
                    .hover(|s| s.text_color(t.text))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(icon(Icon::Bookmark, 11., t.text_faint))
                    .child("管理 WebDAV / 云盘 / 插件数据源 →")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_sidebar_mode(SidebarMode::Sources, cx);
                    })),
            )
            .when(!recents.is_empty(), |d| {
                d.child(
                    div()
                        .mt(px(4.))
                        .w(px(380.))
                        .max_w(gpui::relative(0.92))
                        .rounded(t.radius_lg)
                        .overflow_hidden()
                        .bg(t.panel)
                        .border_1()
                        .border_color(t.line)
                        .flex()
                        .flex_col()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .min_h(t.row_h)
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .justify_between()
                                .border_b_1()
                                .border_color(t.line)
                                .child(
                                    div()
                                        .text_size(t.text_sm)
                                        .text_color(t.text_dim)
                                        .child("最近打开"),
                                )
                                .child(
                                    div()
                                        .id("clear-recent")
                                        .text_size(t.text_xs)
                                        .text_color(if confirm_recent {
                                            t.danger
                                        } else {
                                            t.text_faint
                                        })
                                        .cursor_pointer()
                                        .hover(|s| {
                                            s.text_color(if confirm_recent {
                                                t.danger
                                            } else {
                                                t.text
                                            })
                                        })
                                        .child(if confirm_recent {
                                            "再次点击确认清空"
                                        } else {
                                            "清空"
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if this.confirm_destructive("clear-recent", cx) {
                                                this.store.clear_recent();
                                                this.show_toast("已清除最近打开记录", cx);
                                            }
                                        })),
                                ),
                        )
                        .children(recents.into_iter().enumerate().map(
                            |(i, (uri, stored_title, label))| {
                                let (glyph, badge, remote) = match &uri {
                                    MediaUri::Local(p) => {
                                        let g = match library::kind_of(p) {
                                            Some(Kind::Video) => Icon::Video,
                                            Some(Kind::Photo) => Icon::Photo,
                                            Some(Kind::Audio) => Icon::Music,
                                            None => Icon::Folder,
                                        };
                                        (g, None::<SharedString>, false)
                                    }
                                    MediaUri::Plugin {
                                        plugin_id,
                                        source_id,
                                        ..
                                    } => (
                                        self.plugin_source_icon(plugin_id, source_id),
                                        Some(self.plugin_source_badge(plugin_id, source_id)),
                                        true,
                                    ),
                                    MediaUri::HttpUrl(_) => {
                                        (Icon::Link, Some("网络流".into()), true)
                                    }
                                };
                                div()
                                    .id(("recent-item", i))
                                    .h(t.row_h)
                                    .px(px(10.))
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .cursor_pointer()
                                    .text_size(t.text_md)
                                    .text_color(t.text_muted)
                                    .hover(|s| s.bg(t.hover).text_color(t.text))
                                    .child(icon(
                                        glyph,
                                        13.,
                                        if remote { t.accent } else { t.text_faint },
                                    ))
                                    .child(div().flex_1().min_w(px(0.)).truncate().child(label))
                                    .when_some(badge, |r, b| r.child(badge_pill(b, false, t)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.open_media_uri(&uri, stored_title.clone(), cx);
                                    }))
                            },
                        )),
                )
            })
    }

    pub(super) fn render_photo_filmstrip(
        &self,
        fullscreen: bool,
        t: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let photos = &self.catalog.photo_paths;
        if photos.len() < 2 {
            return None;
        }
        let current = self.active().session.current();
        let cur_ix = self.catalog.filmstrip_index;
        const WINDOW: usize = 11;
        let start = cur_ix
            .saturating_sub(WINDOW / 2)
            .min(photos.len().saturating_sub(WINDOW));
        let slice = &photos[start..(start + WINDOW).min(photos.len())];
        let floating_dock = fullscreen || self.effective_floating_dock();
        let bottom = if floating_dock && self.chrome.chrome_visible {
            f32::from(t.dock_h) + 8.
        } else {
            10.
        };
        Some(
            div()
                .absolute()
                .bottom(px(bottom))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("photo-filmstrip")
                        .p(px(4.))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .rounded(t.radius_lg)
                        .bg(t.overlay)
                        .border_1()
                        .border_color(t.line)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                        .children(slice.iter().enumerate().map(|(offset, path)| {
                            let is_cur = Some(path.as_path()) == current;
                            let p_open = path.clone();
                            let thumb = self.catalog.thumbs.get(path);
                            div()
                                .id(("thumb", start + offset))
                                .w(px(56.))
                                .h(px(38.))
                                .flex_none()
                                .rounded(t.radius_sm)
                                .overflow_hidden()
                                .bg(t.window_bg)
                                .border_1()
                                .border_color(if is_cur { t.accent } else { t.line })
                                .cursor_pointer()
                                .hover(|s| {
                                    s.border_color(if is_cur { t.accent } else { t.text_dim })
                                })
                                .flex()
                                .items_center()
                                .justify_center()
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_path(&p_open, cx)),
                                )
                                .when_some(thumb.clone(), |d, im| {
                                    d.child(img(im).size_full().object_fit(ObjectFit::Cover))
                                })
                                .when(thumb.is_none(), |d| {
                                    d.child(icon(Icon::Photo, 14., t.text_faint))
                                })
                        })),
                ),
        )
    }
}
