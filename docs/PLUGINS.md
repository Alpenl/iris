# Iris 插件系统与开发指南（`iris-api` 协议 `protocol: 1`）

Iris 的插件架构从第一性原理出发，围绕四根支柱构建单一、正交的控制面协议（`PROTOCOL_VERSION = 1`）：

1. **远程数据源（`media_source`）**：插件通过 `MediaSource` 提供目录、搜索、分页、多服务器、授权、播放线路和外挂字幕。稳定的 `source_id` 与 `entry_id` 维持收藏和续播关联。HTTP(S) 播放经过会话专属的本地转发器，凭据只发给授权源。插件也可提供高端口回环流。远程照片从 256 KiB 前缀起步，在请求和字节预算内读取后置元数据。
2. **高自由度参数化工作台与统一声明式控件树（`UiTheme` / `UiToolbar` / `UiSidebar` / `UiModal` / `UiStatus` / `UiOverlay`）**：本体默认保持方向 C 视觉一致，但不限制插件自由度——UI 插件可动态接管全套主题配色令牌（`#RRGGBB[AA]`）、圆角与栏高尺度、无边框影院窗口（`hide_titlebar`，宿主自动提供顶缘叠层唤醒标题栏、视口拖拽与最低对比度逃生护栏）、悬浮控制栏（`floating_dock`）与底栏常驻按钮显隐；侧栏面板（`SidebarPanel`）与非破坏性异步校验弹窗（`PluginModal`）共用同一套 13 种声明式 `Widget` 控件；视口层通过归一化 `OverlayNode` 绘制边框/填充卡片、辅助线、时间码/字幕文本、频谱柱状条与可点击 HUD 按钮。
3. **原子握手、状态持久化与分桶加密凭据库（`SetState` / `SetSecret`）**：非敏感偏好键值（`state`）由宿主自动落盘，敏感密码/令牌（`secrets`，单条上限 2048B）按 `builtin.<id>` / `ext.<id>` 物理隔离存入系统凭据库（macOS Keychain / Windows Credential Manager，失败显式报错，绝不降级明文文件），并在插件启动时通过单次原子 `Init` 事件连同当前播放快照一并注入。
4. **像素与安全严格隔离**：解码、色彩空间转换与原生交换链由内核独占，插件绝不经手实时像素数据，且通过 `DirtySlots` 值相等脏检测守住全屏隐藏控制栏下的 `0 Hz` 空转防线。

协议定义位于无头 crate [`crates/iris-api`](../crates/iris-api/src/lib.rs)，宿主运行时与内置插件位于 [`crates/iris-core/src/plugin/`](../crates/iris-core/src/plugin/mod.rs)，系统凭据库位于 [`crates/iris-core/src/vault.rs`](../crates/iris-core/src/vault.rs)。

---

## 1. 内置官方插件与外部参考插件

### 内置官方插件（编译进内核，`iris.*` 保留命名空间，可在「设置 → 插件」启停）
1. **WebDAV 远程存储（`iris.webdav`）**：支持多台 NAS 管理、Basic/Bearer 认证、有界的后台 `PROPFIND`、同目录外挂字幕和广色域原图。用户名和口令使用独立字段，凭据存入系统凭据库。向非回环 `http://` 地址发送口令需要用户确认。
2. **构图辅助线（`iris.guides`）**：三分线（3×3）、黄金分割线、中心十字、2.39:1 电影画幅框与 90%/80% 安全区；可被「审片与选片」工作区预设联动开启并在退出预设时自动关闭。
3. **A-B 循环与标记（`iris.ab-loop`）**：视频/音频 A-B 区间循环与时间点书签（同时支持本地与远程插件流 `MediaUri`）。

### 外部进程参考插件（位于 `examples/plugins/`）
1. **云盘与视频源参考插件（[`examples/plugins/cloud-media-demo/`](../examples/plugins/cloud-media-demo/)）**
   - 完整演示网盘 / 视频源插件形态：扫码二维码 + Token 登录弹窗（`ui_modal`）、加密凭据存取（`set_secret` + `init.secrets`）、目录树浏览、关键词搜索、清晰度切换（1080P / 720P）以及带自定义请求头的流解析（`resolve_stream`）。
2. **Cinema UI 自由定制参考插件（[`examples/plugins/cinema-ui/`](../examples/plugins/cinema-ui/)）**
   - 完整演示高自由度 UI 插件形态：注册底栏快捷按钮（`ui_toolbar`）、实时热切换多套皮肤、无标题栏无边框窗口（`hide_titlebar`）与悬浮控制栏布局（`ui_theme`）、视口可交互 SMPTE 时间码与动态频谱 HUD（`ui_overlay`），以及含滑块、输入框、开关、选择器的交互式侧栏。
3. **观影计时统计（[`examples/plugins/watch-stats/`](../examples/plugins/watch-stats/)）**
   - 轻量级 `read_state` + `ui_sidebar` + `ui_status` + `set_state` 外部进程插件示例。

---

## 2. 外部插件目录与零进程声明式主题插件

将插件目录放入应用数据目录的 `plugins/<插件目录>/plugin.json`，在 **设置 → 插件** 点击「重新扫描」即可加载：

- **macOS**：`~/Library/Application Support/Iris/plugins/<插件目录>/plugin.json`
- **Windows**：`%APPDATA%\Iris\plugins\<插件目录>\plugin.json`
- **测试隔离**：环境变量 `IRIS_PLUGINS=/自定义路径`

> **纯声明式 UI 主题插件（零子进程开销）**：仅需定制配色、圆角、栏高或无边框/悬浮控制栏骨架时，在 `plugin.json` 中声明 `"permissions": ["ui_theme"]` 并直接填写 `"theme": { ... }` 与 `"chrome": { ... }`，无需提供 `"entry"` 可执行脚本，宿主直接以零进程开销挂载生效。

---

## 3. 权限模型（`Permission`）

`SetState` 用于持久化插件自己的非敏感配置，单插件最多 512 KiB。其余命令与事件受 `plugin.json` 权限白名单门控。

| 权限名 | 说明 | 允许接收的事件 / 发送的命令 |
|---|---|---|
| `read_state` | 读取播放快照与播放进度 | 接收 `init.snapshot`、`media_opened`、`state_changed`、`tick`（含 `uri`、`title`、`is_live`、`buffering`） |
| `control_playback` | 控制传输、倍速、音量与收藏 | `toggle_playback`、`seek_relative`、`seek_absolute`、`set_speed`、`set_volume`、`toggle_mute`、`open_next`、`open_previous`、`toggle_favorite` |
| `ui_sidebar` | 交互式侧栏面板 | `set_sidebar`（`SidebarPanel` → `SidebarSection` → 统一 `Widget` 列表） |
| `ui_status` | 底栏状态槽紧凑徽章 | `set_status`（`StatusBadge`） |
| `ui_overlay` | 视口归一化矢量与 HUD 叠层 | `set_overlay`（`nodes: Vec<OverlayNode>`，空数组即清空叠层） |
| `ui_toolbar` | 底栏自定义工具按钮 | `set_toolbar`（`buttons: Vec<ToolbarButton>`） |
| `ui_modal` | 交互式配置、OAuth 与扫码弹窗 | `open_modal`、`close_modal`（`PluginModal`，与侧栏共用统一 `Widget` 列表） |
| `ui_theme` | 动态主题令牌与窗口/底栏结构定制 | 清单静态声明或运行时发送 `set_theme`（`UiThemeSpec` + `ChromeLayoutSpec`） |
| `media_source` | 虚拟数据源与加密凭据（自动隐含 `secrets`） | 清单声明 `sources` 或 `source_provider`；接收 `source_browse`、`source_resolve`、`source_manage`；发送 `set_sources`、`refresh_source`、`provide_source_page`、`resolve_stream`、`source_error`、`set_secret` |
| `secrets` | 独立使用系统加密凭据库 | 接收 `init.secrets`；发送 `set_secret`（无需声明 `sources`） |
| `notifications` | 顶部轻提示 | `show_toast` |

---

## 4. 消息线协议（JSON-lines over stdio）

外部插件进程从 `stdin` 按行读取 `HostMessage`，向 `stdout` 按行输出 `PluginMessage`（单行上限 **2 MiB**，支撑大目录分页返回；外部插件禁止使用保留前缀 `iris.*`）：

### 4.1 宿主 → 插件事件（`HostEvent`）

1. **原子启动握手（`init`）**：一次性注入协议版本、持久化非敏感 `state`、解密后的 `secrets`（需 `secrets` 或 `media_source` 权限）与当前播放快照：
   ```json
   {
     "type": "event",
     "payload": {
       "event": "init",
       "protocol": 1,
       "host_version": "0.0.1",
       "state": { "skin": "neon" },
       "secrets": { "access_token": "tok_xxx" },
       "snapshot": { "uri": null, "title": null, "kind": null, "position": 0.0, "duration": 0.0, "paused": true, "volume": 100.0, "muted": false, "speed": 1.0, "favorite": false, "chapter": -1, "is_live": false, "buffering": false }
     }
   }
   ```
2. **播放生命周期**：`media_opened { snapshot }`、`state_changed { snapshot }`、`tick { position, duration, paused }`。
3. **命令与统一控件交互**：
   - 命令面板或快捷键触发：`{"event": "command", "id": "login", "snapshot": { ... }}`
   - 任意侧栏 / 底栏按钮 / 状态徽章 / 视口 HUD / 滑块 / 输入框 / 开关 / 选择器触发统一 `action` 事件（`input`、`select`、`toggle`、`slider` 通过 `value` 携带新值，纯按钮点击 `value` 为 `null`）：
     ```json
     {"event": "action", "id": "set_opacity", "value": "0.85", "snapshot": { ... }}
     ```
4. **非破坏性弹窗交互**：用户点击确认时宿主保持弹窗开启（保留已输入内容）并发送 `modal_submit { id, values }`，插件校验成功后显式发送 `close_modal` 关闭弹窗（校验失败仅发 `show_toast` 或更新同 `id` 弹窗即可原地提示）；用户按 `Esc` 或取消时宿主强制立即关窗并发送 `modal_cancel { id }`。
5. **虚拟数据源请求**：`source_browse` 和 `source_resolve` 的应答须回传收到的 `request_id`。只接受当前插件当前请求的一次应答；浏览和解析请求 30 秒超时。插件主动打开媒体时省略 `resolve_stream.request_id`。`source_manage` 处理服务器配置与移除。
6. **退出清理**：插件应在 `shutdown` 或 stdin EOF 时关闭自己的子进程和监听端口。Windows 在创建进程时就把插件加入宿主持有的 Job，宿主异常退出时由系统回收。Unix 宿主停止插件时清理插件进程组。自行创建新会话的辅助进程由插件负责回收。

- `refresh_source` 携带 `source_id`，通知宿主为当前显示的该数据源重新发起浏览请求，例如登录完成后刷新列表。插件使用新 `source_browse` 事件中的 `request_id` 回包。

外部插件以当前用户身份运行。消息权限控制它可以请求的 Iris 功能。只安装来源可信的插件。

### 4.2 统一声明式控件 `Widget`（侧栏与弹窗同构复用）

`SidebarSection.widgets` 与 `PluginModal.widgets` 完全共用以下 13 种 `Widget`（`"kind"` 标签）：

- `label`：`{ text, muted, mono }`
- `stat`：`{ label, value, accent }`
- `progress`：`{ label, ratio }`
- `slider`：`{ id, label, value, min, max, step, unit, action_id }`（带 `-` / 数值单位 / `+` 步进器与进度轨）
- `button`：`{ label, action_id, icon, accent }`（`icon` 支持内置 SVG 名称，如 `"play"`、`"globe"`、`"sliders"`、`"layers"` 等）
- `button_group`：`{ buttons: [{ label, action_id, icon, accent }, ...] }`
- `input`：`{ id, label, value, placeholder, secret, action_id }`（支持中英文 IME 与剪贴板；侧栏中回车或点「提交」触发 `action`；弹窗中随 `modal_submit.values` 一并提交）
- `toggle`：`{ id, label, checked, action_id }`
- `select`：`{ id, label, selected, options: [{ value, label }], action_id }`
- `list_item`：`{ id, title, subtitle, badge, active, action_id }`
- `qr_code`：`{ label, data, matrix, hint }`（高清点阵二维码渲染 + 一键复制内容）
- `link`：`{ label, url, button_text }`
- `divider`：分隔线

### 4.3 视口叠层节点 `OverlayNode` 与统一色彩 `Color`

所有颜色字段统一使用 `Color`：语义令牌 `"white"`、`"muted"`、`"dim"`、`"dark"`、`"accent"`，或十六进制 `"#RRGGBB"` / `"#RRGGBBAA"`。`OverlayNode` 支持 7 种归一化（`0.0..=1.0`）节点：

单个插件最多提交 64 个节点，展开后最多 512 个绘制元素；每帧全部插件合计最多 2,048 个。超限更新保留上一次有效叠层。播放快照只对具备 `read_state` 的插件开放，适用于 `init`、`command` 和 `action`。

- `box`：`{ x, y, w, h, fill, border, border_width, radius, opacity }`
- `line`：`{ x1, y1, x2, y2, color, width, opacity }`
- `grid`：`{ cols, rows, color, opacity }`
- `crosshair`：`{ x, y, size, color, opacity }`
- `text`：`{ x, y, text, size, color, bg, mono, align, opacity }`（`align` 支持 `"left"` | `"center"` | `"right"`）
- `bars`：`{ x, y, w, h, values, color, opacity }`
- `button`：`{ id, x, y, label, icon, accent, action_id }`（宿主精确路由回所属插件，点击不抢视口键盘焦点）

---

## 5. 网盘 / 视频源插件与无边框影院 UI 示例

### 5.1 `plugin.json` 与 `resolve_stream`（`StreamSpec`）

```json
{
  "id": "acme.cloud-drive",
  "name": "示例网盘插件",
  "version": "0.0.1",
  "protocol": 1,
  "entry": "main.py",
  "permissions": ["media_source", "ui_modal", "notifications"],
  "commands": [
    { "id": "login", "title": "网盘：扫码或 Token 登录" }
  ],
  "sources": [
    {
      "id": "my-drive",
      "name": "我的云盘",
      "subtitle": "支持原画视频流与广色域照片直看",
      "supports_search": true,
      "requires_auth": true,
      "auth_command_id": "login"
    }
  ]
}
```

解析播放地址（响应 `source_resolve` 时附带 `request_id`，主动推流时 `request_id` 可省略为 `null`）：

```json
{
  "type": "commands",
  "commands": [
    {
      "command": "resolve_stream",
      "request_id": "resolve-1",
      "stream": {
        "source_id": "my-drive",
        "entry_id": "item-42",
        "url": "https://cdn.example.com/media/42.mkv",
        "headers": {
          "Authorization": "Bearer tok_xxx",
          "User-Agent": "MyDriveClient/1.0"
        },
        "title": "01-样片.mkv",
        "kind": "video",
        "is_live": false,
        "variants": [
          { "id": "4k", "label": "4K 原画" },
          { "id": "1080p", "label": "1080P" }
        ],
        "active_variant": "4k",
        "subtitles": []
      }
    }
  ]
}
```

### 5.2 动态主题与无边框影院窗口（`set_theme`）

```json
{
  "type": "commands",
  "commands": [
    {
      "command": "set_theme",
      "theme": {
        "name": "Borderless Cinema",
        "window_bg": "#050608",
        "panel": "#0D1017",
        "accent": "#00E5FF",
        "corner_radius": 6.0,
        "dock_height": 44.0
      },
      "chrome": {
        "hide_titlebar": true,
        "floating_dock": true,
        "show_titlebar_meta": false,
        "compact_controls": false,
        "hidden_dock_buttons": ["loop", "screenshot"]
      }
    }
  ]
}
```
