//! Direction C Command Palette (`⌘P` / `Ctrl+P` / `⌘⇧P`): unifies built-in
//! player commands and all commands contributed by enabled plugins.
use gpui::{
    ClipboardItem, Context, ElementInputHandler, InteractiveElement, IntoElement, KeyContext,
    KeyDownEvent, MouseButton, ParentElement, StatefulInteractiveElement, Styled, Window, canvas,
    div, prelude::FluentBuilder, px,
};

use super::widgets::icon;
use super::*;
use crate::assets::Icon;

#[derive(Clone, Debug)]
pub(super) enum PaletteTarget {
    Host(&'static str),
    Plugin {
        plugin_id: String,
        command_id: String,
    },
}

#[derive(Clone, Debug)]
pub(super) struct PaletteEntry {
    pub title: String,
    pub subtitle: Option<String>,
    pub badge: String,
    pub shortcut: Option<String>,
    pub target: PaletteTarget,
}

impl IrisWindow {
    pub(super) fn palette_entries(&self) -> Vec<PaletteEntry> {
        let cmd_prefix = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl+"
        };
        let mut entries = vec![
            PaletteEntry {
                title: "打开文件或文件夹…".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some(format!("{cmd_prefix}O")),
                target: PaletteTarget::Host("open"),
            },
            PaletteEntry {
                title: "播放 / 暂停".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some("Space".into()),
                target: PaletteTarget::Host("toggle_pause"),
            },
            PaletteEntry {
                title: "静音 / 取消静音".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some("M".into()),
                target: PaletteTarget::Host("toggle_mute"),
            },
            PaletteEntry {
                title: "切换单曲循环".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some("L".into()),
                target: PaletteTarget::Host("toggle_loop"),
            },
            PaletteEntry {
                title: "切换画面比例 / 铺满裁剪".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some("C".into()),
                target: PaletteTarget::Host("cycle_aspect"),
            },
            PaletteEntry {
                title: "顺时针旋转 90°".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some("R".into()),
                target: PaletteTarget::Host("rotate_cw"),
            },
            PaletteEntry {
                title: "水平镜像翻转".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some("H".into()),
                target: PaletteTarget::Host("hflip"),
            },
            PaletteEntry {
                title: "保存当前帧截图".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some(format!("{cmd_prefix}S")),
                target: PaletteTarget::Host("screenshot"),
            },
            PaletteEntry {
                title: "媒体详情与统计".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some("I".into()),
                target: PaletteTarget::Host("info"),
            },
            PaletteEntry {
                title: "切换窗口置顶".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some("T".into()),
                target: PaletteTarget::Host("always_on_top"),
            },
            PaletteEntry {
                title: "切换目录 / 插件侧栏视图".into(),
                subtitle: Some("在文件目录与插件面板之间切换".into()),
                badge: "内核".into(),
                shortcut: None,
                target: PaletteTarget::Host("toggle_sidebar_mode"),
            },
            PaletteEntry {
                title: "搜索目录".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some(format!("{cmd_prefix}F")),
                target: PaletteTarget::Host("search"),
            },
            PaletteEntry {
                title: "刷新目录".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some(format!("{cmd_prefix}R")),
                target: PaletteTarget::Host("refresh"),
            },
            PaletteEntry {
                title: "打开设置…".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: Some(format!("{cmd_prefix},")),
                target: PaletteTarget::Host("settings"),
            },
            PaletteEntry {
                title: "管理插件…".into(),
                subtitle: Some("启用、禁用或重新扫描外部插件".into()),
                badge: "插件".into(),
                shortcut: None,
                target: PaletteTarget::Host("plugins"),
            },
        ];

        for (plugin_id, plugin_name, spec) in self.plugins.commands() {
            entries.push(PaletteEntry {
                title: spec.title.clone(),
                subtitle: spec.subtitle.clone(),
                badge: plugin_name.to_string(),
                shortcut: spec.shortcut_hint.clone(),
                target: PaletteTarget::Plugin {
                    plugin_id: plugin_id.to_string(),
                    command_id: spec.id.clone(),
                },
            });
        }

        let q = self.palette_input.text().trim().to_ascii_lowercase();
        if !q.is_empty() {
            entries.retain(|e| {
                e.title.to_ascii_lowercase().contains(&q)
                    || e.badge.to_ascii_lowercase().contains(&q)
                    || e.subtitle
                        .as_deref()
                        .is_some_and(|s| s.to_ascii_lowercase().contains(&q))
            });
        }
        entries
    }

    pub(super) fn execute_palette_target(
        &mut self,
        target: PaletteTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlays.palette_open = false;
        self.palette_input.clear();
        self.palette_index = 0;
        self.focus.focus(window);
        match target {
            PaletteTarget::Host("open") => self.open_file(&OpenFile, window, cx),
            PaletteTarget::Host("toggle_pause") => self.toggle_pause(&TogglePause, window, cx),
            PaletteTarget::Host("toggle_mute") => self.toggle_mute(&ToggleMute, window, cx),
            PaletteTarget::Host("toggle_loop") => self.toggle_loop(&ToggleLoop, window, cx),
            PaletteTarget::Host("cycle_aspect") => self.cycle_aspect(&CycleAspect, window, cx),
            PaletteTarget::Host("rotate_cw") => self.rotate_cw(&RotateCw, window, cx),
            PaletteTarget::Host("hflip") => self.toggle_hflip(&ToggleHFlip, window, cx),
            PaletteTarget::Host("screenshot") => self.take_screenshot(&TakeScreenshot, window, cx),
            PaletteTarget::Host("info") => self.toggle_info(&ToggleInfo, window, cx),
            PaletteTarget::Host("always_on_top") => {
                self.toggle_always_on_top(&ToggleAlwaysOnTop, window, cx)
            }
            PaletteTarget::Host("toggle_sidebar_mode") => {
                self.sidebar_plugins = !self.sidebar_plugins;
                if !self.fullscreen {
                    self.show_directory = true;
                } else {
                    self.overlays.dir_popup = true;
                }
                cx.notify();
            }
            PaletteTarget::Host("search") => self.toggle_search(&ToggleSearch, window, cx),
            PaletteTarget::Host("refresh") => self.refresh_directory(&RefreshDirectory, window, cx),
            PaletteTarget::Host("settings") => {
                self.overlays.open_settings(None);
                cx.notify();
            }
            PaletteTarget::Host("plugins") => {
                self.overlays.open_settings(Some(SettingsTab::Plugins));
                cx.notify();
            }
            PaletteTarget::Host(_) => {}
            PaletteTarget::Plugin {
                plugin_id,
                command_id,
            } => {
                self.invoke_plugin_command(&plugin_id, &command_id, window, cx);
            }
        }
    }

    pub(super) fn render_command_palette(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let vp = window.viewport_size();
        let w = (f32::from(vp.width) - 32.).clamp(360., 520.);
        let entries = self.palette_entries();
        let count = entries.len();
        let selected_ix = if count > 0 {
            self.palette_index.min(count - 1)
        } else {
            0
        };
        let is_empty = self.palette_input.is_empty();
        let display = if is_empty {
            "输入命令或插件名称…".to_string()
        } else {
            self.palette_input.display_text(true)
        };
        let focus_ime = self.palette_focus.clone();
        let entity_ime = cx.entity().clone();

        div()
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            .pt(px(56.))
            .flex()
            .items_start()
            .justify_center()
            .bg(scrim())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.overlays.palette_open = false;
                    this.palette_input.clear();
                    this.focus.focus(window);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("palette-box")
                    .w(px(w))
                    .max_h(px(420.))
                    .flex()
                    .flex_col()
                    .bg(panel())
                    .border_1()
                    .border_color(line_strong())
                    .track_focus(&self.palette_focus)
                    .key_context({
                        let mut ctx = KeyContext::default();
                        ctx.add("CommandPalette");
                        ctx.add("TextInput");
                        ctx
                    })
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        if this.palette_input.marked_range.is_some() {
                            // Active IME composition: let the OS input method handle editing/confirm/cancel keys.
                            return;
                        }
                        let mods = ev.keystroke.modifiers;
                        if mods.secondary() && !mods.alt {
                            match ev.keystroke.key.as_str() {
                                "a" => {
                                    this.palette_input.select_all();
                                    cx.stop_propagation();
                                    cx.notify();
                                    return;
                                }
                                "c" => {
                                    let sel = this.palette_input.selected_text();
                                    let to_copy = if sel.is_empty() {
                                        this.palette_input.text()
                                    } else {
                                        sel
                                    };
                                    if !to_copy.is_empty() {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            to_copy.to_string(),
                                        ));
                                    }
                                    cx.stop_propagation();
                                    return;
                                }
                                "x" => {
                                    let sel = this.palette_input.selected_text().to_string();
                                    if !sel.is_empty() {
                                        cx.write_to_clipboard(ClipboardItem::new_string(sel));
                                        this.palette_input.backspace();
                                        this.palette_index = 0;
                                        this.palette_scroll.scroll_to_item(0);
                                        cx.notify();
                                    }
                                    cx.stop_propagation();
                                    return;
                                }
                                "v" => {
                                    if let Some(text) =
                                        cx.read_from_clipboard().and_then(|c| c.text())
                                    {
                                        this.palette_input.insert_paste(&text);
                                        this.palette_index = 0;
                                        this.palette_scroll.scroll_to_item(0);
                                        cx.notify();
                                    }
                                    cx.stop_propagation();
                                    return;
                                }
                                _ => return,
                            }
                        }
                        match ev.keystroke.key.as_str() {
                            "escape" => {
                                this.overlays.palette_open = false;
                                this.palette_input.clear();
                                this.focus.focus(window);
                                cx.stop_propagation();
                                cx.notify();
                            }
                            "up" => {
                                let len = this.palette_entries().len();
                                if len > 0 {
                                    this.palette_index = if this.palette_index == 0 {
                                        len - 1
                                    } else {
                                        this.palette_index - 1
                                    };
                                    this.palette_scroll.scroll_to_item(this.palette_index);
                                }
                                cx.stop_propagation();
                                cx.notify();
                            }
                            "down" => {
                                let len = this.palette_entries().len();
                                if len > 0 {
                                    this.palette_index = (this.palette_index + 1) % len;
                                    this.palette_scroll.scroll_to_item(this.palette_index);
                                }
                                cx.stop_propagation();
                                cx.notify();
                            }
                            "enter" => {
                                let items = this.palette_entries();
                                if !items.is_empty() {
                                    let ix = this.palette_index.min(items.len() - 1);
                                    let target = items[ix].target.clone();
                                    this.execute_palette_target(target, window, cx);
                                }
                                cx.stop_propagation();
                            }
                            "backspace" => {
                                this.palette_input.backspace();
                                this.palette_index = 0;
                                this.palette_scroll.scroll_to_item(0);
                                cx.stop_propagation();
                                cx.notify();
                            }
                            "delete" => {
                                this.palette_input.delete_forward();
                                this.palette_index = 0;
                                this.palette_scroll.scroll_to_item(0);
                                cx.stop_propagation();
                                cx.notify();
                            }
                            "left" => {
                                this.palette_input.move_left();
                                cx.stop_propagation();
                                cx.notify();
                            }
                            "right" => {
                                this.palette_input.move_right();
                                cx.stop_propagation();
                                cx.notify();
                            }
                            "home" => {
                                this.palette_input.move_home();
                                cx.stop_propagation();
                                cx.notify();
                            }
                            "end" => {
                                this.palette_input.move_end();
                                cx.stop_propagation();
                                cx.notify();
                            }
                            _ => {
                                // Leave printable keys unhandled so GPUI routes ASCII & IME
                                // composition through `EntityInputHandler`.
                            }
                        }
                    }))
                    .child(
                        div()
                            .relative()
                            .h(px(38.))
                            .flex_none()
                            .px(px(12.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .bg(window_bg())
                            .border_b_1()
                            .border_color(line())
                            .child(
                                canvas(
                                    |_, _, _| {},
                                    move |bounds, _, window, cx| {
                                        window.handle_input(
                                            &focus_ime,
                                            ElementInputHandler::new(bounds, entity_ime.clone()),
                                            cx,
                                        );
                                    },
                                )
                                .absolute()
                                .inset_0(),
                            )
                            .child(icon(Icon::Command, 14., accent()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_size(px(13.))
                                    .text_color(if is_empty { text_faint() } else { text() })
                                    .child(display),
                            )
                            .child(
                                div()
                                    .font_family(FONT_MONO)
                                    .text_size(px(10.))
                                    .text_color(text_faint())
                                    .child("↑↓ · Enter · Esc"),
                            ),
                    )
                    .child(
                        div()
                            .id("palette-list")
                            .flex_1()
                            .min_h(px(0.))
                            .max_h(px(360.))
                            .overflow_y_scroll()
                            .track_scroll(&self.palette_scroll)
                            .py(px(4.))
                            .flex()
                            .flex_col()
                            .when(entries.is_empty(), |d| {
                                d.child(
                                    div()
                                        .px(px(12.))
                                        .py(px(12.))
                                        .text_size(px(12.))
                                        .text_color(text_faint())
                                        .child("没有匹配的命令"),
                                )
                            })
                            .children(entries.into_iter().enumerate().map(|(i, item)| {
                                let active = i == selected_ix;
                                let target = item.target;
                                div()
                                    .id(("pal-row", i))
                                    .px(px(12.))
                                    .py(px(7.))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(10.))
                                    .cursor_pointer()
                                    .when(active, |d| d.bg(selected()).text_color(text()))
                                    .when(!active, |d| {
                                        d.text_color(text_muted())
                                            .hover(|s| s.bg(hover()).text_color(text()))
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.execute_palette_target(target.clone(), window, cx);
                                    }))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .flex()
                                            .flex_col()
                                            .gap(px(1.))
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .truncate()
                                                    .child(item.title),
                                            )
                                            .when_some(item.subtitle, |d, sub| {
                                                d.child(
                                                    div()
                                                        .text_size(px(10.))
                                                        .text_color(text_faint())
                                                        .truncate()
                                                        .child(sub),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .gap(px(6.))
                                            .child(
                                                div()
                                                    .px(px(5.))
                                                    .py(px(1.))
                                                    .bg(window_bg())
                                                    .border_1()
                                                    .border_color(line())
                                                    .text_size(px(10.))
                                                    .text_color(text_dim())
                                                    .child(item.badge),
                                            )
                                            .when_some(item.shortcut, |d, key| {
                                                d.child(
                                                    div()
                                                        .font_family(FONT_MONO)
                                                        .text_size(px(10.))
                                                        .text_color(text_faint())
                                                        .child(key),
                                                )
                                            }),
                                    )
                            })),
                    ),
            )
    }
}
