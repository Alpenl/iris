# Iris

Iris 是一个以 [mpv](https://mpv.io) 为播放核心的高性能本地照片 / 视频 / 音频播放器，面向 macOS 和 Windows。界面用 Rust 写成，基于 Zed 编辑器的 UI 框架 [GPUI](https://www.gpui.rs)；解码和渲染交给 libmpv，并内置控制面与矢量叠加面插件系统。

> **状态：v0.1 持续完善中。** 已完成核心播放与看图交互打磨、设置界面、命令面板及控制面插件系统，并通过 GitHub Actions 双端（macOS / Windows）编译、测试与许可证门禁检查；正式签名／公证尚未配置。

文档：[架构](docs/ARCHITECTURE.md) · [插件开发指南](docs/PLUGINS.md) · [决策与路线图](docs/DECISIONS.md) · [测试与实测记录](docs/TESTING.md)

## 功能

- **视频与音频播放**：
  - libmpv 解码，默认硬解（macOS VideoToolbox，Windows D3D11VA），支持在设置中一键切换软解。
  - 精确跳转（可配置步长 `3–30s`，`Shift+←/→` 6 倍大步跳转）、逐帧步进（`,` / `.`）、播完策略（停在末帧 / 自动播放下一项 / 单曲循环）、进度记忆与续播。
  - 60fps 跟手时间轴拖拽预览、MKV/MP4 章节刻度与章节名悬停提示、点击时间切换剩余时长、连续可拖拽 0–100% 音量条与静音（`M`）、规整倍速档位（`0.25×–3.00×`）。
  - 画面比例与铺满裁剪（`C`：原始 / 16:9 / 4:3 / 21:9 / 铺满）、临时旋转（`R`）与水平/垂直镜像（`H` / `Shift+H`）、原画/含字幕截图保存（`⌘S` / `Ctrl+S`）。
  - 播放视频期间自动阻止系统休眠 / 屏保；支持 `.mp3 / .flac / .wav / .m4a / .ogg / .opus` 音频文件播放卡片；macOS 原生支持键盘媒体键与控制中心 Now Playing。
- **音轨与字幕**：
  - 按钮正上方弹出音轨与字幕选择浮层，展示语言、标题、编码与外挂标记；按 `A` / `J` 切轨弹出轻量 OSD 提示。
  - 自动模糊匹配同目录外挂字幕（`sub-auto=fuzzy`），拖入 `.srt / .ass / .vtt` 等字幕文件直接挂载而不中断当前播放；支持字幕偏移微调（`Z` / `X` / `Shift+Z`）与字号缩放。
- **可调宽虚拟化目录**：
  - 停靠在窗口一侧，边缘可拖拽调整宽度（`220–520px`，双击分隔线重置 `288px`），左右位置可切换并保存；`Tab` 或工具栏按钮收起 / 展开（全屏为弹出）。
  - 基于 GPUI `uniform_list` 虚拟滚动 + 细滚动条 + 自动滚动到当前项 + 视频行底极细进度条。
  - 支持「全部 / 视频 / 照片 / 收藏」过滤、目录内实时搜索（`⌘F` / `Ctrl+F`）、按名称 / 修改时间 / 文件大小升降序切换、子文件夹与返回上级（`..`）导航、右键上下文菜单，以及空窗口「最近打开」列表。
- **照片浏览**：
  - 滚轮 / macOS 触控板双指捏合围绕光标缩放，放大后抓手光标拖拽平移，`0` 适应窗口，`Space` 或双击切换 1:1（可配置），`R` / `H` 临时旋转与镜像（不改动磁盘原文件）。
  - 内嵌 ICC 色彩识别并在标题栏展示（如 `Display P3` / `Adobe RGB`）、HEIC 方向与超大图保护、内存缩略图底条（`G`，macOS 使用 ImageIO 原生解码，不往媒体目录写缓存）。
- **窗口与交互体验**：
  - 窗口最小尺寸降至 `480×300`，支持小窗自适应工具栏与窗口置顶（`T` 及标题栏图钉）；标题栏窗口按钮按平台习惯排布（macOS 左上、Windows 右上）。
  - 全屏下键盘调节音量 / 跳转 / 暂停弹出独立轻量 OSD，不拉起工具栏也不挤压字幕；画面与目录行均提供方向 C 右键上下文菜单；`I` / `⌘I` 打开媒体详情与 EXIF / 解码统计面板。
  - 窗内居中设置面板（`⌘,` / `Ctrl+,`）实时调整外观、播放、字幕音频、看图、插件与数据管理。
- **命令面板与插件系统**：
  - `⌘P` / `Ctrl+P` 打开命令面板，统一检索内核命令与启用插件的命令。
  - 插件仅工作在控制面与归一化矢量叠加面，绝不经手实时像素数据；内置「构图辅助线（三分线 / 黄金分割 / 十字 / 2.39:1 / 安全区）」与「A-B 循环与标记」，并支持外部 JSON-lines stdio 进程插件（详见 [插件开发指南](docs/PLUGINS.md)）。
- **打开方式**：命令行参数、拖入单个文件 / 文件夹（多文件拖入形成临时播放队列）、`⌘O` / `Ctrl+O`、最近打开列表。

## 界面

石墨色模块面板（方向 C）：1px 线条、直角、橙色强调色 `#FF5B1F`；界面字体 Space Grotesk，数字和代码用 IBM Plex Mono。细节参照 Zed 和 Sooty。

```
┌────────────────────────────────────────────────────────────────────────────────────────┐
│ × − □  IRIS │ 文件夹 / 文件名                                         尺寸 · 解码  📌 │
├─────────────────────────┬──────────────────────────────────────────────────────────────┤
│ 全部 视频 照片 收藏 🔍 ⇅ 🧩 ⇄ │                                                        │
├─────────────────────────┤                                                              │
│ ↑ ..                    │                         视频 / 照片                          │
│ ▷ 01-片头.mp4           │                     (支持矢量叠层与轻量 OSD)                  │
│ ▶ 02-正片.mkv       ★   │                                                              │
│ ▢ 03-截图.jpg           │                                                              │
│                         ├━━━━━━━━━━━━━━●───────────────────────────────────────────────┤
│                         │ ◧ │ ⏮ ▶ ⏭ 01:02 / 24:00  [插件状态] 📷 🔁 字幕 音轨 1.00× … │ 🔇▮▮▮▮▯ │ ⌘P ℹ ☆ ⚙ ⛶ │
└─────────────────────────┴──────────────────────────────────────────────────────────────┘
```

## 快捷键

### 通用

| 按键 | 作用 |
|---|---|
| `⌘P` / `Ctrl+P`（或 `⌘⇧P`） | 打开命令面板（内核与插件命令） |
| `⌘O` / `Ctrl+O` | 打开文件或文件夹 |
| `⌘F` / `Ctrl+F` | 搜索当前目录 |
| `⌘R` / `Ctrl+R` | 刷新当前目录 |
| `⌘,` / `Ctrl+,` | 打开 / 关闭设置 |
| `Tab` | 展开 / 收起目录；全屏时弹出 / 关闭目录 |
| `F` / 双击画面 | 切换全屏 |
| `T` | 窗口置顶开关 |
| `I` / `⌘I` | 媒体详情与统计面板（含照片 EXIF） |
| `S` | 收藏 / 取消收藏当前文件 |
| `PageUp` / `PageDown` | 上一个 / 下一个媒体 |
| `Esc` | 依次关闭右键菜单 / 命令面板 / 设置 / 浮层 / 详情 / 搜索 / 目录弹窗，最后退出全屏 |
| `⌘Q` / `Ctrl+Q` | 退出 |

### 视频 / 音频

| 按键 | 作用 |
|---|---|
| `Space` / 单击画面 | 播放 / 暂停（播完从头重播） |
| `←` / `→` | 精确后退 / 前进（默认 5 秒，设置可调） |
| `Shift+←` / `Shift+→` | 6 倍大步后退 / 前进 |
| `,` / `.` | 逐帧后退 / 逐帧前进 |
| `↑` / `↓` / 画面滚轮 | 调节音量 |
| `M` | 静音 / 取消静音 |
| `[` / `]` / `Backspace` | 减速 / 加速（标准档位）/ 恢复 `1.00×` |
| `V` / `J` | 字幕显隐 / 切换字幕轨 |
| `Z` / `X` / `Shift+Z` | 字幕时间偏移 `−0.1s` / `+0.1s` / 归零 |
| `A` | 切换音轨 |
| `L` | 单曲循环开关 |
| `C` | 切换画面比例 / 铺满裁剪 |
| `R` / `Shift+R` | 顺 / 逆时针旋转 90° |
| `H` / `Shift+H` | 水平 / 垂直镜像翻转 |
| `⌘S` / `Ctrl+S` | 保存当前帧截图 |
| `Shift+[` / `Shift+]` | 上一章节 / 下一章节 |

### 照片

| 按键 | 作用 |
|---|---|
| `←` / `→` | 上一张 / 下一张 |
| 滚轮 / 双指捏合 / `=` / `-` | 以光标为中心缩放 |
| 拖拽 | 放大后平移 |
| `0` / `Space` / 双击 | 适应窗口 / 切换 1:1 原始大小 |
| `R` / `Shift+R` / `H` / `Shift+H` | 临时旋转 90° / 水平或垂直镜像（不改原文件） |
| `G` | 显示 / 隐藏底部内存缩略图条 |

## 架构

mpv 画在原生视图里，GPUI 在它上面画一层透明界面。视频帧不经过 GPUI，界面重绘也不影响视频；插件只在控制面与归一化矢量叠加面运行。

```
NSWindow / HWND
├── GPUI 视图（Metal / DComp topmost，透明背景） ← 标题栏、目录、工具栏、设置、命令面板、插件矢量叠层
└── 原生视频层（CAOpenGLLayer / DComp underlay） ← libmpv render API（OpenGL / D3D11）
```

详见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

## 构建与检查

需要 Rust stable（2024 edition）与 libmpv（macOS：`brew install mpv`，Windows：设置 `MPV_LIB_DIR`）。

```bash
cargo build --release                      # 本地开发构建（仅开发用，不得用 Homebrew libmpv 分发）
./target/release/iris test-media           # 打开测试素材
python3 scripts/check-project.py           # 架构边界与锁定元数据检查
cargo test --locked --workspace            # 单元测试（含 iris-api、iris-core 与外部插件握手）
cargo clippy --locked --workspace --all-targets -- -D warnings
```

### 源码媒体 SDK 与打包

`scripts/release/sources.lock.json` 固定上游提交和 SHA-256。`build-media.py` 关闭 GPL/version3/nonfree/Chromaprint，构建可替换的动态 LGPL 媒体库，产出对应源码、配置、清单和许可证。工作流见 `.github/workflows/release-build.yml` 与 `.github/workflows/check.yml`。

```bash
python scripts/release/build-media.py --target windows-x86_64 --work /tmp/iris-media-work --output /tmp/iris-media-sdk
python scripts/release/package.py --sdk PATH_TO_SDK --output target/dist-local --version 0.1.0 --mode local
```

### 测试素材

```bash
scripts/make-test-media.sh                 # 视频 + 字幕 + 大照片 → test-media/
scripts/make-photo-test-media.sh           # 照片边界素材 → test-media/photo/
python3 scripts/photo-harness.py out/ exif heic icc huge flip
python3 scripts/icc-report.py out/
```

## 设置与个人数据

所有个人数据保存在系统应用数据目录，内核对媒体文件和目录永远只读：

- **macOS**：`~/Library/Application Support/Iris/`（日志：`~/Library/Logs/Iris/iris.log`）
- **Windows**：`%APPDATA%\Iris\`（日志：`%LOCALAPPDATA%\Iris\iris.log`）

目录下包含：
- `settings.conf`：强类型 `key = value` 配置（原子写入，可在 `⌘,` 设置面板内实时修改）
- `records.tsv`：路径主键的收藏与视频续播进度
- `recent.tsv`：最近打开的文件与文件夹记录
- `plugins/`：外部插件安装目录（支持环境变量 `IRIS_PLUGINS` 覆盖）

环境变量 `IRIS_SETTINGS` / `IRIS_LOG` / `IRIS_PLUGINS` 可用于完全隔离的测试运行。

## 调试控制管道

仅 debug 或显式 `--features devtools` 构建启用，普通 release 忽略 `IRIS_CONTROL`。端点只允许当前用户连接：macOS Unix socket；Windows `\\.\pipe\iris-<唯一名称>`。

```bash
cargo build --release --features devtools
IRIS_CONTROL=/tmp/iris-test.sock ./target/release/iris test-media &
python3 scripts/control-client.py /tmp/iris-test.sock "state" "snap /tmp/iris.png"
```

## 代码结构

```
crates/
  iris-mpv/            libmpv 的手写 Rust FFI（client API + render API）
  iris-api/            无头插件协议（清单、权限、事件、语义命令、四类宿主容器、JSON stdio v1）
  iris-core/           无头内核，不依赖 GPUI
    src/player.rs      视口会话、后台打开准备、语义命令、轨道/章节/统计、mpv 事件翻译
    src/plugin/        插件管理器、外部 stdio 进程运行时、权限门控与官方内置插件（mod / builtins）
    src/store.rs       收藏、播放进度与最近打开记录（路径主键，原子写）
    src/photo.rs       照片文件头：方向、尺寸、内嵌 ICC/nclx 与轻量 EXIF 摘要
    src/icc.rs         色彩配置文件匹配与显示名称
    src/input.rs       上下文驱动的指针与手势策略
    src/geometry.rs    照片坐标与光标锚点缩放／平移
    src/library.rs     目录扫描（视频/照片/音频/字幕/子目录）、自然排序与多维排序
    src/settings.rs    用户设置（原子落盘与范围校验）
    src/logging.rs     有界运行日志
  iris/                GPUI 应用
    src/main.rs        启动、命令行、窗口几何恢复、原生菜单栏、系统打开请求
    src/ui.rs          主窗口编排、事件循环与 IME 输入路由
    src/ui/            state / actions / debug_cmd / titlebar / directory / viewport / controls /
                       popover / settings / context_menu / info / palette / plugin_ui / widgets
    src/video/         macOS CAOpenGLLayer / Windows D3D11 原生合成层
    src/instance.rs    Windows 单实例路径转发
    src/platform/      按 OS 隔离的窗口拖动/置顶、防休眠、捏合手势/媒体键、原生缩略图、截图（mod / macos / windows）
    src/theme.rs       方向 C 配色与尺寸
    src/assets.rs      编译进程序的 16px SVG 图标
    src/debug.rs       调试控制管道（仅 debug / devtools）
examples/plugins/
  watch-stats/         外部进程插件示例（Python 3 + JSON stdio v1）
Resources/             Space Grotesk & IBM Plex Mono 字体、16px SVG 图标
scripts/               测试素材、照片实测、仓库约束检查（check-project.py）、LGPL 构建与打包
docs/                  架构、插件开发指南、决策与路线图、测试记录
deny.toml              依赖许可证白名单（cargo-deny）
```

## 许可

主程序按 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 双重许可，由使用者任选。分发时动态链接自行源码构建的 LGPL 媒体库；对应源码、配置和第三方许可随产物分发。开发用的 Homebrew／第三方 GPL libmpv 不用于分发。详见 [决策记录](docs/DECISIONS.md#5-许可路线-b)。

`Resources/Fonts/` 中的字体按 SIL Open Font License 1.1 授权。
