#!/usr/bin/env python3
"""Example out-of-process Iris plugin using the JSON-lines stdio protocol (v1).

Demonstrates:
  - `init` state restoration & `set_state` non-secret persistence across restarts
  - `media_opened` / `tick` playback tracking (`read_state`)
  - `set_status` (`StatusBadge`) & `set_sidebar` (`Widget` tree)
  - `command` / `action` handling (`reset`)

Install by copying this folder into:
  - macOS:   ~/Library/Application Support/Iris/plugins/watch-stats/
  - Windows: %APPDATA%\\Iris\\plugins\\watch-stats\\
Then click "重新扫描插件" in Iris Settings -> 插件.
"""

from __future__ import annotations

import json
import math
import sys
import time
from typing import Any

if hasattr(sys.stdin, "reconfigure"):
    sys.stdin.reconfigure(encoding="utf-8", errors="replace")
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", line_buffering=True)


def fmt_duration(seconds: float) -> str:
    total = max(0, int(seconds))
    h, rem = divmod(total, 3600)
    m, s = divmod(rem, 60)
    return f"{h}:{m:02d}:{s:02d}" if h else f"{m:02d}:{s:02d}"


def emit(commands: list[dict[str, Any]]) -> None:
    if not commands:
        return
    sys.stdout.write(
        json.dumps({"type": "commands", "commands": commands}, ensure_ascii=False) + "\n"
    )
    sys.stdout.flush()


def build_ui(watched: float, opened_count: int, current_title: str) -> list[dict[str, Any]]:
    return [
        {
            "command": "set_status",
            "badge": {
                "id": "watch-timer",
                "text": f"已看 {fmt_duration(watched)}",
                "tooltip": "累计观看时长 · 点击重置",
                "accent": False,
                "action_id": "reset",
            },
        },
        {
            "command": "set_sidebar",
            "panel": {
                "title": "观影计时统计",
                "sections": [
                    {
                        "heading": "播放统计",
                        "widgets": [
                            {
                                "kind": "stat",
                                "label": "累计观看",
                                "value": fmt_duration(watched),
                                "accent": True,
                            },
                            {
                                "kind": "stat",
                                "label": "本次打开媒体",
                                "value": str(opened_count),
                                "accent": False,
                            },
                            {
                                "kind": "label",
                                "text": current_title or "暂无媒体",
                                "muted": True,
                                "mono": False,
                            },
                            {
                                "kind": "button",
                                "label": "重置计时",
                                "action_id": "reset",
                                "accent": False,
                            },
                        ],
                    }
                ],
            },
        },
    ]


def main() -> None:
    watched = 0.0
    opened_count = 0
    current_title = ""
    last_tick: float | None = None
    last_emitted_sec = -1

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
        kind = ev.get("event")

        if kind == "init":
            state = ev.get("state") or {}
            try:
                watched = max(0.0, float(state.get("watched_secs") or 0.0))
                if not math.isfinite(watched):
                    watched = 0.0
            except (TypeError, ValueError, OverflowError):
                watched = 0.0
            snap = ev.get("snapshot") or {}
            current_title = str(snap.get("title") or "")
            last_emitted_sec = int(watched)
            emit(build_ui(watched, opened_count, current_title))

        elif kind == "media_opened":
            snap = ev.get("snapshot") or {}
            opened_count += 1
            current_title = str(snap.get("title") or "")
            last_tick = None
            last_emitted_sec = int(watched)
            emit(build_ui(watched, opened_count, current_title))

        elif kind == "tick":
            now = time.monotonic()
            if not ev.get("paused", True):
                if last_tick is not None:
                    dt = now - last_tick
                    if 0.0 < dt < 5.0:
                        watched += dt
                last_tick = now
                cur_sec = int(watched)
                if cur_sec != last_emitted_sec:
                    last_emitted_sec = cur_sec
                    cmds = build_ui(watched, opened_count, current_title)
                    cmds.append(
                        {
                            "command": "set_state",
                            "key": "watched_secs",
                            "value": f"{watched:.1f}",
                        }
                    )
                    emit(cmds)
            else:
                last_tick = None

        elif kind in ("command", "action"):
            if ev.get("id") == "reset":
                watched = 0.0
                last_tick = None
                last_emitted_sec = 0
                emit(
                    build_ui(watched, opened_count, current_title)
                    + [
                        {"command": "set_state", "key": "watched_secs", "value": None},
                        {"command": "show_toast", "message": "观影计时已重置"},
                    ]
                )

        elif kind == "shutdown":
            break


if __name__ == "__main__":
    main()
