//! Built-in official plugins compiled into Iris.
//! They use the exact same [`BuiltinPlugin`] control-plane & vector-overlay
//! protocol as external stdio plugins.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use iris_api::{
    ActionButton, BuiltinPlugin, CommandSpec, HostCommand, HostEvent, MediaKind, OverlayColor,
    OverlayShape, PROTOCOL_VERSION, Permission, PlaybackSnapshot, PluginManifest, SidebarItem,
    SidebarPanel, SidebarSection, StatusItem,
};

// ============================================================================
// Built-in Official Plugin 1: Composition & Safe-Area Vector Guides
// ============================================================================

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum GuideMode {
    #[default]
    Off,
    RuleOfThirds,
    GoldenRatio,
    Crosshair,
    Cinema239,
    ActionSafe,
}

impl GuideMode {
    fn next(self) -> Self {
        match self {
            GuideMode::Off => GuideMode::RuleOfThirds,
            GuideMode::RuleOfThirds => GuideMode::GoldenRatio,
            GuideMode::GoldenRatio => GuideMode::Crosshair,
            GuideMode::Crosshair => GuideMode::Cinema239,
            GuideMode::Cinema239 => GuideMode::ActionSafe,
            GuideMode::ActionSafe => GuideMode::Off,
        }
    }

    fn label(self) -> &'static str {
        match self {
            GuideMode::Off => "关闭",
            GuideMode::RuleOfThirds => "三分线",
            GuideMode::GoldenRatio => "黄金分割",
            GuideMode::Crosshair => "中心十字",
            GuideMode::Cinema239 => "2.39:1 参考框",
            GuideMode::ActionSafe => "安全区 90/80%",
        }
    }

    fn shapes(self) -> Vec<OverlayShape> {
        match self {
            GuideMode::Off => Vec::new(),
            GuideMode::RuleOfThirds => vec![OverlayShape::Grid {
                cols: 3,
                rows: 3,
                color: OverlayColor::White,
                opacity: 0.45,
            }],
            GuideMode::GoldenRatio => vec![
                OverlayShape::Line {
                    x1: 0.382,
                    y1: 0.,
                    x2: 0.382,
                    y2: 1.,
                    color: OverlayColor::Accent,
                    opacity: 0.55,
                    thickness: 1.,
                },
                OverlayShape::Line {
                    x1: 0.618,
                    y1: 0.,
                    x2: 0.618,
                    y2: 1.,
                    color: OverlayColor::Accent,
                    opacity: 0.55,
                    thickness: 1.,
                },
                OverlayShape::Line {
                    x1: 0.,
                    y1: 0.382,
                    x2: 1.,
                    y2: 0.382,
                    color: OverlayColor::Accent,
                    opacity: 0.55,
                    thickness: 1.,
                },
                OverlayShape::Line {
                    x1: 0.,
                    y1: 0.618,
                    x2: 1.,
                    y2: 0.618,
                    color: OverlayColor::Accent,
                    opacity: 0.55,
                    thickness: 1.,
                },
            ],
            GuideMode::Crosshair => vec![OverlayShape::Crosshair {
                x: 0.5,
                y: 0.5,
                size: 0.08,
                color: OverlayColor::Accent,
                opacity: 0.85,
            }],
            GuideMode::Cinema239 => {
                // 2.39:1 frame centered inside a 16:9 viewport (~12.8% matte top/bottom).
                vec![
                    OverlayShape::Rect {
                        x: 0.0,
                        y: 0.128,
                        w: 1.0,
                        h: 0.744,
                        color: OverlayColor::Accent,
                        opacity: 0.75,
                        stroke: 1.0,
                    },
                    OverlayShape::Label {
                        x: 0.015,
                        y: 0.14,
                        text: "2.39:1 SCOPE".into(),
                        color: OverlayColor::Accent,
                    },
                ]
            }
            GuideMode::ActionSafe => vec![
                OverlayShape::Rect {
                    x: 0.05,
                    y: 0.05,
                    w: 0.90,
                    h: 0.90,
                    color: OverlayColor::White,
                    opacity: 0.5,
                    stroke: 1.0,
                },
                OverlayShape::Rect {
                    x: 0.10,
                    y: 0.10,
                    w: 0.80,
                    h: 0.80,
                    color: OverlayColor::Dim,
                    opacity: 0.45,
                    stroke: 1.0,
                },
                OverlayShape::Label {
                    x: 0.06,
                    y: 0.06,
                    text: "SAFE 90% / 80%".into(),
                    color: OverlayColor::White,
                },
            ],
        }
    }
}

#[derive(Default)]
pub(super) struct GuidesPlugin {
    mode: GuideMode,
}

impl GuidesPlugin {
    fn sync_ui(&self, announce: bool) -> Vec<HostCommand> {
        let mut cmds = Vec::new();
        let shapes = self.mode.shapes();
        if shapes.is_empty() {
            cmds.push(HostCommand::ClearOverlay);
            cmds.push(HostCommand::SetStatus { item: None });
        } else {
            cmds.push(HostCommand::SetOverlay { shapes });
            cmds.push(HostCommand::SetStatus {
                item: Some(StatusItem {
                    id: "guide-badge".into(),
                    text: self.mode.label().into(),
                    tooltip: Some("构图辅助线 · 点击切换".into()),
                    accent: true,
                    action_id: Some("cycle".into()),
                }),
            });
        }
        cmds.push(HostCommand::SetSidebar {
            panel: Some(SidebarPanel {
                title: "构图辅助线".into(),
                sections: vec![SidebarSection {
                    heading: "矢量参考线（仅叠加，不改像素）".into(),
                    items: vec![
                        SidebarItem::KeyValue {
                            key: "当前模式".into(),
                            value: self.mode.label().into(),
                            accent: self.mode != GuideMode::Off,
                        },
                        SidebarItem::ButtonRow {
                            buttons: vec![
                                ActionButton {
                                    label: "关闭".into(),
                                    action_id: "off".into(),
                                    accent: self.mode == GuideMode::Off,
                                },
                                ActionButton {
                                    label: "三分线".into(),
                                    action_id: "thirds".into(),
                                    accent: self.mode == GuideMode::RuleOfThirds,
                                },
                                ActionButton {
                                    label: "黄金分割".into(),
                                    action_id: "golden".into(),
                                    accent: self.mode == GuideMode::GoldenRatio,
                                },
                            ],
                        },
                        SidebarItem::ButtonRow {
                            buttons: vec![
                                ActionButton {
                                    label: "十字".into(),
                                    action_id: "cross".into(),
                                    accent: self.mode == GuideMode::Crosshair,
                                },
                                ActionButton {
                                    label: "2.39:1".into(),
                                    action_id: "cinema".into(),
                                    accent: self.mode == GuideMode::Cinema239,
                                },
                                ActionButton {
                                    label: "安全区".into(),
                                    action_id: "safe".into(),
                                    accent: self.mode == GuideMode::ActionSafe,
                                },
                            ],
                        },
                    ],
                }],
            }),
        });
        if announce {
            cmds.push(HostCommand::ShowToast {
                message: format!("构图辅助线 · {}", self.mode.label()),
            });
        }
        cmds
    }

    fn apply_action(&mut self, action: &str) -> Vec<HostCommand> {
        self.mode = match action {
            "cycle" => self.mode.next(),
            "thirds" => GuideMode::RuleOfThirds,
            "golden" => GuideMode::GoldenRatio,
            "cross" => GuideMode::Crosshair,
            "cinema" => GuideMode::Cinema239,
            "safe" => GuideMode::ActionSafe,
            "off" => GuideMode::Off,
            _ => return Vec::new(),
        };
        self.sync_ui(true)
    }
}

impl BuiltinPlugin for GuidesPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "iris.guides".into(),
            name: "构图辅助线".into(),
            version: "0.1.0".into(),
            author: "Iris".into(),
            description:
                "在视口矢量叠加层绘制三分线、黄金分割、中心十字、2.39:1 电影画幅与安全框参考线"
                    .into(),
            protocol: PROTOCOL_VERSION,
            entry: None,
            args: Vec::new(),
            permissions: vec![
                Permission::ReadState,
                Permission::UiOverlay,
                Permission::UiStatus,
                Permission::UiSidebar,
                Permission::Notifications,
            ],
            commands: vec![
                CommandSpec {
                    id: "cycle".into(),
                    title: "构图：切换辅助线模式".into(),
                    subtitle: Some("三分线 / 黄金分割 / 十字 / 2.39:1 / 安全区".into()),
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "thirds".into(),
                    title: "构图：显示三分线 (3×3)".into(),
                    subtitle: None,
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "golden".into(),
                    title: "构图：显示黄金分割线".into(),
                    subtitle: None,
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "cinema".into(),
                    title: "构图：显示 2.39:1 电影画幅框".into(),
                    subtitle: None,
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "off".into(),
                    title: "构图：清除辅助线".into(),
                    subtitle: None,
                    shortcut_hint: None,
                },
            ],
        }
    }

    fn handle_event(&mut self, event: &HostEvent) -> Vec<HostCommand> {
        match event {
            HostEvent::Init { .. } => self.sync_ui(false),
            HostEvent::CommandInvoked { command_id, .. } => self.apply_action(command_id),
            HostEvent::ActionInvoked { action_id, .. } => self.apply_action(action_id),
            _ => Vec::new(),
        }
    }
}

// ============================================================================
// Built-in Official Plugin 2: A-B Loop & Review Markers
// ============================================================================

/// Minimum interval between automatic loop-back seeks to prevent duplicate
/// seeks while mpv is still executing the previous async seek.
const LOOP_SEEK_COOLDOWN: Duration = Duration::from_millis(400);

#[derive(Default)]
pub(super) struct AbLoopPlugin {
    current_path: Option<String>,
    point_a: Option<f64>,
    point_b: Option<f64>,
    markers: BTreeMap<String, Vec<f64>>,
    last_loop_seek: Option<Instant>,
}

impl AbLoopPlugin {
    pub(super) fn fmt_sec(t: f64) -> String {
        let total = t.max(0.) as u64;
        let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
        if h > 0 {
            format!("{h}:{m:02}:{s:02}")
        } else {
            format!("{m:02}:{s:02}")
        }
    }

    fn sync_ui(&self) -> Vec<HostCommand> {
        let mut cmds = Vec::new();
        let status = match (self.point_a, self.point_b) {
            (Some(a), Some(b)) => Some(StatusItem {
                id: "ab-status".into(),
                text: format!("A-B {}–{}", Self::fmt_sec(a), Self::fmt_sec(b)),
                tooltip: Some("A-B 区间循环中 · 点击清除".into()),
                accent: true,
                action_id: Some("clear".into()),
            }),
            (Some(a), None) => Some(StatusItem {
                id: "ab-status".into(),
                text: format!("A {}–?", Self::fmt_sec(a)),
                tooltip: Some("已设置起点 A · 点击清除".into()),
                accent: false,
                action_id: Some("clear".into()),
            }),
            _ => None,
        };
        cmds.push(HostCommand::SetStatus { item: status });

        let mut sections = vec![SidebarSection {
            heading: "A-B 区间循环".into(),
            items: vec![
                SidebarItem::KeyValue {
                    key: "起点 A".into(),
                    value: self.point_a.map_or("未设置".into(), Self::fmt_sec),
                    accent: self.point_a.is_some(),
                },
                SidebarItem::KeyValue {
                    key: "终点 B".into(),
                    value: self.point_b.map_or("未设置".into(), Self::fmt_sec),
                    accent: self.point_b.is_some(),
                },
                SidebarItem::ButtonRow {
                    buttons: vec![
                        ActionButton {
                            label: "设起点 A".into(),
                            action_id: "set_a".into(),
                            accent: false,
                        },
                        ActionButton {
                            label: "设终点 B".into(),
                            action_id: "set_b".into(),
                            accent: false,
                        },
                        ActionButton {
                            label: "清除".into(),
                            action_id: "clear".into(),
                            accent: false,
                        },
                    ],
                },
            ],
        }];

        let mut marker_items = vec![SidebarItem::Button {
            label: "+ 在当前时间添加标记".into(),
            action_id: "add_marker".into(),
            accent: false,
        }];
        if let Some(path) = &self.current_path
            && let Some(list) = self.markers.get(path)
        {
            for (i, &t) in list.iter().enumerate() {
                marker_items.push(SidebarItem::Button {
                    label: format!("#{}  {}", i + 1, Self::fmt_sec(t)),
                    action_id: format!("jump:{t:.3}"),
                    accent: false,
                });
            }
        }
        sections.push(SidebarSection {
            heading: "时间点标记".into(),
            items: marker_items,
        });

        cmds.push(HostCommand::SetSidebar {
            panel: Some(SidebarPanel {
                title: "A-B 循环与标记".into(),
                sections,
            }),
        });
        cmds
    }

    fn is_timed_media(snapshot: &PlaybackSnapshot) -> bool {
        !matches!(snapshot.kind, Some(MediaKind::Photo))
    }

    fn run_action(&mut self, action: &str, snapshot: &PlaybackSnapshot) -> Vec<HostCommand> {
        let mut cmds = Vec::new();
        if let Some(ts) = action.strip_prefix("jump:")
            && let Ok(sec) = ts.parse::<f64>()
        {
            cmds.push(HostCommand::SeekAbsolute { seconds: sec });
            cmds.push(HostCommand::ShowToast {
                message: format!("跳转标记 · {}", Self::fmt_sec(sec)),
            });
            return cmds;
        }
        if matches!(action, "set_a" | "set_b" | "add_marker") && !Self::is_timed_media(snapshot) {
            cmds.push(HostCommand::ShowToast {
                message: "A-B 循环与时间标记仅支持视频或音频".into(),
            });
            return cmds;
        }
        match action {
            "set_a" => {
                self.point_a = Some(snapshot.position.max(0.));
                self.last_loop_seek = None;
                if let Some(b) = self.point_b
                    && b <= snapshot.position
                {
                    self.point_b = None;
                }
                cmds.extend(self.sync_ui());
                cmds.push(HostCommand::ShowToast {
                    message: format!("A-B 起点 A · {}", Self::fmt_sec(snapshot.position)),
                });
            }
            "set_b" => {
                let pos = snapshot.position.max(0.);
                match self.point_a {
                    Some(a) if pos > a + 0.2 => {
                        self.point_b = Some(pos);
                        self.last_loop_seek = None;
                        cmds.extend(self.sync_ui());
                        cmds.push(HostCommand::ShowToast {
                            message: format!(
                                "A-B 循环 · {}–{}",
                                Self::fmt_sec(a),
                                Self::fmt_sec(pos)
                            ),
                        });
                    }
                    _ => {
                        cmds.push(HostCommand::ShowToast {
                            message: "请先设置起点 A，并确保终点 B 在 A 之后".into(),
                        });
                    }
                }
            }
            "clear" => {
                self.point_a = None;
                self.point_b = None;
                self.last_loop_seek = None;
                cmds.extend(self.sync_ui());
                cmds.push(HostCommand::ShowToast {
                    message: "已清除 A-B 循环".into(),
                });
            }
            "add_marker" => {
                if let Some(path) = snapshot.path.clone() {
                    let list = self.markers.entry(path).or_default();
                    let t = snapshot.position.max(0.);
                    list.push(t);
                    list.sort_by(f64::total_cmp);
                    cmds.extend(self.sync_ui());
                    cmds.push(HostCommand::ShowToast {
                        message: format!("已添加时间标记 · {}", Self::fmt_sec(t)),
                    });
                }
            }
            _ => {}
        }
        cmds
    }
}

impl BuiltinPlugin for AbLoopPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "iris.ab-loop".into(),
            name: "A-B 循环与标记".into(),
            version: "0.1.0".into(),
            author: "Iris".into(),
            description: "设置视频 A-B 区间自动循环播放，并记录当前文件的时间点书签以便审片跳转"
                .into(),
            protocol: PROTOCOL_VERSION,
            entry: None,
            args: Vec::new(),
            permissions: vec![
                Permission::ReadState,
                Permission::ControlPlayback,
                Permission::UiSidebar,
                Permission::UiStatus,
                Permission::Notifications,
            ],
            commands: vec![
                CommandSpec {
                    id: "set_a".into(),
                    title: "A-B 循环：设置起点 A".into(),
                    subtitle: Some("以当前播放时间作为循环起点".into()),
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "set_b".into(),
                    title: "A-B 循环：设置终点 B".into(),
                    subtitle: Some("到达 B 点后自动跳回 A 点".into()),
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "clear".into(),
                    title: "A-B 循环：清除区间".into(),
                    subtitle: None,
                    shortcut_hint: None,
                },
                CommandSpec {
                    id: "add_marker".into(),
                    title: "标记：在当前时间添加书签".into(),
                    subtitle: Some("在侧栏插件面板点击即可跳转".into()),
                    shortcut_hint: None,
                },
            ],
        }
    }

    fn handle_event(&mut self, event: &HostEvent) -> Vec<HostCommand> {
        match event {
            HostEvent::Init { .. } => self.sync_ui(),
            HostEvent::FileOpened { snapshot } => {
                self.current_path = snapshot.path.clone();
                self.point_a = None;
                self.point_b = None;
                self.last_loop_seek = None;
                self.sync_ui()
            }
            HostEvent::PositionTick {
                position, paused, ..
            } => {
                if !paused
                    && let (Some(a), Some(b)) = (self.point_a, self.point_b)
                    && *position >= b
                {
                    let cooled = self
                        .last_loop_seek
                        .is_none_or(|t| t.elapsed() >= LOOP_SEEK_COOLDOWN);
                    if cooled {
                        self.last_loop_seek = Some(Instant::now());
                        return vec![HostCommand::SeekAbsolute { seconds: a }];
                    }
                }
                Vec::new()
            }
            HostEvent::CommandInvoked {
                command_id,
                snapshot,
            } => self.run_action(command_id, snapshot),
            HostEvent::ActionInvoked {
                action_id,
                snapshot,
            } => self.run_action(action_id, snapshot),
            _ => Vec::new(),
        }
    }
}
