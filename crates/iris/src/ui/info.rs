//! Media details & playback stats inspector overlay (`I` / `⌘I`).

use gpui::prelude::FluentBuilder;
use gpui::{
    App, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use iris_core::library::{Kind, format_size};
use iris_core::settings::Side;

use super::AppModel;
use super::kit::{icon_button, panel_header, tip};
use super::views::model_listener;
use crate::assets::Icon;
use crate::theme::{FLOAT_GAP, FONT_MONO, ThemeTokens};

pub(super) fn render_info_overlay(
    model: &Entity<AppModel>,
    fullscreen: bool,
    window: &Window,
    cx: &App,
) -> impl IntoElement {
    let m = model.read(cx);
    let t = ThemeTokens::of(cx);
    let stats = m.active().cached_stats.clone().unwrap_or_default();
    let kind = m.active().session.kind();
    let mut rows: Vec<(&'static str, String)> = Vec::new();

    if !stats.file_name.is_empty() {
        rows.push(("文件", stats.file_name));
    }
    if stats.file_size > 0 {
        rows.push(("大小", format_size(stats.file_size)));
    }
    if !stats.file_format.is_empty() {
        rows.push(("容器", stats.file_format));
    }
    let (w, h) = m.active().session.display_size();
    if w > 0 && h > 0 && kind != Some(Kind::Audio) {
        rows.push(("分辨率", format!("{w} × {h}")));
    }
    match kind {
        Some(Kind::Video) => {
            if !stats.video_codec.is_empty() {
                rows.push(("视频编码", stats.video_codec));
            }
            if !stats.pixel_format.is_empty() {
                rows.push(("像素格式", stats.pixel_format));
            }
            if let Some(fps) = stats.video_fps.filter(|v| *v > 0.) {
                rows.push(("帧率", format!("{fps:.2} fps")));
            }
            if let Some(br) = stats.video_bitrate.filter(|v| *v > 0.) {
                rows.push(("视频码率", format!("{:.2} Mbps", br / 1_000_000.)));
            }
            rows.push((
                "解码器",
                if stats.hwdec.is_empty() || stats.hwdec == "no" {
                    "软件解码".into()
                } else {
                    format!("硬件解码 ({})", stats.hwdec)
                },
            ));
            if let Some(drops) = stats.dropped_frames {
                rows.push(("丢帧", drops.to_string()));
            }
            if !stats.audio_codec.is_empty() {
                let mut audio = stats.audio_codec.to_ascii_uppercase();
                if !stats.audio_channels.is_empty() {
                    audio.push_str(&format!(" · {}", stats.audio_channels));
                }
                if let Some(sr) = stats.audio_sample_rate.filter(|v| *v > 0) {
                    audio.push_str(&format!(" · {:.1} kHz", sr as f64 / 1000.));
                }
                rows.push(("音频", audio));
            }
            let chapters = m.active().session.state().chapters.len();
            if chapters > 0 {
                rows.push(("章节", format!("{chapters} 个")));
            }
        }
        Some(Kind::Audio) => {
            if !stats.audio_codec.is_empty() {
                rows.push(("音频编码", stats.audio_codec.to_ascii_uppercase()));
            }
            if !stats.audio_channels.is_empty() {
                rows.push(("声道", stats.audio_channels));
            }
            if let Some(sr) = stats.audio_sample_rate.filter(|v| *v > 0) {
                rows.push(("采样率", format!("{:.1} kHz", sr as f64 / 1000.)));
            }
            if let Some(br) = stats.audio_bitrate.filter(|v| *v > 0.) {
                rows.push(("码率", format!("{:.0} kbps", br / 1000.)));
            }
        }
        Some(Kind::Photo) => {
            rows.push((
                "色彩空间",
                stats.photo_colour.unwrap_or("sRGB (默认)").into(),
            ));
            if let Some(exif) = stats.photo_exif {
                if let Some(cam) = exif.camera {
                    rows.push(("相机", cam));
                }
                if let Some(lens) = exif.lens {
                    rows.push(("镜头", lens));
                }
                let mut shot = Vec::new();
                if let Some(fl) = exif.focal_length {
                    shot.push(fl);
                }
                if let Some(ap) = exif.aperture {
                    shot.push(ap);
                }
                if let Some(ex) = exif.exposure {
                    shot.push(ex);
                }
                if let Some(iso) = exif.iso {
                    shot.push(format!("ISO {iso}"));
                }
                if !shot.is_empty() {
                    rows.push(("曝光参数", shot.join(" · ")));
                }
                if let Some(date) = exif.date_taken {
                    rows.push(("拍摄时间", date));
                }
            }
        }
        None => {}
    }

    let vp = window.viewport_size();
    let top = if fullscreen { FLOAT_GAP + 36. } else { 12. };
    let panel_w = (f32::from(vp.width) - 24.).clamp(200., 330.);
    let max_h = (f32::from(vp.height) - top - 16.).max(160.);
    let close_btn = icon_button("info-close", Icon::Close, false, t)
        .tooltip(tip("关闭详情", m.key_hint("toggle_info")))
        .on_click(model_listener(model, |this, _, _, cx| {
            this.overlays.info_open = false;
            this.notify_viewport_and_dock(cx);
        }))
        .into_any_element();
    let place_left = if fullscreen {
        m.settings.directory_side == Side::Right
    } else {
        m.settings.directory_side == Side::Left
    };
    let panel_box = div()
        .id("media-info")
        .absolute()
        .top(px(top))
        .w(px(panel_w))
        .max_w(px(panel_w))
        .max_h(px(max_h))
        .flex()
        .flex_col()
        .rounded(t.radius_lg)
        .overflow_hidden()
        .bg(t.overlay)
        .border_1()
        .border_color(t.line_strong)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(panel_header(
            super::strings::chrome::MEDIA_INFO_LABEL,
            None,
            Some(close_btn),
            t,
        ))
        .child(
            div()
                .id("media-info-scroll")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .child(div().p(px(10.)).flex().flex_col().gap(px(6.)).children(
                    rows.into_iter().map(|(k, v)| {
                        div()
                            .flex()
                            .items_baseline()
                            .justify_between()
                            .gap(px(12.))
                            .text_size(t.text_sm)
                            .child(div().flex_none().text_color(t.text_dim).child(k))
                            .child(
                                div()
                                    .min_w(px(0.))
                                    .truncate()
                                    .font_family(FONT_MONO)
                                    .text_color(t.text)
                                    .child(v),
                            )
                    }),
                ))
                .when_some(
                    super::plugin_runtime::annotations::render_personal_annotations(model, t, cx),
                    |view, annotations| view.child(annotations),
                ),
        );

    if place_left {
        panel_box.left(px(if fullscreen { FLOAT_GAP } else { 12. }))
    } else {
        panel_box.right(px(if fullscreen { FLOAT_GAP } else { 12. }))
    }
}
