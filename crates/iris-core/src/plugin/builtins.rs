//! Built-in official plugins compiled into Iris.
//! They use the exact same [`BuiltinPlugin`] control-plane & vector-overlay
//! protocol (`PROTOCOL_VERSION = 1`) as external stdio plugins.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use iris_api::{
    BuiltinPlugin, ButtonSpec, Color, CommandSpec, HostCommand, HostEvent, MediaKind, OverlayNode,
    PROTOCOL_VERSION, Permission, PlaybackSnapshot, PluginManifest, SidebarPanel, SidebarSection,
    StatusBadge, TextAlign, Widget,
};

use crate::player::format_time;

// ============================================================================
// Built-in Official Plugin 1: Composition & Safe-Area Vector Guides
// ============================================================================

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
    fn as_key(self) -> &'static str {
        match self {
            GuideMode::Off => "off",
            GuideMode::RuleOfThirds => "thirds",
            GuideMode::GoldenRatio => "golden",
            GuideMode::Crosshair => "cross",
            GuideMode::Cinema239 => "cinema",
            GuideMode::ActionSafe => "safe",
        }
    }

    fn from_key(s: &str) -> Option<Self> {
        match s {
            "off" => Some(GuideMode::Off),
            "thirds" => Some(GuideMode::RuleOfThirds),
            "golden" => Some(GuideMode::GoldenRatio),
            "cross" => Some(GuideMode::Crosshair),
            "cinema" => Some(GuideMode::Cinema239),
            "safe" => Some(GuideMode::ActionSafe),
            _ => None,
        }
    }

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

    fn nodes(self) -> Vec<OverlayNode> {
        match self {
            GuideMode::Off => Vec::new(),
            GuideMode::RuleOfThirds => vec![OverlayNode::Grid {
                cols: 3,
                rows: 3,
                color: Color::White,
                opacity: 0.45,
            }],
            GuideMode::GoldenRatio => vec![
                OverlayNode::Line {
                    x1: 0.382,
                    y1: 0.0,
                    x2: 0.382,
                    y2: 1.0,
                    color: Color::Accent,
                    width: 1.0,
                    opacity: 0.55,
                },
                OverlayNode::Line {
                    x1: 0.618,
                    y1: 0.0,
                    x2: 0.618,
                    y2: 1.0,
                    color: Color::Accent,
                    width: 1.0,
                    opacity: 0.55,
                },
                OverlayNode::Line {
                    x1: 0.0,
                    y1: 0.382,
                    x2: 1.0,
                    y2: 0.382,
                    color: Color::Accent,
                    width: 1.0,
                    opacity: 0.55,
                },
                OverlayNode::Line {
                    x1: 0.0,
                    y1: 0.618,
                    x2: 1.0,
                    y2: 0.618,
                    color: Color::Accent,
                    width: 1.0,
                    opacity: 0.55,
                },
            ],
            GuideMode::Crosshair => vec![OverlayNode::Crosshair {
                x: 0.5,
                y: 0.5,
                size: 0.08,
                color: Color::Accent,
                opacity: 0.85,
            }],
            GuideMode::Cinema239 => {
                // 2.39:1 frame centered inside a 16:9 viewport (~12.8% matte top/bottom).
                vec![
                    OverlayNode::Box {
                        x: 0.0,
                        y: 0.128,
                        w: 1.0,
                        h: 0.744,
                        fill: None,
                        border: Some(Color::Accent),
                        border_width: 1.0,
                        radius: 0.0,
                        opacity: 0.75,
                    },
                    OverlayNode::Text {
                        x: 0.015,
                        y: 0.14,
                        text: "2.39:1 SCOPE".into(),
                        size: 11.0,
                        color: Color::Accent,
                        bg: Some(Color::Dark),
                        mono: true,
                        align: TextAlign::Left,
                        opacity: 0.9,
                    },
                ]
            }
            GuideMode::ActionSafe => vec![
                OverlayNode::Box {
                    x: 0.05,
                    y: 0.05,
                    w: 0.90,
                    h: 0.90,
                    fill: None,
                    border: Some(Color::White),
                    border_width: 1.0,
                    radius: 0.0,
                    opacity: 0.5,
                },
                OverlayNode::Box {
                    x: 0.10,
                    y: 0.10,
                    w: 0.80,
                    h: 0.80,
                    fill: None,
                    border: Some(Color::Dim),
                    border_width: 1.0,
                    radius: 0.0,
                    opacity: 0.45,
                },
                OverlayNode::Text {
                    x: 0.06,
                    y: 0.06,
                    text: "SAFE 90% / 80%".into(),
                    size: 11.0,
                    color: Color::White,
                    bg: Some(Color::Dark),
                    mono: true,
                    align: TextAlign::Left,
                    opacity: 0.9,
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
    fn sync_ui(&self, persist_and_announce: bool) -> Vec<HostCommand> {
        let mut cmds = Vec::new();
        let nodes = self.mode.nodes();
        if nodes.is_empty() {
            cmds.push(HostCommand::SetOverlay { nodes: Vec::new() });
            cmds.push(HostCommand::SetStatus { badge: None });
        } else {
            cmds.push(HostCommand::SetOverlay { nodes });
            cmds.push(HostCommand::SetStatus {
                badge: Some(Box::new(StatusBadge {
                    id: "guide-badge".into(),
                    text: self.mode.label().into(),
                    tooltip: Some("构图辅助线 · 点击切换".into()),
                    accent: true,
                    action_id: Some("cycle".into()),
                })),
            });
        }
        cmds.push(HostCommand::SetSidebar {
            panel: Some(Box::new(SidebarPanel {
                title: "构图辅助线".into(),
                sections: vec![SidebarSection {
                    heading: "矢量参考线（仅叠加，不改像素）".into(),
                    widgets: vec![
                        Widget::Stat {
                            label: "当前模式".into(),
                            value: self.mode.label().into(),
                            accent: self.mode != GuideMode::Off,
                        },
                        Widget::ButtonGroup {
                            buttons: vec![
                                ButtonSpec {
                                    label: "关闭".into(),
                                    action_id: "off".into(),
                                    icon: None,
                                    accent: self.mode == GuideMode::Off,
                                },
                                ButtonSpec {
                                    label: "三分线".into(),
                                    action_id: "thirds".into(),
                                    icon: None,
                                    accent: self.mode == GuideMode::RuleOfThirds,
                                },
                                ButtonSpec {
                                    label: "黄金分割".into(),
                                    action_id: "golden".into(),
                                    icon: None,
                                    accent: self.mode == GuideMode::GoldenRatio,
                                },
                            ],
                        },
                        Widget::ButtonGroup {
                            buttons: vec![
                                ButtonSpec {
                                    label: "十字".into(),
                                    action_id: "cross".into(),
                                    icon: None,
                                    accent: self.mode == GuideMode::Crosshair,
                                },
                                ButtonSpec {
                                    label: "2.39:1".into(),
                                    action_id: "cinema".into(),
                                    icon: None,
                                    accent: self.mode == GuideMode::Cinema239,
                                },
                                ButtonSpec {
                                    label: "安全区".into(),
                                    action_id: "safe".into(),
                                    icon: None,
                                    accent: self.mode == GuideMode::ActionSafe,
                                },
                            ],
                        },
                    ],
                }],
            })),
        });
        if persist_and_announce {
            cmds.push(HostCommand::SetState {
                key: "mode".into(),
                value: (self.mode != GuideMode::Off).then(|| self.mode.as_key().into()),
            });
            cmds.push(HostCommand::ShowToast {
                message: format!("构图辅助线 · {}", self.mode.label()),
            });
        }
        cmds
    }

    fn apply_action(&mut self, action: &str) -> Vec<HostCommand> {
        self.mode = match action {
            "cycle" => self.mode.next(),
            other => match GuideMode::from_key(other) {
                Some(m) => m,
                None => return Vec::new(),
            },
        };
        self.sync_ui(true)
    }
}

impl BuiltinPlugin for GuidesPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "iris.guides".into(),
            name: "构图辅助线".into(),
            version: env!("CARGO_PKG_VERSION").into(),
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
            ..Default::default()
        }
    }

    fn handle_event(&mut self, event: &HostEvent) -> Vec<HostCommand> {
        match event {
            HostEvent::Init { state, .. } => {
                self.mode = state
                    .get("mode")
                    .and_then(|s| GuideMode::from_key(s))
                    .unwrap_or_default();
                self.sync_ui(false)
            }
            HostEvent::Command { id, .. } | HostEvent::Action { id, .. } => self.apply_action(id),
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
    current_uri: Option<String>,
    point_a: Option<f64>,
    point_b: Option<f64>,
    markers: BTreeMap<String, Vec<f64>>,
    last_loop_seek: Option<Instant>,
}

impl AbLoopPlugin {
    fn sync_ui(&self) -> Vec<HostCommand> {
        let mut cmds = Vec::new();
        let badge = match (self.point_a, self.point_b) {
            (Some(a), Some(b)) => Some(Box::new(StatusBadge {
                id: "ab-status".into(),
                text: format!("A-B {}–{}", format_time(a), format_time(b)),
                tooltip: Some("A-B 区间循环中 · 点击清除".into()),
                accent: true,
                action_id: Some("clear".into()),
            })),
            (Some(a), None) => Some(Box::new(StatusBadge {
                id: "ab-status".into(),
                text: format!("A {}–?", format_time(a)),
                tooltip: Some("已设置起点 A · 点击清除".into()),
                accent: false,
                action_id: Some("clear".into()),
            })),
            _ => None,
        };
        cmds.push(HostCommand::SetStatus { badge });

        let mut sections = vec![SidebarSection {
            heading: "A-B 区间循环".into(),
            widgets: vec![
                Widget::Stat {
                    label: "起点 A".into(),
                    value: self.point_a.map_or("未设置".into(), format_time),
                    accent: self.point_a.is_some(),
                },
                Widget::Stat {
                    label: "终点 B".into(),
                    value: self.point_b.map_or("未设置".into(), format_time),
                    accent: self.point_b.is_some(),
                },
                Widget::ButtonGroup {
                    buttons: vec![
                        ButtonSpec {
                            label: "设起点 A".into(),
                            action_id: "set_a".into(),
                            icon: None,
                            accent: false,
                        },
                        ButtonSpec {
                            label: "设终点 B".into(),
                            action_id: "set_b".into(),
                            icon: None,
                            accent: false,
                        },
                        ButtonSpec {
                            label: "清除".into(),
                            action_id: "clear".into(),
                            icon: None,
                            accent: false,
                        },
                    ],
                },
            ],
        }];

        let mut marker_widgets = vec![Widget::Button {
            label: "+ 在当前时间添加标记".into(),
            action_id: "add_marker".into(),
            icon: None,
            accent: false,
        }];
        if let Some(uri) = &self.current_uri
            && let Some(list) = self.markers.get(uri)
        {
            for (i, &t) in list.iter().enumerate() {
                marker_widgets.push(Widget::Button {
                    label: format!("#{}  {}", i + 1, format_time(t)),
                    action_id: format!("jump:{t:.3}"),
                    icon: None,
                    accent: false,
                });
            }
        }
        sections.push(SidebarSection {
            heading: "时间点标记".into(),
            widgets: marker_widgets,
        });

        cmds.push(HostCommand::SetSidebar {
            panel: Some(Box::new(SidebarPanel {
                title: "A-B 循环与标记".into(),
                sections,
            })),
        });
        cmds
    }

    fn is_timed_media(snapshot: &PlaybackSnapshot) -> bool {
        !matches!(snapshot.kind, Some(MediaKind::Photo))
    }

    fn run_action(&mut self, action: &str, snapshot: &PlaybackSnapshot) -> Vec<HostCommand> {
        let mut cmds = Vec::new();
        if let Some(sec) = action
            .strip_prefix("jump:")
            .and_then(|s| s.parse::<f64>().ok())
        {
            return vec![
                HostCommand::SeekAbsolute { seconds: sec },
                HostCommand::ShowToast {
                    message: format!("跳转标记 · {}", format_time(sec)),
                },
            ];
        }
        if matches!(action, "set_a" | "set_b" | "add_marker") && !Self::is_timed_media(snapshot) {
            return vec![HostCommand::ShowToast {
                message: "A-B 循环与时间标记仅支持视频或音频".into(),
            }];
        }
        match action {
            "set_a" => {
                self.point_a = Some(snapshot.position.max(0.0));
                self.last_loop_seek = None;
                if let Some(b) = self.point_b
                    && b <= snapshot.position
                {
                    self.point_b = None;
                }
                cmds.extend(self.sync_ui());
                cmds.push(HostCommand::ShowToast {
                    message: format!("A-B 起点 A · {}", format_time(snapshot.position)),
                });
            }
            "set_b" => {
                let pos = snapshot.position.max(0.0);
                match self.point_a {
                    Some(a) if pos > a + 0.2 => {
                        self.point_b = Some(pos);
                        self.last_loop_seek = None;
                        cmds.extend(self.sync_ui());
                        cmds.push(HostCommand::ShowToast {
                            message: format!("A-B 循环 · {}–{}", format_time(a), format_time(pos)),
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
                (self.point_a, self.point_b, self.last_loop_seek) = (None, None, None);
                cmds.extend(self.sync_ui());
                cmds.push(HostCommand::ShowToast {
                    message: "已清除 A-B 循环".into(),
                });
            }
            "add_marker" => {
                if let Some(uri) = snapshot.uri.clone().or_else(|| self.current_uri.clone()) {
                    self.current_uri.get_or_insert_with(|| uri.clone());
                    let list = self.markers.entry(uri).or_default();
                    let t = snapshot.position.max(0.0);
                    list.push(t);
                    list.sort_by(f64::total_cmp);
                    cmds.extend(self.sync_ui());
                    let msg = format!("已添加时间标记 · {}", format_time(t));
                    cmds.push(HostCommand::ShowToast { message: msg });
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
            version: env!("CARGO_PKG_VERSION").into(),
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
            commands: [
                (
                    "set_a",
                    "A-B 循环：设置起点 A",
                    Some("以当前播放时间作为循环起点"),
                ),
                (
                    "set_b",
                    "A-B 循环：设置终点 B",
                    Some("到达 B 点后自动跳回 A 点"),
                ),
                ("clear", "A-B 循环：清除区间", None),
                (
                    "add_marker",
                    "标记：在当前时间添加书签",
                    Some("在侧栏插件面板点击即可跳转"),
                ),
            ]
            .into_iter()
            .map(|(id, title, subtitle)| CommandSpec {
                id: id.into(),
                title: title.into(),
                subtitle: subtitle.map(Into::into),
                shortcut_hint: None,
            })
            .collect(),
            ..Default::default()
        }
    }

    fn handle_event(&mut self, event: &HostEvent) -> Vec<HostCommand> {
        match event {
            HostEvent::Init { snapshot, .. } => {
                self.current_uri = snapshot.uri.clone();
                self.sync_ui()
            }
            HostEvent::MediaOpened { snapshot } => {
                self.current_uri = snapshot.uri.clone();
                self.point_a = None;
                self.point_b = None;
                self.last_loop_seek = None;
                self.sync_ui()
            }
            HostEvent::Tick {
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
            HostEvent::Command { id, snapshot } | HostEvent::Action { id, snapshot, .. } => {
                self.run_action(id, snapshot)
            }
            _ => Vec::new(),
        }
    }
}
