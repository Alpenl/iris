//! Command Palette (`⌘P` / `Ctrl+P` / `⌘⇧P`): aggregates core commands,
//! `ResetAppearance`, workspace presets, themes, dynamic `source_providers()` &
//! `media_sources()`, pinned bookmarks, and plugin commands using `kit::text_field`.

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use iris_core::keymap;
use iris_core::preset::{PresetCaptureScope, SidebarMode};
use iris_core::vfs::PinnedBookmark;

use super::IrisWindow;
use super::actions::{ClearAbLoop, CycleAbLoop};
use super::actions::{
    CycleAspect, CyclePreset, OpenFile, RefreshDirectory, RotateCw, TakeScreenshot,
    ToggleDirectory, ToggleInfo, ToggleLoop, ToggleMute, TogglePause, ToggleSearch,
};
use super::kit::badge_pill;
use super::kit::text_field::{TextFieldKind, TextFieldSpec};
use super::state::{BuiltinDialog, SettingsTab};
use crate::assets::Icon;
use crate::platform::IS_MACOS;
use crate::theme::{BUILTIN_THEMES, FONT_MONO, ThemeTokens};

#[derive(Clone, Debug)]
enum PaletteTarget {
    Host(&'static str),
    ResetAppearance,
    Preset(String),
    SavePreset,
    Theme(&'static str),
    OpenUrl,
    BrowsePluginSource {
        plugin_id: String,
        source_id: String,
    },
    Bookmark(PinnedBookmark),
    PinLocation,
    Plugin {
        plugin_id: String,
        command_id: String,
    },
}

#[derive(Clone, Debug)]
struct PaletteEntry {
    title: String,
    subtitle: Option<String>,
    badge: String,
    shortcut: Option<String>,
    target: PaletteTarget,
}

impl IrisWindow {
    fn palette_entries(&self) -> Vec<PaletteEntry> {
        let live_key = |id: &str| self.key_hint(id).map(|s| s.to_string());

        let mut entries = vec![
            PaletteEntry {
                title: "打开文件或文件夹…".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("open_file"),
                target: PaletteTarget::Host("open"),
            },
            PaletteEntry {
                title: "打开网络流 / 远程媒体地址…".into(),
                subtitle: Some("原生直连播放 HTTP / HTTPS 视频、HLS 或远程照片".into()),
                badge: "网络".into(),
                shortcut: live_key("open_url"),
                target: PaletteTarget::OpenUrl,
            },
            PaletteEntry {
                title: "恢复默认外观（清除无边框与插件主题覆盖）".into(),
                subtitle: Some("一键停用异常外观插件、恢复标题栏与默认石墨配色".into()),
                badge: "安全".into(),
                shortcut: None,
                target: PaletteTarget::ResetAppearance,
            },
            PaletteEntry {
                title: "播放 / 暂停".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("toggle_pause"),
                target: PaletteTarget::Host("toggle_pause"),
            },
            PaletteEntry {
                title: "静音 / 取消静音".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("toggle_mute"),
                target: PaletteTarget::Host("toggle_mute"),
            },
            PaletteEntry {
                title: "切换单曲循环".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("toggle_loop"),
                target: PaletteTarget::Host("toggle_loop"),
            },
            PaletteEntry {
                title: "设置 A / B 点或清除区间".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("cycle_ab_loop"),
                target: PaletteTarget::Host("cycle_ab_loop"),
            },
            PaletteEntry {
                title: "清除 A-B 循环".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("clear_ab_loop"),
                target: PaletteTarget::Host("clear_ab_loop"),
            },
            PaletteEntry {
                title: "切换画面比例 / 铺满裁剪".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("cycle_aspect"),
                target: PaletteTarget::Host("cycle_aspect"),
            },
            PaletteEntry {
                title: "顺时针旋转 90°".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("rotate_cw"),
                target: PaletteTarget::Host("rotate_cw"),
            },
            PaletteEntry {
                title: "保存当前帧截图".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("take_screenshot"),
                target: PaletteTarget::Host("screenshot"),
            },
            PaletteEntry {
                title: "媒体详情与统计".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("toggle_info"),
                target: PaletteTarget::Host("info"),
            },
            PaletteEntry {
                title: "切换侧栏视图（目录 / 数据源 / 队列 / 插件）".into(),
                subtitle: Some(format!(
                    "当前侧栏视图：{}",
                    self.catalog.sidebar_mode.label()
                )),
                badge: "工作区".into(),
                shortcut: None,
                target: PaletteTarget::Host("toggle_sidebar_mode"),
            },
            PaletteEntry {
                title: "展开 / 收起侧栏面板".into(),
                subtitle: None,
                badge: "工作区".into(),
                shortcut: live_key("toggle_directory"),
                target: PaletteTarget::Host("toggle_directory"),
            },
            PaletteEntry {
                title: "搜索目录".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("toggle_search"),
                target: PaletteTarget::Host("search"),
            },
            PaletteEntry {
                title: "刷新目录".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("refresh_directory"),
                target: PaletteTarget::Host("refresh"),
            },
            PaletteEntry {
                title: "循环切换下一个工作区预设".into(),
                subtitle: None,
                badge: "预设".into(),
                shortcut: live_key("cycle_preset"),
                target: PaletteTarget::Host("cycle_preset"),
            },
            PaletteEntry {
                title: "预设：将当前状态保存为新预设…".into(),
                subtitle: Some("保存当前窗口布局、主题、播放策略与插件组合".into()),
                badge: "预设".into(),
                shortcut: None,
                target: PaletteTarget::SavePreset,
            },
            PaletteEntry {
                title: "书签：固定当前目录到常驻书签".into(),
                subtitle: None,
                badge: "书签".into(),
                shortcut: None,
                target: PaletteTarget::PinLocation,
            },
            PaletteEntry {
                title: "打开设置…".into(),
                subtitle: None,
                badge: "内核".into(),
                shortcut: live_key("open_settings"),
                target: PaletteTarget::Host("settings"),
            },
            PaletteEntry {
                title: "管理快捷键方案与按键录制…".into(),
                subtitle: None,
                badge: "设置".into(),
                shortcut: None,
                target: PaletteTarget::Host("shortcuts"),
            },
            PaletteEntry {
                title: "管理插件…".into(),
                subtitle: None,
                badge: "插件".into(),
                shortcut: None,
                target: PaletteTarget::Host("plugins"),
            },
        ];

        for (pid, spec) in self.plugins.source_providers() {
            entries.push(PaletteEntry {
                title: format!("数据源：添加{}…", spec.label),
                subtitle: None,
                badge: "数据源".into(),
                shortcut: None,
                target: PaletteTarget::Plugin {
                    plugin_id: pid,
                    command_id: spec.add_command_id,
                },
            });
        }

        for preset in self.presets.list() {
            let cur = if self.settings.active_preset == preset.id {
                " [当前]"
            } else {
                ""
            };
            entries.push(PaletteEntry {
                title: format!("预设：切换到 {}{cur}", preset.name),
                subtitle: (!preset.description.is_empty()).then(|| preset.description.clone()),
                badge: "预设".into(),
                shortcut: live_key(&format!("preset:{}", preset.id)),
                target: PaletteTarget::Preset(preset.id.clone()),
            });
        }

        for th in BUILTIN_THEMES {
            let cur = if self.settings.theme_id.eq_ignore_ascii_case(th.id) {
                " [当前]"
            } else {
                ""
            };
            entries.push(PaletteEntry {
                title: format!("主题：切换到 {}{cur}", th.name),
                subtitle: Some(th.description.into()),
                badge: "主题".into(),
                shortcut: None,
                target: PaletteTarget::Theme(th.id),
            });
        }

        for (pid, pname, src) in self.plugins.media_sources() {
            entries.push(PaletteEntry {
                title: format!("数据源：浏览 {} · {}", pname, src.name),
                subtitle: src.subtitle.clone(),
                badge: pname.to_string(),
                shortcut: None,
                target: PaletteTarget::BrowsePluginSource {
                    plugin_id: pid.to_string(),
                    source_id: src.id.clone(),
                },
            });
        }

        for bm in &self.locations.bookmarks {
            entries.push(PaletteEntry {
                title: format!("书签：打开 {}", bm.name),
                subtitle: Some(bm.uri.to_uri_string()),
                badge: "书签".into(),
                shortcut: None,
                target: PaletteTarget::Bookmark(bm.clone()),
            });
        }

        for (pid, pname, cmd) in self.plugins.commands() {
            entries.push(PaletteEntry {
                title: cmd.title.clone(),
                subtitle: cmd.subtitle.clone(),
                badge: pname.to_string(),
                shortcut: live_key(&format!("plugin:{pid}:{}", cmd.id)).or_else(|| {
                    cmd.shortcut_hint
                        .as_deref()
                        .map(|s| keymap::format_keystroke_display(s, IS_MACOS))
                }),
                target: PaletteTarget::Plugin {
                    plugin_id: pid.to_string(),
                    command_id: cmd.id.clone(),
                },
            });
        }

        let q = self.text_inputs.palette_input.text().trim().to_lowercase();
        if !q.is_empty() {
            entries.retain(|e| {
                e.title.to_lowercase().contains(&q)
                    || e.badge.to_lowercase().contains(&q)
                    || e.subtitle
                        .as_ref()
                        .is_some_and(|s| s.to_lowercase().contains(&q))
            });
        }
        entries
    }

    pub(in crate::ui) fn move_palette_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.palette_entries().len();
        if len == 0 {
            return;
        }
        let next = (self.overlays.palette_index as isize + delta).rem_euclid(len as isize) as usize;
        self.overlays.palette_index = next;
        self.overlays.palette_scroll.scroll_to_item(next);
        cx.notify();
    }

    pub(in crate::ui) fn execute_palette_selected(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries = self.palette_entries();
        if let Some(entry) = entries.get(self.overlays.palette_index).cloned() {
            self.run_palette_target(entry.target, window, cx);
        }
    }

    fn run_palette_target(
        &mut self,
        target: PaletteTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlays.palette_open = false;
        self.text_inputs.palette_input.clear();
        self.focus.focus(window);

        match target {
            PaletteTarget::Host(id) => match id {
                "open" => self.open_file(&OpenFile, window, cx),
                "toggle_pause" => self.toggle_pause(&TogglePause, window, cx),
                "toggle_mute" => self.toggle_mute(&ToggleMute, window, cx),
                "toggle_loop" => self.toggle_loop(&ToggleLoop, window, cx),
                "cycle_ab_loop" => self.cycle_ab_loop(&CycleAbLoop, window, cx),
                "clear_ab_loop" => self.clear_ab_loop(&ClearAbLoop, window, cx),
                "cycle_aspect" => self.cycle_aspect(&CycleAspect, window, cx),
                "rotate_cw" => self.rotate_cw(&RotateCw, window, cx),
                "screenshot" => self.take_screenshot(&TakeScreenshot, window, cx),
                "info" => self.toggle_info(&ToggleInfo, window, cx),
                "toggle_sidebar_mode" => {
                    let next = match self.catalog.sidebar_mode {
                        SidebarMode::Files => SidebarMode::Sources,
                        SidebarMode::Sources => SidebarMode::Queue,
                        SidebarMode::Queue => SidebarMode::Plugins,
                        SidebarMode::Plugins => SidebarMode::Files,
                    };
                    self.set_sidebar_mode(next, cx);
                }
                "toggle_directory" => self.toggle_directory(&ToggleDirectory, window, cx),
                "search" => self.toggle_search(&ToggleSearch, window, cx),
                "refresh" => self.refresh_directory(&RefreshDirectory, window, cx),
                "cycle_preset" => self.cycle_preset_action(&CyclePreset, window, cx),
                "settings" => {
                    self.overlays.open_settings(None);
                    cx.notify();
                }
                "shortcuts" => {
                    self.overlays.open_settings(Some(SettingsTab::Shortcuts));
                    cx.notify();
                }
                "plugins" => {
                    self.overlays.open_settings(Some(SettingsTab::Plugins));
                    cx.notify();
                }
                _ => {}
            },
            PaletteTarget::ResetAppearance => self.reset_appearance(window, cx),
            PaletteTarget::Preset(id) => self.activate_preset(&id, window, cx),
            PaletteTarget::SavePreset => self.open_builtin_dialog(
                BuiltinDialog::SavePreset {
                    scope: PresetCaptureScope::default(),
                },
                window,
                cx,
            ),
            PaletteTarget::Theme(id) => {
                self.settings.theme_id = id.to_string();
                self.persist_user_settings();
                self.sync_theme_tokens(cx);
                cx.notify();
            }
            PaletteTarget::OpenUrl => {
                self.open_builtin_dialog(BuiltinDialog::OpenUrl, window, cx);
            }
            PaletteTarget::BrowsePluginSource {
                plugin_id,
                source_id,
            } => {
                self.browse_plugin_source(&plugin_id, &source_id, None, None, None, cx);
            }
            PaletteTarget::Bookmark(bm) => self.open_bookmark(&bm, cx),
            PaletteTarget::PinLocation => self.pin_current_location(cx),
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
        let t = ThemeTokens::of(cx);
        let entries = self.palette_entries();
        let active_ix = self
            .overlays
            .palette_index
            .min(entries.len().saturating_sub(1));
        let viewport = window.viewport_size();
        let margin = 12.;
        let top = (f32::from(viewport.height) * 0.1).clamp(margin, 64.);
        let width = (f32::from(viewport.width) - 2. * margin).clamp(0., 540.);
        let height = (f32::from(viewport.height) - top - margin).clamp(0., 420.);

        div()
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .pt(px(top))
            .bg(t.scrim)
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, w, cx| {
                    this.overlays.palette_open = false;
                    this.text_inputs.palette_input.clear();
                    this.focus.focus(w);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("palette-box")
                    .w(px(width))
                    .max_h(px(height))
                    .flex()
                    .flex_col()
                    .rounded(t.radius_lg)
                    .overflow_hidden()
                    .bg(t.panel)
                    .border_1()
                    .border_color(t.line_strong)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex_none()
                            .min_h(px((f32::from(t.control_h_md) + 12.).max(40.)))
                            .px(px(6.))
                            .py(px(3.))
                            .flex()
                            .items_center()
                            .bg(t.window_bg)
                            .border_b_1()
                            .border_color(t.line)
                            .child(self.render_text_field(
                                TextFieldSpec {
                                    id: "palette-search".into(),
                                    kind: TextFieldKind::Palette,
                                    placeholder: "输入命令、工作区预设、数据源或插件动作…".into(),
                                    height: px((f32::from(t.control_h_md) + 6.).max(34.)),
                                    leading_icon: Some(Icon::Command),
                                    borderless: true,
                                },
                                Some(window),
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .id("palette-list")
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .track_scroll(&self.overlays.palette_scroll)
                            .p(px(6.))
                            .flex()
                            .flex_col()
                            .when(entries.is_empty(), |d| {
                                d.child(
                                    div()
                                        .p(px(14.))
                                        .text_size(t.text_md)
                                        .text_color(t.text_faint)
                                        .child("没有匹配的命令"),
                                )
                            })
                            .children(entries.into_iter().enumerate().map(|(ix, entry)| {
                                let selected = ix == active_ix;
                                let target = entry.target;
                                div()
                                    .id(("pal-row", ix))
                                    .px(px(10.))
                                    .py(px(7.))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(10.))
                                    .rounded(t.radius_sm)
                                    .cursor_pointer()
                                    .when(selected, |d| d.bg(t.selected))
                                    .when(!selected, |d| d.hover(|s| s.bg(t.hover)))
                                    .on_click(cx.listener(move |this, _, w, cx| {
                                        this.run_palette_target(target.clone(), w, cx);
                                    }))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .flex()
                                            .flex_col()
                                            .gap(px(2.))
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_size(t.text_md)
                                                    .text_color(t.text)
                                                    .child(entry.title),
                                            )
                                            .when_some(entry.subtitle, |d, sub| {
                                                d.child(
                                                    div()
                                                        .truncate()
                                                        .text_size(t.text_xs)
                                                        .text_color(t.text_dim)
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
                                            .child(badge_pill(entry.badge, selected, t))
                                            .when_some(entry.shortcut, |d, key| {
                                                d.child(
                                                    div()
                                                        .font_family(FONT_MONO)
                                                        .text_size(t.text_xs)
                                                        .text_color(t.text_dim)
                                                        .child(key),
                                                )
                                            }),
                                    )
                            })),
                    ),
            )
    }
}
