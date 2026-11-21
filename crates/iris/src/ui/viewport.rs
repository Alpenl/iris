//! A viewport: one session and native video surface, plus the area that
//! measures where the surface goes and the overlays drawn over it (OSD, audio
//! card, photo filmstrip, recent items, media info, popovers, fullscreen dock).
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    Context, CursorStyle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ObjectFit,
    ParentElement, Pixels, Point, StatefulInteractiveElement, Styled, StyledImage, canvas, div,
    img, prelude::FluentBuilder, px,
};

use super::context_menu::{ContextMenu, ContextTarget};
use super::controls::fmt_time;
use super::widgets::icon;
use super::*;
use crate::assets::Icon;
use crate::geometry::ImageView;
use crate::player::Session;
use crate::video::VideoSurface;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct VideoRect(pub [f64; 4]);

pub(super) struct Viewport {
    pub session: Session,
    pub video: Rc<RefCell<Option<VideoSurface>>>,
    pub rect: Rc<Cell<Option<VideoRect>>>,
    pub image: ImageView,
    pub drag_origin: Option<Point<Pixels>>,
}

impl Drop for Viewport {
    fn drop(&mut self) {
        drop(self.video.borrow_mut().take());
        self.session.shutdown();
    }
}

impl IrisWindow {
    pub(super) fn render_video_area(
        &self,
        fullscreen: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let last_rect = self.viewports[0].rect.clone();
        let video = self.viewports[0].video.clone();
        let probe = canvas(
            move |bounds, _, _| {
                let r = VideoRect([
                    f32::from(bounds.origin.x) as f64,
                    f32::from(bounds.origin.y) as f64,
                    f32::from(bounds.size.width) as f64,
                    f32::from(bounds.size.height) as f64,
                ]);
                if r.0[2] <= 0. || r.0[3] <= 0. || last_rect.get() == Some(r) {
                    return;
                }
                last_rect.set(Some(r));
                if let Some(v) = video.borrow().as_ref() {
                    let [x, y, w, h] = r.0;
                    v.set_frame(x, y, w, h);
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        let kind = self.viewports[0].session.kind();
        let is_photo_zoomed = kind == Some(Kind::Photo) && self.viewports[0].image.zoomed();
        let panning = self.viewports[0].drag_origin.is_some();
        let show_filmstrip = kind == Some(Kind::Photo)
            && self.settings.photo_filmstrip
            && (!fullscreen || self.chrome_visible);

        // Top-centre pill: priority order Toast > OSD > delayed Loading.
        let top_pill = self
            .toast
            .clone()
            .or_else(|| self.osd.clone())
            .or_else(|| self.loading_visible.then(|| SharedString::from("加载中…")));

        div()
            .id("video")
            .relative()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .overflow_hidden()
            .when(self.chrome_shown() && is_photo_zoomed && panning, |d| {
                d.cursor(CursorStyle::ClosedHand)
            })
            .when(self.chrome_shown() && is_photo_zoomed && !panning, |d| {
                d.cursor(CursorStyle::OpenHand)
            })
            .child(probe)
            .key_context({
                let mut context = gpui::KeyContext::default();
                if self.settings_open || self.context_menu.is_some() || self.palette_open {
                    context.add("Overlay");
                } else {
                    context.add("Viewport");
                    context.set(
                        "media",
                        match kind {
                            Some(Kind::Photo) => "image",
                            Some(Kind::Video) => "video",
                            Some(Kind::Audio) => "audio",
                            None => "none",
                        },
                    );
                    if self.viewports[0].image.zoomed() {
                        context.add("zoomed");
                    }
                }
                context
            })
            .track_focus(&self.focus)
            .on_scroll_wheel(cx.listener(|this, ev: &gpui::ScrollWheelEvent, _, cx| {
                let raw_y = f32::from(ev.delta.pixel_delta(px(20.)).y) as f64;
                if raw_y == 0. {
                    return;
                }
                match input::resolve_pointer_input(
                    PointerInput::Wheel(raw_y / 240.),
                    this.input_context(),
                ) {
                    Some(PointerCommand::Zoom(delta)) => {
                        let scale = if ev.delta.precise() { 0.45 } else { 1.0 };
                        this.zoom_at(delta * scale, Some(ev.position), cx);
                        cx.stop_propagation();
                    }
                    Some(PointerCommand::VolumeBy(_)) => {
                        let delta = this.damped_volume_delta(ev);
                        if delta.abs() >= 0.5 {
                            this.command(Command::VolumeBy(delta), cx);
                            let next =
                                (this.viewports[0].session.state.volume + delta).clamp(0., 100.);
                            this.show_osd(format!("音量 · {next:.0}%"), cx);
                        }
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.popover = None;
                    this.context_menu = Some(ContextMenu {
                        pos: ev.position,
                        target: ContextTarget::Viewport,
                    });
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    let ctx = this.input_context();
                    if ev.click_count >= 2 {
                        // Cancel any pending single-click pause timer.
                        this.click_seq += 1;
                        match input::resolve_pointer_input(PointerInput::DoubleClick, ctx) {
                            Some(PointerCommand::CloseOverlay) => this.close_overlays(cx),
                            Some(PointerCommand::ToggleFullscreen) => window.toggle_fullscreen(),
                            Some(PointerCommand::ToggleActualAtCursor) => {
                                this.toggle_image_size_at(ev.position, window, cx);
                            }
                            _ => {}
                        }
                    } else if let Some(PointerCommand::CloseOverlay) =
                        input::resolve_pointer_input(PointerInput::BeginDrag, ctx)
                    {
                        this.close_overlays(cx);
                    } else if let Some(PointerCommand::BeginPan) =
                        input::resolve_pointer_input(PointerInput::BeginDrag, ctx)
                    {
                        this.viewports[0].drag_origin = Some(ev.position);
                        cx.notify();
                    } else if let Some(PointerCommand::TogglePlayback) =
                        input::resolve_pointer_input(PointerInput::SingleClick, ctx)
                    {
                        // Short delay so double-click fullscreen doesn't pause first.
                        this.click_seq += 1;
                        let seq = this.click_seq;
                        cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(Duration::from_millis(200))
                                .await;
                            let _ = this.update(cx, |this, cx| {
                                if this.click_seq == seq {
                                    let next_paused = !this.viewports[0].session.state.paused;
                                    this.command(Command::TogglePlayback, cx);
                                    this.show_osd(if next_paused { "暂停" } else { "播放" }, cx);
                                }
                            });
                        })
                        .detach();
                    }
                }),
            )
            // Audio playback card (when playing pure audio files).
            .when(kind == Some(Kind::Audio), |d| {
                d.child(self.render_audio_card())
            })
            // Plugin vector overlay slot (normalized shapes; non-intercepting).
            .children(self.render_plugin_overlay())
            // Fullscreen: file name and media info at the top, on the edge
            // away from the directory so the popup never covers it.
            .when(fullscreen && self.chrome_visible, |d| {
                let name = self.viewports[0]
                    .session
                    .current
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned());
                d.when_some(name, |d, name| {
                    let chip = div()
                        .absolute()
                        .top(px(FLOAT_GAP))
                        .max_w(px(520.))
                        .h(px(28.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .bg(overlay())
                        .border_1()
                        .border_color(line())
                        .child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .text_size(px(12.))
                                .child(name),
                        )
                        .when_some(self.media_info(), |d, info| {
                            d.child(
                                div()
                                    .flex_none()
                                    .font_family(FONT_MONO)
                                    .text_size(px(11.))
                                    .text_color(text_dim())
                                    .child(info),
                            )
                        });
                    d.child(if self.settings.directory_side == Side::Left {
                        chip.right(px(FLOAT_GAP))
                    } else {
                        chip.left(px(FLOAT_GAP))
                    })
                })
            })
            // Top-centre pill: toast / lightweight OSD / delayed loading indicator.
            .when_some(top_pill, |d, msg| {
                d.child(
                    div()
                        .absolute()
                        .top(px(if fullscreen { FLOAT_GAP } else { 12. }))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .h(px(28.))
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .bg(overlay())
                                .border_1()
                                .border_color(line())
                                .child(div().flex_none().size(px(6.)).bg(accent()))
                                .child(
                                    div()
                                        .font_family(FONT_MONO)
                                        .text_size(px(12.))
                                        .child(msg),
                                ),
                        ),
                )
            })
            // Media info inspector panel (`I` / `⌘I`).
            .when(self.info_open && self.viewports[0].session.current.is_some(), |d| {
                d.child(self.render_info_overlay(fullscreen, cx))
            })
            // Empty state: open CTA + Recent files/folders list.
            .when(
                self.viewports[0].session.current.is_none() && self.pending_path.is_none(),
                |d| d.child(self.render_empty_state(cx)),
            )
            // Photo thumbnail filmstrip along the bottom edge.
            .when(show_filmstrip, |d| {
                d.children(self.render_photo_filmstrip(fullscreen, cx))
            })
            // Fullscreen floating control bar.
            .when(fullscreen && self.chrome_visible, |d| {
                let slot = self.dock_bounds.clone();
                let seam = matches!(kind, Some(Kind::Video | Kind::Audio));
                d.child(
                    div()
                        .absolute()
                        .bottom(px(FLOAT_GAP))
                        .left(px(FLOAT_GAP))
                        .right(px(FLOAT_GAP))
                        .h(px(DOCK_H))
                        .bg(overlay())
                        .border_color(line())
                        .border_l_1()
                        .border_r_1()
                        .border_b_1()
                        .when(!seam, |d| d.border_t_1())
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                        .child(
                            canvas(move |b, _, _| slot.set(Some(b)), |_, _, _, _| {})
                                .absolute()
                                .size_full(),
                        )
                        .child(self.render_dock(true, window, cx)),
                )
            })
            // Button-anchored popover above the control bar.
            .when_some(self.popover, |d, pop| {
                d.child(self.render_popover(pop, fullscreen, cx))
            })
            // Fullscreen directory popup.
            .when(fullscreen && self.dir_popup, |d| {
                let popup = div()
                    .absolute()
                    .top(px(FLOAT_GAP))
                    .bottom(px(DOCK_H + FLOAT_GAP + 8.))
                    .border_1()
                    .border_color(line())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .child(self.render_directory(true, window, cx));
                d.child(if self.settings.directory_side == Side::Left {
                    popup.left(px(FLOAT_GAP))
                } else {
                    popup.right(px(FLOAT_GAP))
                })
            })
    }

    fn render_audio_card(&self) -> impl IntoElement {
        let session = &self.viewports[0].session;
        let state = &session.state;
        let name = session
            .current
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "音频".into());
        div()
            .absolute()
            .inset_0()
            .bg(window_bg())
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(360.))
                    .p(px(20.))
                    .bg(panel())
                    .border_1()
                    .border_color(line())
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .size(px(48.))
                            .bg(window_bg())
                            .border_1()
                            .border_color(line_strong())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(Icon::Music, 22., accent())),
                    )
                    .child(
                        div()
                            .max_w(px(320.))
                            .truncate()
                            .text_size(px(14.))
                            .text_color(text())
                            .child(name),
                    )
                    .child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(px(12.))
                            .text_color(text_dim())
                            .child(format!(
                                "{} / {} · {}",
                                fmt_time(state.time_pos),
                                if state.duration > 0. {
                                    fmt_time(state.duration)
                                } else {
                                    "--:--".into()
                                },
                                if state.paused { "已暂停" } else { "播放中" }
                            )),
                    ),
            )
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let recent: Vec<_> = self.store.recent.iter().take(6).cloned().collect();
        div()
            .absolute()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(16.))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(text_dim())
                    .child("拖入文件或文件夹"),
            )
            .child(
                div()
                    .id("open-file")
                    .h(px(BUTTON))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .border_1()
                    .border_color(line_strong())
                    .text_size(px(12.))
                    .cursor_pointer()
                    .hover(|s| s.bg(hover()))
                    .active(|s| s.bg(selected()))
                    .child("打开…")
                    .child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(px(11.))
                            .text_color(text_dim())
                            .child(if cfg!(target_os = "macos") {
                                "⌘O"
                            } else {
                                "Ctrl+O"
                            }),
                    )
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, w, cx| this.open_file(&OpenFile, w, cx))),
            )
            .when(!recent.is_empty(), |d| {
                d.child(
                    div()
                        .mt(px(8.))
                        .w(px(340.))
                        .bg(panel())
                        .border_1()
                        .border_color(line())
                        .flex()
                        .flex_col()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .h(px(28.))
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .justify_between()
                                .border_b_1()
                                .border_color(line())
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(text_dim())
                                        .child("最近打开"),
                                )
                                .child(
                                    div()
                                        .id("clear-recent")
                                        .text_size(px(10.))
                                        .text_color(text_faint())
                                        .cursor_pointer()
                                        .hover(|s| s.text_color(text()))
                                        .child("清空")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.store.clear_recent();
                                            cx.notify();
                                        })),
                                ),
                        )
                        .children(recent.into_iter().enumerate().map(|(i, p)| {
                            let label = p
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| p.display().to_string());
                            let glyph = match library::kind_of(&p) {
                                Some(Kind::Video) => Icon::Video,
                                Some(Kind::Photo) => Icon::Photo,
                                Some(Kind::Audio) => Icon::Music,
                                None => Icon::Folder,
                            };
                            div()
                                .id(("recent", i))
                                .h(px(26.))
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .cursor_pointer()
                                .text_size(px(12.))
                                .text_color(text_muted())
                                .hover(|s| s.bg(hover()).text_color(text()))
                                .child(icon(glyph, 13., text_faint()))
                                .child(div().flex_1().min_w(px(0.)).truncate().child(label))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_path(&p, cx);
                                }))
                        })),
                )
            })
    }

    /// Lightweight bottom thumbnail filmstrip for quick visual photo browsing.
    fn render_photo_filmstrip(
        &self,
        fullscreen: bool,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let photos: Vec<Entry> = self
            .display_entries()
            .into_iter()
            .filter(|e| e.kind == Kind::Photo)
            .collect();
        if photos.len() < 2 {
            return None;
        }
        let current = self.viewports[0].session.current.as_ref();
        let cur_ix = current
            .and_then(|p| photos.iter().position(|e| &e.path == p))
            .unwrap_or(0);
        const WINDOW: usize = 11;
        let half = WINDOW / 2;
        let start = cur_ix
            .saturating_sub(half)
            .min(photos.len().saturating_sub(WINDOW));
        let end = (start + WINDOW).min(photos.len());
        let slice = &photos[start..end];
        let bottom = if fullscreen {
            DOCK_H + FLOAT_GAP + 8.
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
                        .bg(overlay())
                        .border_1()
                        .border_color(line())
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                        .children(slice.iter().enumerate().map(|(offset, e)| {
                            let is_cur = Some(&e.path) == current;
                            let path = e.path.clone();
                            let thumb = self.thumb_cache.get(&e.path).cloned();
                            div()
                                .id(("thumb", start + offset))
                                .w(px(56.))
                                .h(px(38.))
                                .flex_none()
                                .overflow_hidden()
                                .bg(window_bg())
                                .border_1()
                                .border_color(if is_cur { accent() } else { line() })
                                .cursor_pointer()
                                .hover(|s| s.border_color(if is_cur { accent() } else { text_dim() }))
                                .flex()
                                .items_center()
                                .justify_center()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_path(&path, cx);
                                }))
                                .when_some(thumb.clone(), |d, t| {
                                    d.child(img(t).size_full().object_fit(ObjectFit::Cover))
                                })
                                .when(thumb.is_none(), |d| {
                                    d.child(icon(Icon::Photo, 14., text_faint()))
                                })
                        })),
                ),
        )
    }
}
