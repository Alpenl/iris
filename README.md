# Iris

Iris 是一个以 [mpv](https://mpv.io) 为播放核心的高性能照片 / 视频播放器，面向 macOS 和 Windows。界面用 Rust 写成，基于 Zed 编辑器的 UI 框架 [GPUI](https://www.gpui.rs)；解码和渲染交给 libmpv。

> **状态：技术验证（spike）。** macOS 版已经可以日常试用；Windows 版的视频层还没写，目前不能编译。

文档：[架构](docs/ARCHITECTURE.md) · [决策与路线图](docs/DECISIONS.md) · [测试与实测记录](docs/TESTING.md)

## 功能

- **播放**：所有格式交给 mpv，默认硬解（macOS 上是 VideoToolbox，含 4K HEVC 10bit）。
- **字幕**：内封字幕自动选轨，优先简体中文，其次繁体中文、英文；没有匹配时仍显示一条完整字幕。
- **目录**：打开文件时扫描所在文件夹，按自然顺序（`ep2` 在 `ep10` 前）列出其中的视频和照片。
  - 停靠在窗口一侧，可收起成窄条。
  - 左右位置可以切换，设置会保存。
- **全屏**：工具栏悬浮在画面上。
  - 鼠标静止 2 秒后，工具栏、信息条和光标一起隐藏，动一下鼠标就回来。
  - 鼠标停在工具栏上、或目录弹窗打开时不会隐藏。
  - 工具栏上的目录按钮（或 Tab）在画面上方弹出目录，视频不缩小。
- **照片**：←/→ 切换上一张 / 下一张。
- **打开方式**：命令行参数、拖入文件或文件夹、⌘O。

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
- **色彩**：视频图层标注为 Display P3，mpv 按 Display P3 输出，再由系统转换到实际屏幕，和 GPUI 界面走同一条色彩管理路径。
- **Windows（计划）**：GPUI 在 Windows 上用 DirectComposition 占窗口的 topmost 合成目标；Iris 计划在同一窗口的非 topmost 目标上放 mpv 的 D3D11 画面，不需要 ANGLE。方案和风险见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

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

目前是动态链接，运行时需要系统里有 libmpv，还没有打包成 `.app`。

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
| 空格 | 播放 / 暂停 |
| ← / → | 后退 / 前进 5 秒；照片为上一张 / 下一张 |
| ↑ / ↓ | 音量 |
| `[` / `]` | 减速 / 加速 |
| Tab | 展开 / 收起目录；全屏时弹出 / 关闭目录 |
| F，双击画面 | 全屏 |
| Esc | 先关闭目录弹窗，再退出全屏 |
| V | 字幕开关 |
| J | 切换字幕轨 |
| PageUp / PageDown | 上一个 / 下一个 |
| ⌘O（Windows：Ctrl+O） | 打开 |
| ⌘Q | 退出 |

## 设置

设置保存在一个 `key = value` 文本文件里：

- macOS：`~/Library/Application Support/Iris/settings.conf`
- Windows：`%APPDATA%\Iris\settings.conf`

目前只有一项：

```
directory_side = left   # 或 right
```

环境变量 `IRIS_SETTINGS` 可以改用别的文件，测试时不会动到真实设置。

## 调试控制管道

设置 `IRIS_CONTROL` 指向一个 FIFO 后，Iris 会逐行读取命令，便于脚本化检查：

```bash
mkfifo /tmp/iris.fifo
IRIS_CONTROL=/tmp/iris.fifo ./target/release/iris test-media &
echo "state" > /tmp/iris.fifo
echo "snap /tmp/iris.png" > /tmp/iris.fifo
```

| 命令 | 作用 |
|---|---|
| `key <按键>` | 模拟按键，如 `key tab`、`key f` |
| `move <x> <y>` / `click <x> <y>` | 模拟鼠标移动 / 左键点击（窗口坐标，macOS） |
| `snap <file.png>` | 截取本窗口（macOS，无需录屏权限） |
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
    src/player.rs    mpv 选项和事件泵
    src/video/mac.rs macOS 视频层（CAOpenGLLayer + mpv render API）
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

- [ ] Windows 视频层（D3D11），以及 Windows 上的完整验证
- [ ] 打包：随应用分发 libmpv，生成 `.app` / 安装包
- [ ] 全屏时工具栏出现会遮住底部字幕
- [ ] 窗口模式下的提示文字会和字幕重叠
- [ ] 播放结束后按空格不会从头播放
- [ ] 方向键跳转只落在关键帧（尚未开启精确跳转）
- [ ] 全屏动画进行中第一次按 Esc 可能无效
- [ ] 目录的“照片”“收藏”标签还只是占位
- [x] HEIC 的方向标记被忽略，竖拍照片横着显示：已从文件头读出方向交给 mpv
- [x] 超过 16384 像素的图片显示为蓝屏：长边超过 8192 的图先缩小再显示（4000×40000 解码时内存约 1 GB）
- [x] 照片内嵌的 ICC 配置文件被忽略（P3 / Adobe RGB 照片偏淡）：已按配置文件逐张覆盖原色，常见配置文件以外的仍按 sRGB

## 许可

许可证尚未确定，目前不授予开源许可（`Cargo.toml` 中为 `UNLICENSED`）。方向已定为宽松协议（Apache-2.0 或 MIT OR Apache-2.0），分发时动态链接自行构建的 LGPL 版 libmpv / FFmpeg；开发时用的 Homebrew libmpv 是 GPL 构建，不用于分发。详见 [docs/DECISIONS.md](docs/DECISIONS.md)。

`Resources/Fonts/` 中的字体按 SIL Open Font License 1.1 授权，许可全文与字体放在一起。
