use iris_plugin_sdk::api::v2::HostCommand;
use iris_plugin_sdk::api::{
    ButtonSpec, Color, OverlayNode, SidebarPanel, SidebarSection, StatusBadge, TextAlign, Widget,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum GuideMode {
    #[default]
    Off,
    RuleOfThirds,
    GoldenRatio,
    Crosshair,
    Cinema239,
    ActionSafe,
}

impl GuideMode {
    pub fn as_key(self) -> &'static str {
        match self {
            GuideMode::Off => "off",
            GuideMode::RuleOfThirds => "thirds",
            GuideMode::GoldenRatio => "golden",
            GuideMode::Crosshair => "cross",
            GuideMode::Cinema239 => "cinema",
            GuideMode::ActionSafe => "safe",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
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

    pub fn next(self) -> Self {
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
pub(super) struct GuidesView {
    pub mode: GuideMode,
}

impl GuidesView {
    pub fn sync_ui(&self, persist_and_announce: bool) -> Vec<HostCommand> {
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
            cmds.push(HostCommand::ShowToast {
                message: format!("构图辅助线 · {}", self.mode.label()),
            });
        }
        cmds
    }
}
