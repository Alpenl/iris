# Iris 插件系统与开发指南

Iris 的插件系统严格遵循**“控制面 + 矢量叠加面”**边界：

- **绝不经手实时像素数据**：解码、色彩管理与渲染管线由内核独占，插件不会拖慢帧率或破坏色彩准确性。
- **宿主掌控视觉语法**：插件不能绘制任意自定义 UI，而是向宿主规定的 4 个结构化容器填入数据，由 Iris 按方向 C 样式统一渲染。
- **最小权限与进程隔离**：外部插件以独立子进程运行，经 JSON-lines stdio 与宿主通信；所有动作都是强类型语义命令，受清单权限严格过滤，无法注入任何 mpv 原始选项或命令行。

协议定义位于独立的无头 crate [`crates/iris-api`](../crates/iris-api/src/lib.rs)，宿主运行时与内置插件位于 [`crates/iris-core/src/plugin.rs`](../crates/iris-core/src/plugin.rs)。

---

## 1. 内置官方插件

Iris 内置了两个基于同一套 `iris-api` 协议实现的官方插件，可在 **设置 → 插件** 中随时启停：

1. **构图辅助线（`iris.guides`）**
   - 权限：`read_state`、`ui_overlay`、`ui_status`、`ui_sidebar`、`notifications`
   - 能力：在视口矢量叠层绘制三分线（3×3）、黄金分割线、中心十字、2.39:1 电影画幅参考框与 90%/80% 安全区；通过命令面板（`⌘P` / `Ctrl+P`）、侧栏插件视图或底栏状态徽章切换。
2. **A-B 循环与标记（`iris.ab-loop`）**
   - 权限：`read_state`、`control_playback`、`ui_sidebar`、`ui_status`、`notifications`
   - 能力：设置视频 A/B 点实现区间自动循环，并在当前文件添加多个时间点书签，点击侧栏标记直接精确跳转。

---

## 2. 外部插件安装与目录

将插件文件夹放入应用数据目录的 `plugins/` 下（每个子目录包含一个 `plugin.json`），然后在 **设置 → 插件** 点击「重新扫描」即可生效：

- **macOS**：`~/Library/Application Support/Iris/plugins/<插件目录>/plugin.json`
- **Windows**：`%APPDATA%\Iris\plugins\<插件目录>\plugin.json`
- **测试覆盖**：设置环境变量 `IRIS_PLUGINS=/自定义路径` 可隔离扫描目录。

仓库自带一个开箱即用的外部插件示例：[examples/plugins/watch-stats/](../examples/plugins/watch-stats/)（Python 3，记录本次会话累计观看时长并在底栏与侧栏展示）。

---

## 3. 清单规范（`plugin.json`）

```json
{
  "id": "example.watch-stats",
  "name": "观影计时统计",
  "version": "0.1.0",
  "author": "Iris Examples",
  "description": "记录本次会话实际播放时长。",
  "protocol": 1,
  "entry": "main.py",
  "args": [],
  "permissions": [
    "read_state",
    "ui_sidebar",
    "ui_status",
    "notifications"
  ],
  "commands": [
    {
      "id": "reset",
      "title": "统计：重置本次观影计时",
      "subtitle": "将当前会话累计播放时长归零",
      "shortcut_hint": null
    }
  ]
}
```

### 字段校验与安全约束

- `id`：1–64 字符，仅允许 ASCII 字母、数字、`.`、`-`、`_`。
- `protocol`：当前必须为 `1`。
- `entry`：相对于插件目录的可执行文件或 `.py` 脚本；**禁止**绝对路径和 `..` 路径穿越（以 `.py` 结尾时宿主自动使用 `python3 -u` / `python -u` 启动）。
- Windows 上子进程自动附带 `CREATE_NO_WINDOW`，不闪黑框；标准输出单行限制 **64 KiB**，单次轮询最多读取 256 条消息。

---

## 4. 权限模型（`Permission`）

插件发出的每条 `HostCommand` 都会被宿主按清单 `permissions` 校验，未声明的命令直接拦截并记录警告日志：

| 权限名 | 说明 | 允许的命令 / 事件 |
|---|---|---|
| `read_state` | 读取播放快照与时间推进 | 接收 `file_opened`、`state_changed`、`position_tick` |
| `control_playback` | 控制播放与收藏 | `toggle_playback`、`seek_relative`、`seek_absolute`、`set_speed`、`set_volume`、`toggle_mute`、`open_next`、`open_previous`、`toggle_favorite` |
| `ui_sidebar` | 侧栏插件面板槽位 | `set_sidebar` |
| `ui_status` | 底栏紧凑状态徽章槽位 | `set_status` |
| `ui_overlay` | 视口归一化矢量叠层槽位 | `set_overlay`、`clear_overlay` |
| `notifications` | 顶部轻提示 | `show_toast` |

---

## 5. 通信协议（JSON-lines stdio v1）

所有消息均为 **UTF-8 单行 JSON**，以 `\n` 分隔（`stdin` 收宿主事件，`stdout` 发插件命令，务必每次写入后 `flush`）。

### 宿主 → 插件（`HostMessage`）

```json
{"type":"event","payload":{"event":"init","protocol":1,"host_version":"0.1.0"}}
{"type":"event","payload":{"event":"file_opened","snapshot":{"path":"/path/a.mp4","file_name":"a.mp4","kind":"video","position":0.0,"duration":120.0,"paused":false,"volume":100.0,"muted":false,"speed":1.0,"favorite":false,"chapter":-1}}}
{"type":"event","payload":{"event":"position_tick","position":12.4,"duration":120.0,"paused":false}}
{"type":"event","payload":{"event":"command_invoked","command_id":"reset","snapshot":{...}}}
{"type":"event","payload":{"event":"action_invoked","action_id":"reset","snapshot":{...}}}
{"type":"event","payload":{"event":"shutdown"}}
```

### 插件 → 宿主（`PluginMessage`）

```json
{"type":"commands","commands":[{"command":"show_toast","message":"已执行"}]}
{"type":"log","message":"diagnostic info"}
```

---

## 6. 四类宿主界面槽位

### 6.1 视口矢量叠层（`set_overlay` / `clear_overlay`）

坐标全部为相对于视口宽高的归一化比例 `0.0..=1.0`，配色仅限宿主视觉规范 `white` / `accent` / `dim` / `dark`，绘制在画面上方且不拦截鼠标和滚轮事件：

```json
{
  "command": "set_overlay",
  "shapes": [
    {"shape": "grid", "cols": 3, "rows": 3, "color": "white", "opacity": 0.45},
    {"shape": "rect", "x": 0.05, "y": 0.05, "w": 0.9, "h": 0.9, "color": "accent", "opacity": 0.7, "stroke": 1.0},
    {"shape": "line", "x1": 0.5, "y1": 0.0, "x2": 0.5, "y2": 1.0, "color": "dim", "opacity": 0.5, "thickness": 1.0},
    {"shape": "crosshair", "x": 0.5, "y": 0.5, "size": 0.08, "color": "accent", "opacity": 0.85},
    {"shape": "label", "x": 0.02, "y": 0.02, "text": "2.39:1", "color": "accent"}
  ]
}
```

### 6.2 底栏状态徽章（`set_status`）

传 `item: null` 清除徽章；带 `action_id` 时可点击并回调 `action_invoked`：

```json
{
  "command": "set_status",
  "item": {
    "id": "watch-timer",
    "text": "已看 04:12",
    "tooltip": "点击重置",
    "accent": false,
    "action_id": "reset"
  }
}
```

### 6.3 侧栏插件面板（`set_sidebar`）

在目录侧栏右上角点击插件图标（方块网格）可切换至插件面板视图，支持 `text`、`key_value`、`progress`、`button`、`button_row` 五种结构化条目：

```json
{
  "command": "set_sidebar",
  "panel": {
    "title": "示例面板",
    "sections": [
      {
        "heading": "概览",
        "items": [
          {"kind": "key_value", "key": "状态", "value": "运行中", "accent": true},
          {"kind": "progress", "label": "进度", "ratio": 0.65},
          {"kind": "button", "label": "执行动作", "action_id": "run", "accent": false},
          {"kind": "button_row", "buttons": [
            {"label": "A", "action_id": "a", "accent": true},
            {"label": "B", "action_id": "b", "accent": false}
          ]}
        ]
      }
    ]
  }
}
```

### 6.4 命令面板（`commands`）

清单中声明的每条 `commands` 会自动注册到 **命令面板（`⌘P` / `Ctrl+P` / `⌘⇧P`）**，支持按标题、说明或插件名实时过滤；用户选中后宿主发送 `command_invoked`。
