#!/usr/bin/env python3
"""Example out-of-process Iris plugin using the JSON-lines stdio protocol v1.

Install by copying this folder into:
  - macOS:   ~/Library/Application Support/Iris/plugins/watch-stats/
  - Windows: %APPDATA%\\Iris\\plugins\\watch-stats\\
Then click "重新扫描插件" in Iris Settings -> 插件.
"""

import json
import sys
import time


def fmt_duration(seconds: float) -> str:
    total = max(0, int(seconds))
    h, rem = divmod(total, 3600)
    m, s = divmod(rem, 60)
    return f"{h}:{m:02d}:{s:02d}" if h else f"{m:02d}:{s:02d}"


def emit(commands: list[dict]) -> None:
    sys.stdout.write(json.dumps({"type": "commands", "commands": commands}, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def build_ui(watched: float, opened_count: int, current_file: str) -> list[dict]:
    return [
        {
            "command": "set_status",
            "item": {
                "id": "watch-timer",
                "text": f"已看 {fmt_duration(watched)}",
                "tooltip": "本次会话累计观看时长 · 点击重置",
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
                        "heading": "本次会话",
                        "items": [
                            {"kind": "key_value", "key": "累计观看", "value": fmt_duration(watched), "accent": True},
                            {"kind": "key_value", "key": "已打开文件", "value": str(opened_count), "accent": False},
                            {"kind": "text", "text": current_file or "暂无媒体", "muted": True},
                            {"kind": "button", "label": "重置计时", "action_id": "reset", "accent": False},
                        ],
                    }
                ],
            },
        },
    ]


def main() -> None:
    watched = 0.0
    opened_count = 0
    current_file = ""
    last_tick = None
    last_emitted_sec = -1

    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if msg.get("type") != "event":
            continue
        ev = msg.get("payload") or {}
        kind = ev.get("event")

        if kind == "init":
            last_emitted_sec = int(watched)
            emit(build_ui(watched, opened_count, current_file))
        elif kind == "file_opened":
            snap = ev.get("snapshot") or {}
            opened_count += 1
            current_file = snap.get("file_name") or ""
            last_tick = None
            last_emitted_sec = int(watched)
            emit(build_ui(watched, opened_count, current_file))
        elif kind == "position_tick":
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
                    emit(build_ui(watched, opened_count, current_file))
            else:
                last_tick = None
        elif kind in ("command_invoked", "action_invoked"):
            act = ev.get("command_id") or ev.get("action_id")
            if act == "reset":
                watched = 0.0
                last_tick = None
                last_emitted_sec = 0
                emit(
                    build_ui(watched, opened_count, current_file)
                    + [{"command": "show_toast", "message": "观影计时已重置"}]
                )
        elif kind == "shutdown":
            break


if __name__ == "__main__":
    main()
