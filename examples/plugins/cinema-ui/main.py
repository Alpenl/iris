#!/usr/bin/env python3
"""Official Iris reference plugin (protocol v1): Cinema HUD & Dynamic Theme Workbench.

Demonstrates parameterized UI workbench customization over the safe control plane:
  - `set_toolbar`: contributes native-icon buttons (`layers`, `sliders`) to the dock
  - `set_theme`: live runtime `UiThemeSpec` hex palette + `corner_radius` + `ChromeLayoutSpec`
    (including frameless borderless window + floating dock mode)
  - `set_overlay`: `Box` translucent card + `Text` SMPTE timecode (`HH:MM:SS.d`)
    + animated `Bars` spectrum/progress + clickable `Button` ("+15s 跳过片头")
  - `set_sidebar`: rich interactive widgets (`Toggle`, `Slider`, `Select`, `Input`, `ListItem`)
  - `set_state`: persists active skin, HUD toggle, opacity, and tag across restarts

Install by copying this folder into:
  - macOS:   ~/Library/Application Support/Iris/plugins/cinema-ui/
  - Windows: %APPDATA%\\Iris\\plugins\\cinema-ui\\
Then click "重新扫描插件" in Iris Settings -> 插件.
"""

from __future__ import annotations

import json
import math
import sys
from typing import Any

if hasattr(sys.stdin, "reconfigure"):
    sys.stdin.reconfigure(encoding="utf-8", errors="replace")
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", line_buffering=True)

SKIN_ORDER = ["default", "neon", "amber", "minimal", "frameless"]

def _theme(
    name: str, bg: str, panel: str, hover: str, sel: str, line: str, line_s: str,
    text: str, muted: str, dim: str, faint: str, accent: str, ov: str, scrim: str,
    tb_h: float, dock_h: float, row_h: float, radius: float,
) -> dict[str, Any]:
    return {
        "name": name, "window_bg": bg, "panel": panel, "hover": hover, "selected": sel,
        "line": line, "line_strong": line_s, "text": text, "text_muted": muted,
        "text_dim": dim, "text_faint": faint, "accent": accent, "overlay": ov,
        "scrim": scrim, "titlebar_height": tb_h, "dock_height": dock_h,
        "row_height": row_h, "corner_radius": radius,
    }


def _chrome(
    hide_tb: bool, float_dock: bool, meta: bool, compact: bool, hidden: list[str]
) -> dict[str, Any]:
    return {
        "hide_titlebar": hide_tb, "floating_dock": float_dock,
        "show_titlebar_meta": meta, "compact_controls": compact,
        "hidden_dock_buttons": hidden,
    }


SKINS: dict[str, dict[str, Any]] = {
    "default": {
        "id": "default", "label": "默认还原", "short": "默认", "badge": "HOST",
        "subtitle": "还原宿主当前内置主题与标准控制栏密度", "hud_bg": "#15171AD9",
        "theme": None, "chrome": None,
    },
    "neon": {
        "id": "neon", "label": "赛博霓虹 Neon", "short": "霓虹", "badge": "R8 · FLOAT",
        "subtitle": "深邃午夜蓝底色 + 青色电光强调 #00F0FF + 圆角悬浮底栏", "hud_bg": "#0E1024DE",
        "theme": _theme(
            "赛博霓虹 Neon", "#070812", "#0E1022", "#181C38", "#232952", "#2A3266", "#3E4B94",
            "#ECF4FF", "#A9BAE0", "#788BB8", "#4D5D85", "#00F0FF", "#0E1022E6", "#03040AB3",
            38.0, 48.0, 30.0, 8.0,
        ),
        "chrome": _chrome(False, True, True, False, []),
    },
    "amber": {
        "id": "amber", "label": "暗房胶片 Amber", "short": "琥珀", "badge": "R2 · DOCK",
        "subtitle": "胶片暗房暖黑底色 + 琥珀金 #FFB020 + 审片级微圆角", "hud_bg": "#1A1511E0",
        "theme": _theme(
            "暗房胶片 Amber", "#110E0B", "#1A1511", "#261F19", "#332920", "#3B2F25", "#544335",
            "#F2E8DC", "#C4B5A3", "#948575", "#665A4E", "#FFB020", "#1A1511EB", "#080604B3",
            36.0, 44.0, 28.0, 2.0,
        ),
        "chrome": _chrome(False, False, True, False, []),
    },
    "minimal": {
        "id": "minimal", "label": "极简悬浮窗 Minimal", "short": "极简", "badge": "R12 · COMPACT",
        "subtitle": "大圆角紧凑胶囊悬浮栏 + 自动收纳低频按钮", "hud_bg": "#0D0F14DE",
        "theme": _theme(
            "极简悬浮窗 Minimal", "#050608", "#0D0F14", "#171B24", "#202633", "#232936", "#343D4F",
            "#F0F3F8", "#B0B8C5", "#7D8694", "#505763", "#A78BFA", "#0D0F14D9", "#000000AD",
            30.0, 38.0, 25.0, 12.0,
        ),
        "chrome": _chrome(False, True, False, True, ["loop", "aspect", "screenshot"]),
    },
    "frameless": {
        "id": "frameless", "label": "无边框悬浮窗 Frameless", "short": "无边框", "badge": "FRAMELESS",
        "subtitle": "隐藏窗口标题栏 + 纯黑 OLED 沉浸画布 + 悬浮胶囊底栏", "hud_bg": "#08090CE0",
        "theme": _theme(
            "无边框悬浮窗 Frameless", "#000000", "#0C0D10", "#17191F", "#22252E", "#252932", "#383E4C",
            "#F5F6F8", "#B4B9C4", "#7E8592", "#505662", "#FF5B1F", "#0C0D10E6", "#000000BF",
            28.0, 40.0, 26.0, 10.0,
        ),
        "chrome": _chrome(True, True, False, True, ["loop", "aspect"]),
    },
}


def emit(commands: list[dict[str, Any]]) -> None:
    if not commands:
        return
    sys.stdout.write(
        json.dumps({"type": "commands", "commands": commands}, ensure_ascii=False) + "\n"
    )
    sys.stdout.flush()


def parse_toggle(val: str | None, current: bool) -> bool:
    if val is None:
        return not current
    low = val.strip().lower()
    if low in ("true", "1", "yes", "on"):
        return True
    if low in ("false", "0", "no", "off"):
        return False
    return not current


def finite_number(value: Any, fallback: float) -> float:
    try:
        parsed = float(value)
    except (TypeError, ValueError, OverflowError):
        return fallback
    return parsed if math.isfinite(parsed) else fallback


def fmt_smpte(seconds: float) -> str:
    """Formats playback time as SMPTE-style `HH:MM:SS.d`."""
    clamped = max(0.0, finite_number(seconds, 0.0))
    whole = int(clamped)
    tenths = int((clamped - whole) * 10.0) % 10
    h, rem = divmod(whole, 3600)
    m, s = divmod(rem, 60)
    return f"{h:02d}:{m:02d}:{s:02d}.{tenths}"


def compute_spectrum_bars(
    position: float, duration: float, paused: bool, count: int = 16
) -> list[float]:
    """Synthesizes smooth animated spectrum / timeline energy bars in `[0.12, 0.96]`."""
    if paused:
        return [round(0.28 + 0.14 * math.sin(i * 0.65), 3) for i in range(count)]
    ratio = max(0.0, min(1.0, (position / duration) if duration > 0.0 else 0.0))
    phase = position * 4.2
    bars: list[float] = []
    for i in range(count):
        slot = i / max(1, count - 1)
        wave = (
            0.48
            + 0.26 * math.sin(phase + i * 0.58)
            + 0.16 * math.cos(phase * 1.7 - i * 0.93)
        )
        boost = 0.12 if slot <= ratio else -0.06
        bars.append(round(max(0.12, min(0.96, wave + boost)), 3))
    return bars


class CinemaUiPlugin:
    def __init__(self) -> None:
        self.skin_id: str = "default"
        self.hud_enabled: bool = False
        self.force_floating_dock: bool = False
        self.hud_opacity: float = 90.0
        self.hud_tag: str = "CINEMA"
        self.position: float = 0.0
        self.duration: float = 0.0
        self.paused: bool = True
        self.speed: float = 1.0
        self.title: str = ""
        self.last_hud_bucket: tuple[int, bool] | None = None

    @property
    def current_skin(self) -> dict[str, Any]:
        return SKINS[self.skin_id]

    def build_toolbar_cmd(self) -> dict[str, Any]:
        skin = self.current_skin
        return {
            "command": "set_toolbar",
            "buttons": [
                {
                    "id": "hud",
                    "label": "HUD",
                    "icon": "layers",
                    "tooltip": "开关影院 SMPTE 时间码与动态频谱 HUD",
                    "active": self.hud_enabled,
                    "accent": self.hud_enabled,
                    "action_id": "toggle_hud",
                },
                {
                    "id": "skin",
                    "label": skin["short"],
                    "icon": "sliders",
                    "tooltip": f"当前皮肤：{skin['label']} · 点击循环切换主题外观",
                    "active": self.skin_id != "default",
                    "accent": self.skin_id != "default",
                    "action_id": "cycle_skin",
                },
            ],
        }

    def build_status_cmd(self) -> dict[str, Any]:
        skin = self.current_skin
        hud_flag = "HUD ON" if self.hud_enabled else "HUD OFF"
        return {
            "command": "set_status",
            "badge": {
                "id": "cinema-badge",
                "text": f"{skin['short']} · {hud_flag}",
                "tooltip": "点击循环切换影院皮肤预设",
                "accent": self.skin_id != "default" or self.hud_enabled,
                "action_id": "cycle_skin",
            },
        }

    def build_theme_cmd(self) -> dict[str, Any]:
        skin = self.current_skin
        theme = dict(skin["theme"]) if skin["theme"] is not None else None
        if skin["chrome"] is not None:
            chrome: dict[str, Any] | None = dict(skin["chrome"])
            if self.force_floating_dock:
                chrome["floating_dock"] = True
        elif self.force_floating_dock:
            chrome = {
                "floating_dock": True,
                "show_titlebar_meta": True,
                "compact_controls": False,
                "hidden_dock_buttons": [],
            }
        else:
            chrome = None
        return {
            "command": "set_theme",
            "theme": theme,
            "chrome": chrome,
        }

    def build_overlay_cmd(self) -> dict[str, Any]:
        if not self.hud_enabled:
            return {"command": "set_overlay", "nodes": []}

        skin = self.current_skin
        tc_line = f"{fmt_smpte(self.position)} / {fmt_smpte(self.duration)}"
        state_str = "PAUSE" if self.paused else f"PLAY {self.speed:.2g}x"
        header_line = f"{self.hud_tag} · {state_str}"
        bars = compute_spectrum_bars(self.position, self.duration, self.paused)
        scale = max(0.3, min(1.0, self.hud_opacity / 100.0))

        nodes: list[dict[str, Any]] = [
            {
                "kind": "box",
                "x": 0.705,
                "y": 0.04,
                "w": 0.265,
                "h": 0.148,
                "fill": skin["hud_bg"],
                "border": "accent",
                "border_width": 1.0,
                "radius": 8.0,
                "opacity": round(0.92 * scale, 3),
            },
            {
                "kind": "text",
                "x": 0.722,
                "y": 0.053,
                "text": header_line,
                "size": 11.0,
                "color": "accent",
                "bg": None,
                "mono": True,
                "align": "left",
                "opacity": round(0.95 * scale, 3),
            },
            {
                "kind": "text",
                "x": 0.722,
                "y": 0.081,
                "text": tc_line,
                "size": 14.0,
                "color": "white",
                "bg": None,
                "mono": True,
                "align": "left",
                "opacity": round(0.98 * scale, 3),
            },
            {
                "kind": "bars",
                "x": 0.722,
                "y": 0.122,
                "w": 0.231,
                "h": 0.048,
                "values": bars,
                "color": "accent",
                "opacity": round(0.86 * scale, 3),
            },
            {
                "kind": "button",
                "id": "skip-intro-btn",
                "x": 0.835,
                "y": 0.202,
                "label": "+15s 跳过片头",
                "accent": True,
                "action_id": "skip_intro",
            },
        ]
        return {"command": "set_overlay", "nodes": nodes}

    def build_sidebar_cmd(self) -> dict[str, Any]:
        skin_options = [
            {"value": sid, "label": SKINS[sid]["label"]} for sid in SKIN_ORDER
        ]
        preset_widgets: list[dict[str, Any]] = [
            {
                "kind": "list_item",
                "id": f"preset-{sid}",
                "title": SKINS[sid]["label"],
                "subtitle": SKINS[sid]["subtitle"],
                "badge": SKINS[sid]["badge"],
                "active": self.skin_id == sid,
                "action_id": f"apply_skin:{sid}",
            }
            for sid in SKIN_ORDER
        ]

        effective_float = self.force_floating_dock or bool(
            (self.current_skin["chrome"] or {}).get("floating_dock", False)
        )

        return {
            "command": "set_sidebar",
            "panel": {
                "title": "影院 HUD 与主题工作台",
                "sections": [
                    {
                        "heading": "视口叠层与快捷控制",
                        "widgets": [
                            {
                                "kind": "toggle",
                                "id": "hud-toggle",
                                "label": "实时 SMPTE 时间码与动态频谱 HUD",
                                "checked": self.hud_enabled,
                                "action_id": "toggle_hud",
                            },
                            {
                                "kind": "toggle",
                                "id": "float-dock-toggle",
                                "label": "强制窗口模式悬浮玻璃底栏",
                                "checked": effective_float,
                                "action_id": "toggle_float_dock",
                            },
                            {
                                "kind": "slider",
                                "id": "hud-opacity-slider",
                                "label": "HUD 叠层不透明度",
                                "value": self.hud_opacity,
                                "min": 30.0,
                                "max": 100.0,
                                "step": 5.0,
                                "unit": "%",
                                "action_id": "set_hud_opacity",
                            },
                            {
                                "kind": "input",
                                "id": "hud-tag-input",
                                "label": "HUD 机位 / 审片抬头",
                                "value": self.hud_tag,
                                "placeholder": "输入自定义抬头（如 REEL-A / IMAX），回车生效",
                                "secret": False,
                                "action_id": "set_hud_tag",
                            },
                            {
                                "kind": "button_group",
                                "buttons": [
                                    {
                                        "label": "+15s 跳过片头",
                                        "action_id": "skip_intro",
                                        "accent": True,
                                    },
                                    {
                                        "label": "恢复默认外观",
                                        "action_id": "reset_skin",
                                        "accent": False,
                                    },
                                ],
                            },
                        ],
                    },
                    {
                        "heading": "动态主题与无边框布局",
                        "widgets": [
                            {
                                "kind": "select",
                                "id": "skin-select",
                                "label": "当前激活皮肤",
                                "selected": self.skin_id,
                                "options": skin_options,
                                "action_id": "select_skin",
                            },
                            {"kind": "divider"},
                            *preset_widgets,
                        ],
                    },
                ],
            },
        }

    def sync_all_ui(self) -> list[dict[str, Any]]:
        self.last_hud_bucket = (int(self.position * 10.0), self.paused)
        return [
            self.build_toolbar_cmd(),
            self.build_status_cmd(),
            self.build_sidebar_cmd(),
            self.build_overlay_cmd(),
        ]

    def restore_state(self, state: dict[str, Any]) -> None:
        saved_skin = str(state.get("skin") or "default").strip()
        if saved_skin in SKINS:
            self.skin_id = saved_skin
        self.hud_enabled = state.get("hud_enabled") == "true"
        self.force_floating_dock = state.get("force_floating_dock") == "true"
        saved_tag = str(state.get("hud_tag") or "").strip()
        if saved_tag:
            self.hud_tag = saved_tag[:24]
        opacity = finite_number(state.get("hud_opacity"), self.hud_opacity)
        self.hud_opacity = max(30.0, min(100.0, opacity))

    def update_from_snapshot(self, snap: dict[str, Any]) -> None:
        if not snap:
            return
        self.position = finite_number(snap.get("position"), 0.0)
        self.duration = finite_number(snap.get("duration"), 0.0)
        self.paused = bool(snap.get("paused", True))
        self.speed = finite_number(snap.get("speed"), 1.0)
        self.title = str(snap.get("title") or "")

    def apply_skin(self, target_id: str) -> list[dict[str, Any]]:
        if target_id not in SKINS:
            return []
        self.skin_id = target_id
        self.last_hud_bucket = None
        cmds = [
            self.build_theme_cmd(),
            {
                "command": "set_state",
                "key": "skin",
                "value": None if self.skin_id == "default" else self.skin_id,
            },
        ]
        cmds.extend(self.sync_all_ui())
        cmds.append(
            {"command": "show_toast", "message": f"界面皮肤已切换：{self.current_skin['label']}"}
        )
        return cmds

    def handle_action(self, act: str, value: str | None = None) -> list[dict[str, Any]]:
        if act in ("toggle_hud", "hud", "hud-toggle"):
            self.hud_enabled = parse_toggle(value, self.hud_enabled)
            self.last_hud_bucket = None
            cmds = [
                {
                    "command": "set_state",
                    "key": "hud_enabled",
                    "value": "true" if self.hud_enabled else None,
                }
            ]
            cmds.extend(self.sync_all_ui())
            msg = "影院时间码 HUD 已开启" if self.hud_enabled else "影院时间码 HUD 已关闭"
            cmds.append({"command": "show_toast", "message": msg})
            return cmds

        if act in ("cycle_skin", "skin"):
            idx = SKIN_ORDER.index(self.skin_id)
            next_id = SKIN_ORDER[(idx + 1) % len(SKIN_ORDER)]
            return self.apply_skin(next_id)

        if act == "reset_skin":
            self.force_floating_dock = False
            cmds = [
                {"command": "set_state", "key": "force_floating_dock", "value": None},
            ]
            cmds.extend(self.apply_skin("default"))
            return cmds

        if act in ("toggle_float_dock", "float-dock-toggle"):
            self.force_floating_dock = parse_toggle(value, self.force_floating_dock)
            cmds = [
                self.build_theme_cmd(),
                {
                    "command": "set_state",
                    "key": "force_floating_dock",
                    "value": "true" if self.force_floating_dock else None,
                },
            ]
            cmds.extend(self.sync_all_ui())
            state_txt = "已开启悬浮底栏" if self.force_floating_dock else "已跟随皮肤默认底栏"
            cmds.append({"command": "show_toast", "message": state_txt})
            return cmds

        if act in ("set_hud_opacity", "hud-opacity-slider"):
            val = finite_number(value, self.hud_opacity)
            self.hud_opacity = round(max(30.0, min(100.0, val)))
            self.last_hud_bucket = (int(self.position * 10.0), self.paused)
            return [
                {
                    "command": "set_state",
                    "key": "hud_opacity",
                    "value": None if self.hud_opacity == 90.0 else f"{self.hud_opacity:.0f}",
                },
                self.build_sidebar_cmd(),
                self.build_overlay_cmd(),
            ]

        if act in ("set_hud_tag", "hud-tag-input"):
            val_str = (value or "").strip()
            self.hud_tag = val_str[:24] if val_str else "CINEMA"
            self.last_hud_bucket = (int(self.position * 10.0), self.paused)
            return [
                {
                    "command": "set_state",
                    "key": "hud_tag",
                    "value": None if self.hud_tag == "CINEMA" else self.hud_tag,
                },
                self.build_sidebar_cmd(),
                self.build_overlay_cmd(),
                {"command": "show_toast", "message": f"HUD 抬头已更新：{self.hud_tag}"},
            ]

        if act in ("select_skin", "skin-select"):
            return self.apply_skin((value or "").strip())

        if act == "skip_intro":
            return [
                {"command": "seek_relative", "seconds": 15.0},
                {"command": "show_toast", "message": "已跳过片头 +15s"},
            ]

        if act.startswith("apply_skin:"):
            target = act.split(":", 1)[1]
            return self.apply_skin(target)

        return []

    def handle_event(self, ev: dict[str, Any]) -> list[dict[str, Any]]:
        kind = ev.get("event")

        if kind == "init":
            self.restore_state(ev.get("state") or {})
            self.update_from_snapshot(ev.get("snapshot") or {})
            cmds: list[dict[str, Any]] = []
            if self.skin_id != "default" or self.force_floating_dock:
                cmds.append(self.build_theme_cmd())
            cmds.extend(self.sync_all_ui())
            return cmds

        if kind in ("media_opened", "state_changed"):
            self.update_from_snapshot(ev.get("snapshot") or {})
            if self.hud_enabled:
                self.last_hud_bucket = (int(self.position * 10.0), self.paused)
                return [self.build_overlay_cmd()]
            return []

        if kind == "tick":
            was_paused = self.paused
            self.position = finite_number(ev.get("position"), 0.0)
            self.duration = finite_number(ev.get("duration"), 0.0)
            self.paused = bool(ev.get("paused", True))
            if not self.hud_enabled:
                return []
            if self.paused:
                if not was_paused or self.last_hud_bucket is None:
                    self.last_hud_bucket = (int(self.position * 10.0), True)
                    return [self.build_overlay_cmd()]
                return []
            bucket = (int(self.position * 10.0), False)
            if bucket != self.last_hud_bucket:
                self.last_hud_bucket = bucket
                return [self.build_overlay_cmd()]
            return []

        if kind == "command":
            self.update_from_snapshot(ev.get("snapshot") or {})
            return self.handle_action(str(ev.get("id") or ""), None)

        if kind == "action":
            self.update_from_snapshot(ev.get("snapshot") or {})
            return self.handle_action(str(ev.get("id") or ""), ev.get("value"))

        return []


def main() -> None:
    plugin = CinemaUiPlugin()
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("type") != "event":
            continue
        ev = msg.get("payload") or {}
        if ev.get("event") == "shutdown":
            break
        emit(plugin.handle_event(ev))


if __name__ == "__main__":
    main()
