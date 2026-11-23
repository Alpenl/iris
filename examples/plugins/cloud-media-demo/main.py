#!/usr/bin/env python3
"""Official Iris reference plugin (protocol v1): Cloud Drive & Video Source Demo.

Demonstrates:
  - Virtual media sources (`source_browse` / `provide_source_page`, `source_resolve` / `resolve_stream`)
  - Unified widget modal with scannable QR code + secret token (`open_modal` / `modal_submit` / `modal_cancel`)
  - Atomic `init` delivery & persistence of `secrets` (`set_secret`) and non-secret `state` (`set_state`)
  - Multi-bitrate quality variants (`1080p` / `720p`) and custom HTTP gateway headers

Install by copying this folder into:
  - macOS:   ~/Library/Application Support/Iris/plugins/cloud-media-demo/
  - Windows: %APPDATA%\\Iris\\plugins\\cloud-media-demo\\
Then click "重新扫描插件" in Iris Settings -> 插件.
"""

from __future__ import annotations

import json
import sys
from typing import Any

if hasattr(sys.stdin, "reconfigure"):
    sys.stdin.reconfigure(encoding="utf-8", errors="replace")
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", line_buffering=True)

SOURCE_ID = "demo-drive"
AUTH_HINT = "请先点击登录按钮连接云盘（支持扫码或任意测试 Token）"

VARIANTS = [
    {"id": "1080p", "label": "1080P 高清"},
    {"id": "720p", "label": "720P 流畅"},
]

CDN_OPTIONS = [
    {"value": "auto", "label": "智能优选 (auto)"},
    {"value": "cn", "label": "亚太加速 (cn)"},
    {"value": "global", "label": "全球节点 (global)"},
]

# Authentic 15x15 QR pattern matrix with three 7x7 finder squares & quiet separators.
QR_MATRIX_15X15 = [
    "111111101111111",
    "100000101000001",
    "101110101011101",
    "101110101011101",
    "101110101011101",
    "100000101000001",
    "111111101111111",
    "000000000000000",
    "111111101011010",
    "100000100110101",
    "101110101101011",
    "101110100101100",
    "101110101010111",
    "100000101100101",
    "111111100111011",
]

ROOT_FOLDERS = [
    {
        "id": "movies",
        "name": "4K 演示片库",
        "kind": "folder",
        "subtitle": "开放版权短片 · 支持 1080P / 720P 清晰度切换",
        "variants": [],
    },
    {
        "id": "photos",
        "name": "广色域在线图集",
        "kind": "folder",
        "subtitle": "高分辨率风光与几何样张 · 在线直读预览",
        "variants": [],
    },
    {
        "id": "live",
        "name": "公开测试频道",
        "kind": "folder",
        "subtitle": "HLS 自适应低延迟测试流",
        "variants": [],
    },
]

MEDIA_CATALOG: dict[str, list[dict[str, Any]]] = {
    "movies": [
        {
            "id": "bbb",
            "name": "Big Buck Bunny (Blender 开放电影)",
            "kind": "video",
            "size": 158008374,
            "duration": 596.0,
            "subtitle": "H.264 · 双清晰度测试源",
            "variants": VARIANTS,
            "urls": {
                "1080p": "https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4",
                "720p": "https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/ForBiggerBlazes.mp4",
            },
        },
        {
            "id": "sintel",
            "name": "Sintel 宽银幕样片 (Blender Studio)",
            "kind": "video",
            "size": 129241752,
            "duration": 888.0,
            "subtitle": "2.35:1 暗场调色与字幕审片",
            "variants": VARIANTS,
            "urls": {
                "1080p": "https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/Sintel.mp4",
                "720p": "https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/ForBiggerEscapes.mp4",
            },
        },
        {
            "id": "tears",
            "name": "Tears of Steel 科幻合成测试",
            "kind": "video",
            "size": 185400000,
            "duration": 734.0,
            "subtitle": "实拍与 CG 合成高对比样片",
            "variants": VARIANTS,
            "urls": {
                "1080p": "https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/TearsOfSteel.mp4",
                "720p": "https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/ForBiggerFun.mp4",
            },
        },
    ],
    "photos": [
        {
            "id": "photo-alps",
            "name": "阿尔卑斯雪峰晨光.jpg",
            "kind": "photo",
            "size": 2840000,
            "subtitle": "JPEG · 高动态范围自然风光样张",
            "variants": [],
            "urls": {
                "default": "https://upload.wikimedia.org/wikipedia/commons/e/e7/Everest_North_Face_toward_Base_Camp_Tibet_Luca_Galuzzi_2006.jpg",
            },
        },
        {
            "id": "photo-canyon",
            "name": "暖色岩层纹理审片.jpg",
            "kind": "photo",
            "size": 3120000,
            "subtitle": "JPEG · 暗部与暖调层次测试",
            "variants": [],
            "urls": {
                "default": "https://upload.wikimedia.org/wikipedia/commons/a/a9/Grand_Canyon_view_from_Pima_Point_2010.jpg",
            },
        },
    ],
    "live": [
        {
            "id": "live-bipbop",
            "name": "Apple HLS 多码率基准信号",
            "kind": "live",
            "subtitle": "HLS fMP4 · 实时流时钟与缓冲测试",
            "variants": VARIANTS,
            "is_live": True,
            "urls": {
                "1080p": "https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_fmp4/master.m3u8",
                "720p": "https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_fmp4/master.m3u8",
            },
        },
        {
            "id": "live-test",
            "name": "公开演播室自适应测试流",
            "kind": "live",
            "subtitle": "HLS · CDN 边缘节点连通性演示",
            "variants": VARIANTS,
            "is_live": True,
            "urls": {
                "1080p": "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8",
                "720p": "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8",
            },
        },
    ],
}


def emit(commands: list[dict[str, Any]]) -> None:
    if not commands:
        return
    sys.stdout.write(
        json.dumps({"type": "commands", "commands": commands}, ensure_ascii=False) + "\n"
    )
    sys.stdout.flush()


def mask_token(token: str) -> str:
    if not token:
        return "未登录"
    if len(token) <= 6:
        return token[:2] + "***"
    return f"{token[:4]}…{token[-2:]}"


def to_source_entry(raw: dict[str, Any]) -> dict[str, Any]:
    entry: dict[str, Any] = {
        "id": raw["id"],
        "name": raw["name"],
        "kind": raw["kind"],
        "variants": raw.get("variants", []),
    }
    if "size" in raw:
        entry["size"] = raw["size"]
    if "duration" in raw:
        entry["duration"] = raw["duration"]
    if "subtitle" in raw:
        entry["subtitle"] = raw["subtitle"]
    return entry


def find_catalog_item(entry_id: str) -> dict[str, Any] | None:
    for items in MEDIA_CATALOG.values():
        for item in items:
            if item["id"] == entry_id:
                return item
    return None


class CloudDemoPlugin:
    def __init__(self) -> None:
        self.token: str = ""
        self.cdn: str = "auto"
        self.last_browse: dict[str, Any] | None = None

    @property
    def authenticated(self) -> bool:
        return bool(self.token)

    def build_login_modal(self) -> dict[str, Any]:
        return {
            "command": "open_modal",
            "modal": {
                "id": "cloud-login",
                "title": "云盘与视频源账号连接",
                "subtitle": "使用客户端扫码授权，或输入任意测试 API Token 体验完整云盘流解析",
                "submit_label": "立即连接",
                "cancel_label": "稍后再说",
                "widgets": [
                    {
                        "kind": "qr_code",
                        "label": "手机端扫码快捷授权",
                        "data": "https://iris-player.example/qr-login?source=demo-drive",
                        "matrix": QR_MATRIX_15X15,
                        "hint": "演示环境无需真实扫码，可直接点击“立即连接”使用默认演示凭证",
                    },
                    {
                        "kind": "label",
                        "text": "凭证将通过宿主安全密钥库加密存储，下次启动播放器自动恢复会话。",
                        "muted": True,
                        "mono": False,
                    },
                    {
                        "kind": "input",
                        "id": "api_token",
                        "label": "API 访问令牌 (Token)",
                        "value": self.token,
                        "placeholder": "输入任意演示 Token（留空默认 demo-vip-token）",
                        "secret": True,
                    },
                    {
                        "kind": "select",
                        "id": "cdn",
                        "label": "传输线路节点 (CDN)",
                        "selected": self.cdn,
                        "options": CDN_OPTIONS,
                    },
                ],
            },
        }

    def build_ui_slots(self) -> list[dict[str, Any]]:
        if self.authenticated:
            status_cmd = {
                "command": "set_status",
                "badge": {
                    "id": "cloud-auth-badge",
                    "text": f"云盘已连 · {self.cdn.upper()}",
                    "tooltip": f"当前凭证 {mask_token(self.token)} · 点击重新配置账号或线路",
                    "accent": True,
                    "action_id": "login",
                },
            }
            account_widgets: list[dict[str, Any]] = [
                {"kind": "stat", "label": "连接状态", "value": "已认证在线", "accent": True},
                {
                    "kind": "stat",
                    "label": "当前凭证",
                    "value": mask_token(self.token),
                    "accent": False,
                },
                {
                    "kind": "select",
                    "id": "sidebar-cdn",
                    "label": "网关优选节点",
                    "selected": self.cdn,
                    "options": CDN_OPTIONS,
                    "action_id": "change_cdn",
                },
                {
                    "kind": "button_group",
                    "buttons": [
                        {"label": "切换账号", "action_id": "login", "accent": False},
                        {"label": "退出登录", "action_id": "logout", "accent": False},
                    ],
                },
            ]
        else:
            status_cmd = {
                "command": "set_status",
                "badge": {
                    "id": "cloud-auth-badge",
                    "text": "云盘未登录",
                    "tooltip": "点击打开云盘扫码 / Token 登录弹窗",
                    "accent": False,
                    "action_id": "login",
                },
            }
            account_widgets = [
                {"kind": "label", "text": AUTH_HINT, "muted": True, "mono": False},
                {
                    "kind": "button",
                    "label": "扫码 / Token 登录",
                    "action_id": "login",
                    "accent": True,
                },
            ]

        quick_widgets: list[dict[str, Any]] = [
            {
                "kind": "list_item",
                "id": "quick-bbb",
                "title": "Big Buck Bunny",
                "subtitle": "直连播放在线 1080P 演示短片",
                "badge": "1080P",
                "active": False,
                "action_id": "quick_play:bbb",
            },
            {
                "kind": "list_item",
                "id": "quick-live",
                "title": "Apple HLS 实时测试流",
                "subtitle": "自适应码率直播网关测试",
                "badge": "LIVE",
                "active": False,
                "action_id": "quick_play:live-bipbop",
            },
        ]

        sidebar_cmd = {
            "command": "set_sidebar",
            "panel": {
                "title": "云盘与视频源演示",
                "sections": [
                    {"heading": "账号与线路网关", "widgets": account_widgets},
                    {"heading": "在线样片直通车", "widgets": quick_widgets},
                ],
            },
        }
        return [status_cmd, sidebar_cmd]

    def build_browse_response(
        self,
        request_id: str,
        source_id: str,
        folder_id: str | None,
        query: str | None,
    ) -> dict[str, Any]:
        if not self.authenticated:
            return {
                "command": "provide_source_page",
                "request_id": request_id,
                "page": {
                    "source_id": source_id,
                    "folder_id": folder_id,
                    "parent_id": None,
                    "title": "云盘与视频源演示",
                    "entries": [],
                    "next_page": None,
                    "authenticated": False,
                    "auth_hint": AUTH_HINT,
                },
            }

        q = (query or "").strip().lower()
        if q and query is not None:
            matched: list[dict[str, Any]] = []
            for folder in ROOT_FOLDERS:
                haystack = f"{folder['name']} {folder.get('subtitle', '')}".lower()
                if q in haystack:
                    matched.append(to_source_entry(folder))
            for items in MEDIA_CATALOG.values():
                for item in items:
                    haystack = f"{item['id']} {item['name']} {item.get('subtitle', '')}".lower()
                    if q in haystack:
                        matched.append(to_source_entry(item))
            return {
                "command": "provide_source_page",
                "request_id": request_id,
                "page": {
                    "source_id": source_id,
                    "folder_id": folder_id,
                    "parent_id": "root" if folder_id not in (None, "", "root", "/") else None,
                    "title": f"搜索：{query.strip()}",
                    "entries": matched,
                    "next_page": None,
                    "authenticated": True,
                    "auth_hint": None,
                },
            }

        norm_folder = (folder_id or "").strip()
        if norm_folder in ("", "root", "/"):
            return {
                "command": "provide_source_page",
                "request_id": request_id,
                "page": {
                    "source_id": source_id,
                    "folder_id": None,
                    "parent_id": None,
                    "title": "云盘与视频源演示",
                    "entries": [to_source_entry(f) for f in ROOT_FOLDERS],
                    "next_page": None,
                    "authenticated": True,
                    "auth_hint": None,
                },
            }

        folder_meta = next((f for f in ROOT_FOLDERS if f["id"] == norm_folder), None)
        entries = [to_source_entry(x) for x in MEDIA_CATALOG.get(norm_folder, [])]
        return {
            "command": "provide_source_page",
            "request_id": request_id,
            "page": {
                "source_id": source_id,
                "folder_id": norm_folder,
                "parent_id": "root",
                "title": folder_meta["name"] if folder_meta else norm_folder,
                "entries": entries,
                "next_page": None,
                "authenticated": True,
                "auth_hint": None,
            },
        }

    def build_stream_spec(
        self,
        source_id: str,
        entry_id: str,
        variant: str | None,
    ) -> dict[str, Any] | None:
        item = find_catalog_item(entry_id)
        if item is None:
            return None

        variants = item.get("variants", [])
        urls: dict[str, str] = item["urls"]
        if variants:
            valid_ids = {v["id"] for v in variants}
            chosen = variant if variant in valid_ids else variants[0]["id"]
            url = urls.get(chosen) or next(iter(urls.values()))
            active_variant: str | None = chosen
        else:
            url = urls.get("default") or next(iter(urls.values()))
            active_variant = None

        return {
            "source_id": source_id,
            "entry_id": entry_id,
            "url": url,
            "headers": {
                "User-Agent": "IrisCloudDemo/1.0",
                "X-Cloud-CDN": self.cdn,
            },
            "title": item["name"],
            "kind": item["kind"],
            "is_live": bool(item.get("is_live", False)),
            "variants": variants,
            "active_variant": active_variant,
            "subtitles": [],
        }

    def refresh_last_browse(self) -> list[dict[str, Any]]:
        if not self.last_browse:
            return []
        return [
            self.build_browse_response(
                request_id=self.last_browse["request_id"],
                source_id=self.last_browse["source_id"],
                folder_id=self.last_browse.get("folder_id"),
                query=self.last_browse.get("query"),
            )
        ]

    def handle_action(self, act: str, value: str | None = None) -> list[dict[str, Any]]:
        if act == "login":
            return [self.build_login_modal()]

        if act == "logout":
            self.token = ""
            cmds: list[dict[str, Any]] = [
                {"command": "set_secret", "key": "api_token", "value": None},
                {"command": "set_state", "key": "cdn", "value": None},
            ]
            cmds.extend(self.build_ui_slots())
            cmds.extend(self.refresh_last_browse())
            cmds.append({"command": "show_toast", "message": "已退出云盘演示账号"})
            return cmds

        if act in ("change_cdn", "sidebar-cdn"):
            val = (value or "").strip()
            if val in ("auto", "cn", "global"):
                self.cdn = val
                cmds = [{"command": "set_state", "key": "cdn", "value": self.cdn}]
                cmds.extend(self.build_ui_slots())
                cmds.append(
                    {"command": "show_toast", "message": f"云盘传输节点已切换至 {self.cdn.upper()}"}
                )
                return cmds
            return []

        if act.startswith("quick_play:"):
            if not self.authenticated:
                return [
                    self.build_login_modal(),
                    {"command": "show_toast", "message": AUTH_HINT},
                ]
            entry_id = act.split(":", 1)[1]
            stream = self.build_stream_spec(SOURCE_ID, entry_id, "1080p")
            if stream:
                return [{"command": "resolve_stream", "request_id": None, "stream": stream}]

        return []

    def handle_event(self, ev: dict[str, Any]) -> list[dict[str, Any]]:
        kind = ev.get("event")

        if kind == "init":
            state = ev.get("state") or {}
            secrets = ev.get("secrets") or {}
            saved_cdn = (state.get("cdn") or secrets.get("cdn") or "auto").strip()
            if saved_cdn in ("auto", "cn", "global"):
                self.cdn = saved_cdn
            saved_token = (secrets.get("api_token") or "").strip()
            if saved_token:
                self.token = saved_token
            return self.build_ui_slots()

        if kind == "command":
            return self.handle_action(str(ev.get("id") or ""), None)

        if kind == "action":
            return self.handle_action(str(ev.get("id") or ""), ev.get("value"))

        if kind == "modal_submit":
            if ev.get("id") == "cloud-login":
                values = ev.get("values") or {}
                raw_token = (values.get("api_token") or "").strip()
                if raw_token == "invalid":
                    return [{"command": "show_toast", "message": "Token 格式无效，请重新输入"}]
                self.token = raw_token or "demo-vip-token"
                chosen_cdn = (values.get("cdn") or self.cdn).strip()
                if chosen_cdn in ("auto", "cn", "global"):
                    self.cdn = chosen_cdn
                cmds: list[dict[str, Any]] = [
                    {"command": "set_secret", "key": "api_token", "value": self.token},
                    {"command": "set_state", "key": "cdn", "value": self.cdn},
                    {"command": "close_modal"},
                ]
                cmds.extend(self.build_ui_slots())
                cmds.extend(self.refresh_last_browse())
                cmds.append({"command": "show_toast", "message": "云盘账号已连接"})
                return cmds
            return []

        if kind == "modal_cancel":
            return []

        if kind == "source_browse":
            req_id = str(ev.get("request_id") or "")
            src_id = str(ev.get("source_id") or SOURCE_ID)
            folder_id = ev.get("folder_id")
            query = ev.get("query")
            self.last_browse = {
                "request_id": req_id,
                "source_id": src_id,
                "folder_id": folder_id,
                "query": query,
            }
            return [self.build_browse_response(req_id, src_id, folder_id, query)]

        if kind == "source_resolve":
            req_id = str(ev.get("request_id") or "")
            src_id = str(ev.get("source_id") or SOURCE_ID)
            entry_id = str(ev.get("entry_id") or "")
            variant = ev.get("variant")
            if not self.authenticated:
                return [
                    {
                        "command": "source_error",
                        "request_id": req_id,
                        "message": AUTH_HINT,
                    },
                    self.build_login_modal(),
                ]
            stream = self.build_stream_spec(src_id, entry_id, variant)
            if stream is None:
                return [
                    {
                        "command": "source_error",
                        "request_id": req_id,
                        "message": f"未找到云盘媒体条目：{entry_id}",
                    }
                ]
            return [{"command": "resolve_stream", "request_id": req_id, "stream": stream}]

        return []


def main() -> None:
    plugin = CloudDemoPlugin()
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
