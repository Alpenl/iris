# Iris

Iris 是一个以 [mpv](https://mpv.io) 为播放核心的高性能照片 / 视频播放器，面向 macOS 和 Windows。界面用 Rust 写成，基于 Zed 编辑器的 UI 框架 [GPUI](https://www.gpui.rs)；解码和渲染交给 libmpv。

> **状态：v0.1 收口验证。** macOS 既有播放方案保留；Windows D3D11 已通过实机播放、硬解、全屏和窗口交互验证。本轮只交付未签名本地包，macOS 新变更尚未实机复验，未完成正式签名／公证。

文档：[架构](docs/ARCHITECTURE.md) · [决策与路线图](docs/DECISIONS.md) · [测试与实测记录](docs/TESTING.md)

## 功能

- **播放**：libmpv 解码，默认硬解（macOS VideoToolbox，Windows D3D11VA）；方向键精确前后跳转 5 秒，播完按空格从头播放。
- **音轨与字幕**：A 切换音轨，J 切字幕轨，V 开关字幕。字幕优先简体中文，其次繁体中文、英文；没有匹配时仍显示一条完整字幕。
- **目录**：打开文件时扫描所在文件夹，按自然顺序（`ep2` 在 `ep10` 前）列出其中的视频和照片。
  - 停靠在窗口一侧，可收起成窄条。
  - 左右位置可以切换，设置会保存。
- **全屏**：工具栏悬浮在画面上。
  - 鼠标静止 2 秒后，工具栏、信息条和光标一起隐藏，动一下鼠标就回来。
  - 鼠标停在工具栏上、或目录弹窗打开时不会隐藏。
  - 工具栏上的目录按钮（或 Tab）在画面上方弹出目录，视频不缩小。
  - 工具栏显示时抬高字幕安全边距，隐藏后恢复；提示文字移到画面顶部。
- **照片**：←/→ 翻页；滚轮围绕光标缩放，放大后拖拽平移；0 / 按钮回到适应窗口，空格在适应窗口与 1:1 间切换。
- **打开方式**：命令行参数、拖入文件或文件夹、⌘O / Ctrl+O。

## 界面

石墨色模块面板：1px 线条、直角、橙色强调色 `#FF5B1F`；界面字体 Space Grotesk，数字和代码用 IBM Plex Mono。

```
┌ 标题栏 ─────────────────────────────────────────────┐
│ 目录 │                                               │
│ 视频 │                  视频画面                     │
│ 照片 │                                               │
│      │                                               │
├──────┴───────────────────────────────────────────────┤
│ 进度条                                              │
│ ⏮ ▶ ⏭  00:00              字幕 速度 音量  [目录] [⛶] │
└──────────────────────────────────────────────────────┘
```

## 架构

mpv 画在原生视图里，GPUI 在它上面画一层透明界面。视频帧不经过 GPUI，界面重绘也不影响视频。

```
NSWindow contentView
├── GPUI 视图（Metal，透明背景）  ← 标题栏、目录、工具栏、浮层
└── IrisVideoLayer（CAOpenGLLayer）← mpv render API（OpenGL）
```

- 视频视图的位置跟随 GPUI 量出来的视频区域，所以字幕和 OSD 永远落在画面内，不会被侧边栏或工具栏挡住。
- 时间显示每秒最多重绘 4 次；暂停时整个应用 CPU 约 0.5%。
- mpv 自带的 Lua 脚本（OSC、stats、console 等）全部关闭，界面完全由 Iris 自己画。
- **色彩**：macOS 图层与 mpv 输出一致标为 Display P3，由系统转到显示器；Windows SDR 图层读取所在显示器 ICC 交给 mpv，照片常见内嵌 ICC 逐张覆盖源原色。HDR/EDR 尚不支持。
- **Windows**：GPUI 占 DirectComposition topmost 目标，mpv 的独立 D3D11 交换链放在同窗口下层目标，不需要 ANGLE。实测范围和同步风险见 [架构](docs/ARCHITECTURE.md#4-windows-原生视频层)。

### 性能（macOS，Apple Silicon，release 构建，1440×900 窗口）

| 片源 | Iris CPU | mpv 命令行 CPU |
|---|---|---|
| 1080p60 H.264 | 约 24% | 约 30% |
| 4K30 HEVC 10bit | 约 17% | 约 17.5% |
| 暂停 | 约 0.5% | — |

测试中没有丢帧。之后的测法和数据见 [docs/TESTING.md](docs/TESTING.md)。

## 构建（macOS）

需要：

- Rust stable（2024 edition）
- Xcode Command Line Tools（不需要完整的 Xcode）
- libmpv：`brew install mpv`

```bash
cargo build --release
./target/release/iris                # 空窗口，拖入或 ⌘O 打开
./target/release/iris ~/Movies/a.mkv # 打开文件，并列出所在文件夹
./target/release/iris ~/Pictures     # 打开文件夹
```

构建脚本默认从 `/opt/homebrew/lib` 链接 libmpv；装在别处时用 `MPV_LIB_DIR` 指定：

```bash
MPV_LIB_DIR=/usr/local/lib cargo build --release
```

开发时可以动态链接系统 libmpv；分发必须改用本仓库脚本源码构建的 LGPL SDK，不得把 Homebrew 或第三方预编译媒体库混入发行包。

### Windows 开发构建

需要 Rust、Visual Studio C++ Build Tools、Windows SDK，以及支持 D3D11 render API 的 libmpv。设置 `MPV_LIB_DIR` 指向含 `mpv.lib` 的目录，运行时将该 SDK 的 DLL 放到 exe 同目录：

```powershell
$env:MPV_LIB_DIR = "C:\iris-media-sdk\prefix\lib"
cargo build --features devtools
Copy-Item C:\iris-media-sdk\prefix\bin\*.dll target\debug\
.\target\debug\iris.exe .\test-media
```

### 源码媒体 SDK 与打包

`scripts/release/sources.lock.json` 固定上游提交和 SHA-256。`build-media.py` 关闭 GPL/version3/nonfree/Chromaprint，构建可替换的动态媒体库，产出对应源码、配置、清单和许可证。Windows 可从 Linux/WSL 交叉构建；macOS 使用原生 Apple Silicon / Intel runner。

```bash
python scripts/release/build-media.py --target windows-x86_64 --work /tmp/iris-media-work --output /tmp/iris-media-sdk
# macOS 分别使用 --target macos-arm64 / macos-x86_64；前提与工具版本见脚本开头。
python scripts/release/package.py --sdk PATH_TO_SDK --output target/dist-local --version 0.1.0 --mode local
```

打包需在目标系统运行：Windows 生成便携 ZIP 和 Inno Setup 安装程序；macOS 生成 `.app` / ZIP / DMG。`--mode local` 明确为未签名／本地 ad-hoc；`--mode release` 缺少证书或公证凭据即失败。构建矩阵及凭据名称见 `.github/workflows/release-build.yml`，本地验证记录见 [TESTING.md](docs/TESTING.md)。

本轮已生成并实际安装／启动／卸载验证的 Windows 本地包：

- 安装包：`target/dist-final/Iris-0.1.0-windows-x86_64-local-setup.exe`
- 便携包：`target/dist-final/Iris-0.1.0-windows-x86_64-local.zip`
- 同目录每个包附 `.build-manifest.json`、`.corresponding-source.tar.gz`、`.licenses.tar.gz`，再分发时一并保留。包内也包含媒体、Rust 及 Iris 对应源码和许可证，因此体积较大。

这些产物均为 **unsigned-local**；不是正式签名发行。`target/dist-local/` 是旧版验证／编译缓存，不应使用其中旧包替代 `dist-final`。

### 测试素材

`scripts/make-test-media.sh` 用 ffmpeg 生成一组测试文件到 `test-media/`（已在 `.gitignore` 中）：1080p60、4K HEVC 10bit、带中文字幕的 MKV，以及几张大尺寸照片。

```bash
scripts/make-test-media.sh
./target/release/iris test-media
```

照片边界测试（EXIF / HEIC 方向、内嵌 ICC、超大图、翻页延迟）另有素材脚本和自动测量脚本，用法见 [docs/TESTING.md](docs/TESTING.md)：

```bash
scripts/make-photo-test-media.sh                       # 素材 → test-media/photo/
python3 scripts/photo-harness.py out/ exif heic icc huge flip
python3 scripts/icc-report.py out/
```

## 快捷键

| 按键 | 作用 |
|---|---|
| 空格 | 视频播放 / 暂停（播完则重播）；照片切换适应窗口 / 1:1 |
| ← / → | 精确后退 / 前进 5 秒；照片上一张 / 下一张 |
| ↑ / ↓ | 音量 |
| `[` / `]` | 减速 / 加速 |
| Tab | 展开 / 收起目录；全屏时弹出 / 关闭目录 |
| F，双击画面 | 全屏 |
| Esc | 先关闭目录弹窗，再退出全屏 |
| V | 字幕开关 |
| J | 切换字幕轨 |
| A | 切换音轨 |
| 0，`=` / `-` | 照片适应窗口；放大 / 缩小 |
| PageUp / PageDown | 上一个 / 下一个 |
| ⌘O（Windows：Ctrl+O） | 打开 |
| ⌘Q（Windows：Ctrl+Q） | 退出 |

## 设置

设置保存在一个 `key = value` 文本文件里：

- macOS：`~/Library/Application Support/Iris/settings.conf`
- Windows：`%APPDATA%\Iris\settings.conf`

目前只有一项：

```
directory_side = left   # 或 right
```

环境变量 `IRIS_SETTINGS` 可以改用别的文件，测试时不会动到真实设置。

运行日志每次启动覆盖，最多 4 MiB：Windows `%LOCALAPPDATA%\Iris\iris.log`，macOS `~/Library/Logs/Iris/iris.log`。`IRIS_LOG` 可覆盖路径；控制协议的逐命令回复仍在 stderr。

## 调试控制管道

仅 debug 或显式 `devtools` 构建启用，普通 release 忽略 `IRIS_CONTROL`。端点只允许当前用户连接：macOS Unix socket；Windows `\\.\pipe\iris-<唯一名称>`。回复写到 Iris stderr。

```bash
cargo build --release --features devtools
IRIS_CONTROL=/tmp/iris-test.sock ./target/release/iris test-media &
python3 scripts/control-client.py /tmp/iris-test.sock "state" "snap /tmp/iris.png"
```

Windows 例：设置 `$env:IRIS_CONTROL = '\\.\pipe\iris-test'` 后启动独立测试进程，再用 `python scripts/control-client.py $env:IRIS_CONTROL "state"` 连接。测试版不得作为发行包。

| 命令 | 作用 |
|---|---|
| `key <按键>` | 模拟按键，如 `key tab`、`key f` |
| `move <x> <y>` / `click <x> <y>` | 经原生事件模拟鼠标移动 / 左键点击（窗口逻辑坐标） |
| `snap <file.png>` | macOS 只截本窗口；Windows 截取窗口所在可见桌面区域，要求窗口未被遮挡 |
| `resize <w> <h>` | 调整窗口大小 |
| `open <路径>` | 打开文件或文件夹 |
| `state` | 把播放和界面状态打印到 stderr |
| `get <属性>` / `mpv <命令…>` | 读取 mpv 属性 / 执行 mpv 命令 |
| `quit` | 退出 |

## 代码结构

```
crates/
  iris-mpv/          libmpv 的 Rust 绑定（client API + render API）
  iris/
    src/main.rs      启动、字体、窗口
    src/ui.rs        主窗口：标题栏、目录、视频区、工具栏、全屏浮层
    src/player.rs    视口会话、语义命令、播放状态、mpv 事件翻译
    src/input.rs     上下文指针策略
    src/geometry.rs  照片坐标与缩放／平移
    src/video/       macOS CAOpenGLLayer / Windows D3D11 原生层
    src/logging.rs   有界运行日志（IRIS_LOG 可覆盖）
    src/library.rs   文件夹扫描
    src/settings.rs  用户设置
    src/theme.rs     颜色、字体、尺寸
    src/platform.rs  原生窗口辅助（拖动、截图、模拟输入）
    src/debug.rs     调试控制管道
Resources/Fonts/     Space Grotesk、IBM Plex Mono（SIL OFL 1.1）
scripts/             测试素材生成、照片实测脚本
docs/                架构、决策与路线图、测试记录
deny.toml            依赖许可证检查（cargo-deny）
```

## 已知问题与计划

- [x] Windows D3D11 视频层、硬解及首轮窗口交互验证
- [x] 精确跳转、播完空格重播、照片缩放／平移、音轨切换
- [x] 全屏字幕避让；提示文字移到顶部
- [x] 普通 release 禁用调试控制，本地流只允许同用户
- [ ] macOS 本轮新变更的实机复验；Apple Silicon / Intel 安装包验证
- [ ] 正式分发 Developer ID / 公证、Authenticode 签名（本轮没有凭据）
- [ ] macOS 全屏动画中第一次 Esc 的历史问题仍待实机验证
- [ ] 目录照片过滤尚未实现；收藏不属于本轮范围，不再显示虚假收藏入口
- [x] HEIC 的方向标记被忽略，竖拍照片横着显示：已从文件头读出方向交给 mpv
- [x] 超过 16384 像素的图片显示为蓝屏：长边超过 8192 的图先缩小再显示（4000×40000 解码时内存约 1 GB）
- [x] 照片内嵌的 ICC 配置文件被忽略（P3 / Adobe RGB 照片偏淡）：已按配置文件逐张覆盖原色，常见配置文件以外的仍按 sRGB

## 许可

主程序按 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 双重许可，由使用者任选。分发时动态链接自行源码构建的 LGPL 媒体库；对应源码、配置和第三方许可随产物分发。开发用的 Homebrew／第三方 GPL libmpv 不用于分发。详见 [决策记录](docs/DECISIONS.md#5-许可路线-b)。

`Resources/Fonts/` 中的字体按 SIL Open Font License 1.1 授权，许可全文与字体放在一起。
