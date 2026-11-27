//! "More" visual tools popover (aspect ratio via `kit::form::segmented`,
//! 4-channel video EQ via `kit::form::stepper`, transform/screenshot rows via
//! `kit::modal::menu_row`, and chapter list) plus titlebar workspace presets.

use gpui::{
    AnyElement, App, Context, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use iris_core::library::Kind;
use iris_core::player::{AspectMode, Chapter, Command, format_time};
use iris_core::preset::PresetCaptureScope;

use super::super::AppModel;
use super::super::actions::{RotateCw, TakeScreenshot, ToggleHFlip, ToggleInfo, ToggleVFlip};
use super::super::kit::form::{segmented, stepper};
use super::super::kit::list_row::select_row;
use super::super::kit::modal::menu_row;
use super::super::kit::{
    ButtonSize, ButtonVariant, badge_pill, button, hbetween, hstack, icon, vstack,
};
use super::super::state::{BuiltinDialog, SettingsTab};
use super::super::views::{DirtyRegions, model_listener};
use super::popover_header;
use crate::assets::Icon;
use crate::theme::ThemeTokens;

#[derive(Clone, Copy)]
enum EqChannel {
    Brightness,
    Contrast,
    Saturation,
    Gamma,
}

impl EqChannel {
    fn label(self) -> &'static str {
        match self {
            EqChannel::Brightness => "亮度",
            EqChannel::Contrast => "对比度",
            EqChannel::Saturation => "饱和度",
            EqChannel::Gamma => "伽马",
        }
    }
}

const ASPECT_OPTIONS: &[(&str, AspectMode)] = &[
    ("原始", AspectMode::Original),
    ("16:9", AspectMode::Ratio16x9),
    ("4:3", AspectMode::Ratio4x3),
    ("21:9", AspectMode::Ratio21x9),
    ("裁剪", AspectMode::FillCrop),
];

impl AppModel {
    fn adjust_video_eq(&mut self, ch: EqChannel, next_val: i64, cx: &mut Context<Self>) {
        let clamped = next_val.clamp(-100, 100);
        let s = self.session().state();
        let (mut b, mut c, mut sat, mut g) =
            (s.eq_brightness, s.eq_contrast, s.eq_saturation, s.eq_gamma);
        match ch {
            EqChannel::Brightness => b = clamped,
            EqChannel::Contrast => c = clamped,
            EqChannel::Saturation => sat = clamped,
            EqChannel::Gamma => g = clamped,
        }
        self.command(
            Command::SetVideoEq {
                brightness: b,
                contrast: c,
                saturation: sat,
                gamma: g,
            },
            cx,
        );
        self.show_osd(format!("{} · {clamped:+}", ch.label()), cx);
        self.notify_dock(cx);
    }

    fn step_video_eq(&mut self, ch: EqChannel, delta: i64, cx: &mut Context<Self>) {
        let st = self.session().state();
        let cur = match ch {
            EqChannel::Brightness => st.eq_brightness,
            EqChannel::Contrast => st.eq_contrast,
            EqChannel::Saturation => st.eq_saturation,
            EqChannel::Gamma => st.eq_gamma,
        };
        self.adjust_video_eq(ch, cur + delta, cx);
    }
}

fn render_eq_section(
    model: &Entity<AppModel>,
    eq_rows: [(&'static str, EqChannel, i64); 4],
    eq_dirty: bool,
    t: ThemeTokens,
) -> AnyElement {
    let header = hbetween(6.)
        .child(
            hstack(6.)
                .text_size(t.text_xs)
                .text_color(t.text_dim)
                .child(icon(Icon::Sliders, 12., t.text_faint))
                .child("画面均衡器"),
        )
        .child(
            div()
                .id("eq-reset-all")
                .text_size(t.text_xs)
                .text_color(if eq_dirty { t.accent } else { t.text_faint })
                .cursor_pointer()
                .hover(|s| s.text_color(t.text))
                .child("重置均衡")
                .on_click(model_listener(model, |this, _, _, cx| {
                    this.command(Command::ResetVideoEq, cx);
                    this.show_osd("画面均衡已重置", cx);
                    this.notify_dock(cx);
                })),
        );

    let channels: Vec<AnyElement> = eq_rows
        .into_iter()
        .map(|(id, ch, val)| {
            let disp = if val == 0 {
                "0".into()
            } else {
                format!("{val:+}")
            };
            hbetween(8.)
                .min_h(t.control_h_sm)
                .child(
                    div()
                        .text_size(t.text_sm)
                        .text_color(if val != 0 { t.text } else { t.text_muted })
                        .child(ch.label()),
                )
                .child(stepper(
                    id,
                    disp,
                    val != 0,
                    t,
                    model_listener(model, move |this, _, _, cx| this.step_video_eq(ch, -5, cx)),
                    model_listener(model, move |this, _, _, cx| this.step_video_eq(ch, 5, cx)),
                    model_listener(model, move |this, _, _, cx| this.adjust_video_eq(ch, 0, cx)),
                ))
                .into_any_element()
        })
        .collect();

    vstack(5.)
        .px(px(10.))
        .py(px(8.))
        .border_b_1()
        .border_color(t.line)
        .child(header)
        .children(channels)
        .into_any_element()
}

fn render_chapter_section(
    model: &Entity<AppModel>,
    chapters: Vec<Chapter>,
    current_ch: i64,
    t: ThemeTokens,
) -> AnyElement {
    let rows: Vec<AnyElement> = chapters
        .into_iter()
        .enumerate()
        .map(|(i, ch)| {
            let active = current_ch == i as i64;
            select_row(
                ("ch-row", i),
                super::super::strings::chrome::chapter_title(i, &ch.title),
                Some(format_time(ch.time).into()),
                active,
                false,
                false,
                t,
            )
            .text_size(t.text_sm)
            .on_click(model_listener(model, move |this, _, _, cx| {
                this.command(Command::SelectChapter(i), cx);
                this.overlays.popover = None;
                this.notify_regions(
                    DirtyRegions {
                        root: true,
                        dock: true,
                        ..DirtyRegions::NONE
                    },
                    cx,
                );
            }))
            .into_any_element()
        })
        .collect();

    vstack(0.)
        .border_t_1()
        .border_color(t.line)
        .child(
            div()
                .px(px(10.))
                .pt(px(6.))
                .pb(px(4.))
                .text_size(t.text_xs)
                .text_color(t.text_dim)
                .child("章节"),
        )
        .child(
            vstack(0.)
                .id("chapter-list")
                .max_h(px(120.))
                .overflow_y_scroll()
                .pb(px(4.))
                .children(rows),
        )
        .into_any_element()
}

pub(super) fn render_more_popover(
    model: &Entity<AppModel>,
    t: ThemeTokens,
    cx: &App,
) -> impl IntoElement {
    let m = model.read(cx);
    let session = m.session();
    let state = session.state();
    let aspect = state.aspect;
    let (hflip, vflip) = session.display_flips();
    let (chapters, current_ch) = (state.chapters.clone(), state.current_chapter);
    let is_video = session.kind() == Some(Kind::Video);
    let is_visual = matches!(session.kind(), Some(Kind::Video | Kind::Photo));
    let eq_dirty = state.eq_brightness != 0
        || state.eq_contrast != 0
        || state.eq_saturation != 0
        || state.eq_gamma != 0;
    let eq_rows = [
        ("eq-bri", EqChannel::Brightness, state.eq_brightness),
        ("eq-con", EqChannel::Contrast, state.eq_contrast),
        ("eq-sat", EqChannel::Saturation, state.eq_saturation),
        ("eq-gam", EqChannel::Gamma, state.eq_gamma),
    ];
    let info_open = m.overlays.info_open;

    let eq_sec = is_visual.then(|| render_eq_section(model, eq_rows, eq_dirty, t));
    let ch_sec =
        (!chapters.is_empty()).then(|| render_chapter_section(model, chapters, current_ch, t));
    let act = |id: &'static str, lbl: &'static str, hint: &str, on: bool| {
        menu_row(id, None, lbl, m.key_hint(hint), on, t)
    };
    let actions = vstack(0.)
        .p(px(4.))
        .child(
            act(
                "more-rot",
                super::super::strings::actions::action_title("rotate_cw"),
                "rotate_cw",
                false,
            )
            .on_click(model_listener(model, |this, _, w, cx| {
                this.rotate_cw(&RotateCw, w, cx);
            })),
        )
        .child(
            act(
                "more-hflip",
                super::super::strings::actions::action_title("toggle_hflip"),
                "toggle_hflip",
                hflip,
            )
            .on_click(model_listener(model, |this, _, w, cx| {
                this.toggle_hflip(&ToggleHFlip, w, cx);
            })),
        )
        .child(
            act(
                "more-vflip",
                super::super::strings::actions::action_title("toggle_vflip"),
                "toggle_vflip",
                vflip,
            )
            .on_click(model_listener(model, |this, _, w, cx| {
                this.toggle_vflip(&ToggleVFlip, w, cx);
            })),
        )
        .when(is_video, |d| {
            d.child(
                act(
                    "more-shot",
                    super::super::strings::actions::action_title("take_screenshot"),
                    "take_screenshot",
                    false,
                )
                .on_click(model_listener(model, |this, _, w, cx| {
                    this.overlays.popover = None;
                    this.take_screenshot(&TakeScreenshot, w, cx);
                    this.notify_dock(cx);
                })),
            )
        })
        .child(
            act(
                "more-info",
                super::super::strings::chrome::MEDIA_INFO_LABEL,
                "toggle_info",
                info_open,
            )
            .on_click(model_listener(model, |this, _, w, cx| {
                this.overlays.popover = None;
                this.toggle_info(&ToggleInfo, w, cx);
                this.notify_root(cx);
            })),
        );

    let aspect_sec = is_video.then(|| {
        vstack(6.)
            .px(px(10.))
            .py(px(8.))
            .border_b_1()
            .border_color(t.line)
            .child(
                hstack(6.)
                    .text_size(t.text_xs)
                    .text_color(t.text_dim)
                    .child(icon(Icon::Aspect, 12., t.text_faint))
                    .child("画面比例"),
            )
            .child(segmented(
                "aspect-opt",
                ASPECT_OPTIONS,
                aspect,
                t,
                model,
                |this, m, _, cx| {
                    this.command(Command::SetAspect(m), cx);
                    this.show_osd(
                        format!(
                            "画面比例 · {}",
                            super::super::strings::chrome::aspect_label(m)
                        ),
                        cx,
                    );
                    this.notify_dock(cx);
                },
            ))
    });

    vstack(0.)
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .child(popover_header("画面与工具", None, t))
        .child(
            vstack(0.)
                .id("more-scroll")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .children(aspect_sec)
                .children(eq_sec)
                .child(actions)
                .children(ch_sec),
        )
}

pub(super) fn render_presets_popover(
    model: &Entity<AppModel>,
    t: ThemeTokens,
    cx: &App,
) -> impl IntoElement {
    let m = model.read(cx);
    let active_id = if m.settings.active_preset.trim().is_empty() {
        "default".to_string()
    } else {
        m.settings.active_preset.clone()
    };
    let active_is_custom = m.presets.get(&active_id).is_some_and(|p| !p.builtin);
    let presets: Vec<_> = m.presets.list().to_vec();
    let cycle_hint = m
        .key_hint("cycle_preset")
        .map(|k| SharedString::from(format!("{k} 循环切换")));
    let update_id = active_id.clone();

    let rows: Vec<AnyElement> = presets
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let active = p.id == active_id;
            let name = super::super::strings::presets::preset_name(&p).to_string();
            let desc = super::super::strings::presets::preset_description(&p).to_string();
            let (pid, builtin) = (p.id, p.builtin);
            let title_col = if active { t.text } else { t.text_muted };
            div()
                .id(("preset-row", i))
                .px(px(10.))
                .py(px(6.))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .cursor_pointer()
                .when(active, |d| d.bg(t.selected))
                .when(!active, |d| d.hover(|s| s.bg(t.hover)))
                .child(
                    vstack(2.)
                        .flex_1()
                        .min_w(px(0.))
                        .child(
                            hstack(6.)
                                .min_w(px(0.))
                                .child(
                                    div()
                                        .min_w(px(0.))
                                        .truncate()
                                        .text_size(t.text_md)
                                        .text_color(title_col)
                                        .child(name),
                                )
                                .child(badge_pill(
                                    if builtin { "内置" } else { "自定义" },
                                    !builtin,
                                    t,
                                )),
                        )
                        .when(!desc.is_empty(), |d| {
                            d.child(
                                div()
                                    .truncate()
                                    .text_size(t.text_xs)
                                    .text_color(t.text_dim)
                                    .child(desc),
                            )
                        }),
                )
                .when(active, |d| d.child(icon(Icon::Check, 12., t.accent)))
                .on_click(model_listener(model, move |this, _, w, cx| {
                    this.activate_preset(&pid, w, cx);
                }))
                .into_any_element()
        })
        .collect();

    let fbtn = |id: &'static str, label: &'static str, accent: bool| {
        let hover_text = if accent { t.accent } else { t.text };
        button(id, label, ButtonVariant::Ghost, ButtonSize::Sm, t)
            .w_full()
            .justify_start()
            .px(px(10.))
            .rounded(px(0.))
            .text_color(if accent { t.accent } else { t.text_muted })
            .hover(move |s| s.bg(t.hover).text_color(hover_text))
    };

    vstack(0.)
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .child(popover_header("工作区预设", cycle_hint, t))
        .child(
            vstack(0.)
                .id("presets-list")
                .flex_1()
                .min_h(px(0.))
                .max_h(px(280.))
                .overflow_y_scroll()
                .py(px(4.))
                .children(rows),
        )
        .child(
            vstack(0.)
                .flex_none()
                .border_t_1()
                .border_color(t.line)
                .py(px(4.))
                .child(
                    fbtn("preset-save-new", "+ 将当前状态保存为新预设…", true).on_click(
                        model_listener(model, |this, _, w, cx| {
                            let scope = PresetCaptureScope::default();
                            this.open_builtin_dialog(BuiltinDialog::SavePreset { scope }, w, cx);
                        }),
                    ),
                )
                .when(active_is_custom, |d| {
                    d.child(
                        fbtn("preset-update-cur", "↻ 将当前变更更新到此预设", false).on_click(
                            model_listener(model, move |this, _, w, cx| {
                                this.overlays.popover = None;
                                let scope = PresetCaptureScope::default();
                                this.update_preset_from_current(&update_id, scope, w, cx);
                            }),
                        ),
                    )
                })
                .child(
                    fbtn("preset-manage-all", "管理全部预设…", false).on_click(model_listener(
                        model,
                        |this, _, _, cx| {
                            this.overlays.open_settings(Some(SettingsTab::Presets));
                            this.notify_regions(
                                DirtyRegions {
                                    root: true,
                                    settings: true,
                                    dock: true,
                                    ..DirtyRegions::NONE
                                },
                                cx,
                            );
                        },
                    )),
                ),
        )
}
