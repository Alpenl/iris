# Iris

Iris 是一个以 [mpv](https://mpv.io) 为播放核心的高性能跨平台照片 / 视频 / 音频播放器，面向 macOS 和 Windows。界面用 Rust 基于 [GPUI](https://www.gpui.rs) 编写；解码与色彩管线由原生视频渲染层独占，本体默认保持硬朗克制的方向 C 模块面板风格，同时通过正交插件系统（`iris-api`）提供**全插件化远程存储（标配免挂载 `iris.webdav`）**、**带个人基线保护的工作区预设**、**多方案可录制快捷键**以及**高自由度界面与视口叠层定制**。

> **当前版本为 0.0.1。** 项目只构建和打包与工作区版本一致的 `0.0.x`。启用 `0.1.x` 需要项目维护者手动授权并修改版本门禁。正式签名与公证需要发行凭据。

文档：[架构](docs/ARCHITECTURE.md) · [插件开发指南](docs/PLUGINS.md) · [Plugin V2 架构](docs/PLUGIN_V2_ARCHITECTURE.md) · [Plugin V2 决议](docs/PLUGIN_V2_DECISIONS.md) · [决策与路线图](docs/DECISIONS.md) · [决议讨论录](docs/DISCUSSIONS.md) · [测试与实测记录](docs/TESTING.md)

## 核心特性

- **视频、音频与远程流播放**：
  - libmpv 原生硬解（macOS VideoToolbox，Windows D3D11VA），支持软硬解切换、精确/大步跳转、逐帧步进（`,` / `.`）、播完策略、本地与远程统一 `MediaUri` 续播记忆。
  - 可拖动时间轴、网络流缓冲区间、直播模式、多清晰度和线路切换、MKV/MP4 章节、连续音量条与 `0.25×–4.00×` 倍速。
  - 画面比例与铺满裁剪（`C`）、临时旋转（`R`）与镜像（`H`）、**画面均衡器（亮度 / 对比度 / 饱和度 / 伽马微调）**、**音频输出设备实时枚举与切换**、截图保存（`⌘S` / `Ctrl+S`）、播放期防休眠，以及 macOS 控制中心和 Windows 系统媒体控件。
- **广色域照片浏览（本地 & 远程/网盘同享）**：
  - 滚轮 / macOS 触控板双指捏合围绕光标缩放、拖拽平移、`0` 适应窗口、`Space` 或双击切换 1:1、底部内存缩略图条（`G`）。
  - 根据容器元数据识别内嵌 ICC、照片尺寸、HEIC 旋转与镜像、EXIF 拍摄参数。远程照片先读 256 KiB，再通过 HTTP Range 按需补读后置元数据。每张最多 16 次请求、总计 5 MiB；服务器忽略 Range 时使用已读前缀。
- **四合一统一侧栏与全插件化远程存储**：
  - 侧栏支持 **「目录 / 数据源 / 队列 / 插件」** 四视图，可拖拽调宽、左右换边。进入全屏时目录自动收起，鼠标移到停靠侧边缘即可展开，移回画面后收起。全屏播放栏贴合画面底边，目录展开时视频与进度条保持原位。
  - **统一目录项（`CatalogItem`）与独立播放队列（`PlayQueue`）**：跨文件夹/跨远程源自由排队，切集绝不冲掉自定义队列；支持常驻位置书签与慢速 SMB/UNC 网络共享盘免 `stat` 风暴快速扫描。
  - **远程源与凭据**：HTTP(S) 流由宿主的会话转发器打开，凭据只发给授权源。内置 `iris.webdav` 支持多台 NAS、系统凭据库和外挂字幕，`⌘U` / `Ctrl+U` 可打开网络直链。
- **工作区与场景预设（`1B+` 稀疏场景宏）**：
  - 标题栏一键切换或通过 `Alt+W` / `⌘P` 秒切预设；内置 **「默认工作区」「沉浸观影」「审片与选片」「精听与学习」「小窗置顶」** 五套预设，并支持保存自定义预设（可选包含布局、播放策略与插件差量组合）。
  - `settings.conf` 始终保护用户的纯净个人日常基线，切换预设自动通过内存 `PresetRevert` 回正上个预设改动的参数并触发退出指令，手动微调实时感知且绝不误启无关禁用插件。
- **多方案可录制快捷键系统（Keymap）**：
  - 内置 **Iris 默认 / mpv 经典 / PotPlayer 风格** 三套零冲突键位方案一键热切换，内置 `!TextInput` 打字安全防火墙（单键快捷键绝不干扰搜索框与表单输入），全局按钮悬停提示随当前键位方案实时联动。
- **高自由度主题与正交插件系统（`iris-api`，`protocol: 2`）**：
  - GPUI 全局 `ThemeTokens` 引擎（内置 **石墨模块 / OLED 纯黑暗房 / 胶片暖灰审片 / 冰川高对比** 四套色板）。
  - 插件可在受控权限内动态扩展多实例数据源（`SetSources`）、定制配色/圆角/栏高与无标题栏影院窗（`hide_titlebar`，宿主内置顶缘悬浮唤醒、视口拖拽与「恢复默认外观」三重安全护栏）、挂载 13 类同构声明式 `Widget` 侧栏/弹窗以及 7 类视口 `OverlayNode` 交互式矢量 HUD。详见 [插件开发指南](docs/PLUGINS.md)。

## 默认快捷键（Iris 标准方案，可在设置中切换 mpv / PotPlayer 方案或自由录制）

| 按键 | 作用 |
|---|---|
| `⌘P` / `Ctrl+P`（或 `⌘⇧P`） | 打开命令面板（动作、预设、主题、数据源与插件命令） |
| `⌘O` / `Ctrl+O` | 打开本地文件或文件夹 |
| `⌘U` / `Ctrl+U` | 打开网络流 / 远程媒体地址 |
| `Alt+W` | 循环切换工作区预设 |
| `⌘F` / `Ctrl+F` | 搜索当前目录（插件源支持回车触发远程搜索） |
| `⌘R` / `Ctrl+R` | 刷新当前目录 / 远程源 |
| `⌘,` / `Ctrl+,` | 打开设置中心（外观与常规 / 工作区预设 / 播放 / 字幕与音频 / 看图 / 插件 / 快捷键） |
| `Tab` | 展开 / 收起侧栏（全屏时弹出 / 关闭） |
| `F` / 双击画面 | 切换全屏 |
| `T` | 窗口置顶开关 |
| `I` / `⌘I` | 媒体详情、解码统计与 EXIF 面板 |
| `S` | 收藏 / 取消收藏当前本地或远程媒体 |
| `Space` | 视频/音频：播放/暂停；照片：切换 1:1 原始尺寸 |
| `←` / `→`（`Shift+←/→`） | 视频精确跳转（6 倍大步跳转）；照片上一张 / 下一张 |
| `,` / `.` | 视频逐帧后退 / 逐帧前进 |
| `↑` / `↓` / `M` | 调节音量 / 静音 |
| `[` / `]` / `Backspace` | 倍速调节 / 恢复 `1.00×` |
| `V` / `J` / `Z` / `X` | 字幕显隐 / 切轨 / 时间偏移微调 |
| `Shift+,` / `Shift+.` | 跳到上一句 / 下一句字幕，保持原有暂停状态 |
| `A` / `C` / `R` / `H` | 切换音轨 / 画面比例 / 旋转 90° / 镜像翻转 |
| `G` | 照片底部内存缩略图条开关 |
| `⌘S` / `Ctrl+S` | 保存当前帧截图 |
| `⌘⌥⇧R` / `Ctrl+Alt+Shift+R` | 恢复默认外观（固定快捷键） |
| `Esc` | 依次关闭键位录制 / 弹窗 / 菜单 / 命令面板 / 设置 / 浮层 / 搜索，最后退出全屏 |

## 构建与验证

需要 Rust `1.100.0`（2024 edition）与 libmpv。macOS 可用 `brew install mpv` 开发。Windows x64 MSVC 可设置 `MPV_LIB_DIR`，或将开发用 `mpv.lib` 放到 `target/deps/mpv/`，并将运行库所在目录加入 `PATH`。

```bash
cargo build --release                      # 本地开发构建（仅开发用，不得用 Homebrew libmpv 分发）
./target/release/iris test-media           # 打开测试素材
python3 scripts/check-project.py           # 架构边界、模块行数门禁、依赖白名单、图标与插件清单检查
cargo test --locked --workspace            # 全量单元与端到端集成测试
cargo clippy --locked --workspace --all-targets -- -D warnings
```

`iris --help` 显示用法，`iris --version` 显示版本。用 `iris -- -clip.mp4` 打开以 `-` 开头的路径。

## 个人数据与配置存储

所有个人数据均保存在系统应用数据目录，内核对本地与远程媒体永远只读：

- **macOS**：`~/Library/Application Support/Iris/`（凭据存于 macOS Keychain，日志：`~/Library/Logs/Iris/iris.log`）
- **Windows**：`%APPDATA%\Iris\`（凭据存于 Windows Credential Manager，日志：`%LOCALAPPDATA%\Iris\iris.log`）

目录下包含：
- `settings.conf`：用户的个人日常基线偏好与当前激活预设 ID
- `presets.json`：用户保存的自定义工作区预设
- `keymap.json`：快捷键方案与自定义按键映射
- `locations.json`：常驻位置书签
- `records.tsv` / `recent.tsv`：保存收藏、续播进度与最近打开记录。包含用户信息、查询或片段的 HTTP(S) 直链仅用于当前会话；具有稳定条目 ID 的插件媒体仍可收藏与续播。
- `plugin-v2-state/`：按安装身份保存非敏感配置；敏感凭据保存在系统凭据库。旧 `plugin_state/` 为官方迁移输入，成功前保留旧资料。
- `credential_index.json` 仅索引凭据的作用域和键名。密钥正文保存在系统凭据库。
- `plugins-v2/`：审核后的 Wasm 包与安装授权（支持 `IRIS_PLUGINS_V2` 开发覆盖）；`plugin-v2-temp/` 为有配额的临时产物。
- `plugin-tool-bindings.json`：用户显式批准的可信原生工具与模型绑定。

## 代码结构

```
crates/
  iris-mpv/            libmpv 手写 Rust FFI（client API + mpv_node 结构化传参 + RenderHandle 窄能力封装）
  iris-api/            无头插件协议（protocol: 2 / ABI: 2，纯 serde 零重依赖）
  iris-plugin-sdk/     Wasm Guest 开发 SDK（纯插件侧依赖，宿主零反向依赖）
  iris-plugin-guides/  官方构图辅助线 Wasm 插件（`iris.guides`）
  iris-plugin-webdav/  官方 WebDAV 远程数据源 Wasm 插件（`iris.webdav`）
  iris-core/           无头内核（零 GPUI 依赖）
    src/player/        视口会话状态机、媒体检查器与播放命令
    src/photo/         魔数优先单遍照片头解析（JPEG/PNG/WebP/HEIF/TIFF）、EXIF 与 ICC 色彩匹配
    src/plugin_v2/     Wasmtime 沙箱运行时、安装授权、网络/流与官方包自举
    src/logging/       单一脱敏轮转文本日志（`iris.log`）与结构化事件流（`events.jsonl`）
    src/keymap/        三方案动作目录、文本输入保护与冲突检测
    src/vfs/           常驻书签与有界 HTTP 控制面客户端（含跨源凭据剥离与 SSRF 拦截）
    src/{catalog,media_uri,vault,preset,store,settings,library,geometry,input}.rs
  iris/                GPUI 桌面应用
    src/theme.rs       GPUI Global ThemeTokens 令牌引擎（方向 C 默认 + 字号缩放 + 对比度兜底）
    src/ui/kit/        共用基础控件库（TextInputHub、按钮/列表行/表单/卡片/模态框壳）
    src/ui/            瘦 IrisWindow 领域控制器 + 5 层 Z-Order 渲染栈 + sidebar / controls / popover / plugin_ui / settings / dialogs
    src/video/         macOS CAOpenGLLayer / Windows D3D11 DirectComposition 原生视频合成层
    src/platform/      按 OS 隔离的窗口控制、防休眠、触控板捏合、媒体键、原生缩略图解码与截图
examples/plugins/
  cinema-ui/           声明式无边框影院皮肤插件示例
  watch-stats/         观影计时 Rust Wasm 插件示例
  virtual-audio/       虚拟音频流 Rust Wasm 插件示例
  bilibili/            Bilibili 视频源、弹幕与清晰度 Rust Wasm 插件示例
  baidupan/            百度网盘 HLS 播放源 Rust Wasm 插件示例
```

## 许可

主程序按 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 双重许可，由使用者任选。分发时动态链接源码自建的 LGPL 媒体库；对应源码、配置和第三方许可随产物分发。详见 [决策记录](docs/DECISIONS.md#5-许可路线-b)。
