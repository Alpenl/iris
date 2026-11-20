# Iris 架构

本文记录当前代码的实际结构（macOS 技术验证）、Windows 的计划方案和风险，以及色彩管线。决策和进度见 [DECISIONS.md](DECISIONS.md)，测试方法和实测数据见 [TESTING.md](TESTING.md)。

## 1. 分层

```
crates/
  iris-mpv/   libmpv 的手写 FFI（不用 bindgen）：client API + render API
  iris/       GPUI 应用：窗口、界面、平台视频层、调试管道
```

- `crates/iris-mpv/src/sys.rs`：手写的 C 声明。render API 同时声明了 OpenGL 和 D3D11 两套常量（`MPV_RENDER_API_TYPE_D3D11`、`MPV_RENDER_PARAM_D3D11_INIT_PARAMS = 21`、`MPV_RENDER_PARAM_D3D11_FBO = 22`）。
- `crates/iris-mpv/build.rs`：macOS 默认链接 `/opt/homebrew/lib` 的 libmpv（Homebrew 的 pkg-config 因 libarchive 是 keg-only 而失败）；其他平台必须用 `MPV_LIB_DIR` 指定。始终动态链接。
- `crates/iris/src/player.rs`：mpv 选项和事件泵。`vo=libmpv`、`hwdec=auto-safe`、`keep-open`、`image-display-duration=inf`；mpv 自带的 Lua 脚本、OSC、默认按键、配置文件全部关闭，界面完全由 Iris 自己画。
- `crates/iris/src/ui.rs`：主窗口。目前按键直接绑定到动作，部分动作在 UI 里直接调用 mpv（约 13 处），方向键按 `Kind::Photo` 分支。这是已知的技术债，见 DECISIONS.md 的“输入上下文”和“收拢 mpv 调用”。
- `crates/iris/src/library.rs`：扫描所在文件夹，按自然顺序排序（`ep2` 在 `ep10` 前）。
- `crates/iris/src/platform.rs`：原生窗口辅助。窗口拖动是自己实现的，因为 GPUI 0.2.2 的 `start_window_move()` 在 macOS 和 Windows 上是空函数（`gpui-0.2.2/src/platform.rs:536`，只有 Linux 后端重写了）。
- `crates/iris/src/debug.rs`：调试控制管道（`IRIS_CONTROL`），见第 6 节。

## 2. macOS 视频合成（已验证）

```
NSWindow contentView
├── GPUI 视图（Metal，非不透明，每帧清成透明）  ← 标题栏、目录、工具栏、浮层
└── IrisVideoLayer 视图（CAOpenGLLayer）          ← mpv render API（OpenGL）
```

代码在 `crates/iris/src/video/mac.rs`。

- 视频视图用 `addSubview:positioned:NSWindowBelow relativeTo:gpui_view` 插在 GPUI 视图下面。视频帧不经过 GPUI，界面重绘也不影响视频。
- 视频视图的 frame 等于 GPUI 布局里量出来的视频区矩形（canvas 探针），并设置宽高自动伸缩以跟随实时缩放。字幕和 OSD 因此永远落在画面内。曾试过用 `video-margin-ratio-*` 定位视频，结果字幕跑到目录栏下面，已放弃。
- mpv 的 render 更新回调只触发图层 `setNeedsDisplay`，绘制在 `drawInCGLContext:` 里完成。
- 时间显示每秒最多重绘 4 次，暂停时整个应用 CPU 约 0.5%。
- GPUI 0.2.2 的 `surface()` 只接受 NV12 的 CVPixelBuffer，所以没有走“把帧交给 GPUI 画”的方案。

**尚未人工验证**：拖动窗口边缘实时缩放时，视频层和界面会不会错位几帧。程序模拟不出真实拖动，需要人手动测。这和 Windows 的同步风险是同一类问题。

## 3. 色彩管线

### 原则

- 用 render API 时，交换链和图层归 Iris 管，不归 mpv 管。mpv 的 `target-colorspace-hint` 管不到 Iris 的图层，色彩空间要 Iris 自己设置。
- **图层标注的色彩空间必须和 mpv 的输出目标一致**。不一致或不标注，就会少转一次或多转一次。
- GPUI 的 Metal 图层本身有色彩管理（强调色 `#FF5B1F` 在 P3 屏上测得 (236,103,54)，转换正确）。

### macOS 现状

| 项目 | 状态 | 做法 |
|---|---|---|
| 视频层色彩空间 | 已完成 | `CAOpenGLLayer` 标注为 Display P3；mpv 设 `target-prim=display-p3`、`target-trc=srgb`。Core Animation 再转换到实际屏幕，和 GPUI 图层走同一条路（`video/mac.rs`，`setColorspace:` 和 `target-prim`/`target-trc` 两处）。|
| sRGB / BT.709 内容 | 已验证 | 与系统色彩转换的预期值相差不超过 3 个色阶；CPU 无变化。修复前图层未标注，系统把 mpv 输出当成屏幕原始值，sRGB 内容在 P3 屏上过饱和。|
| 照片内嵌 ICC | 已完成 | mpv 的 libmpv OpenGL 路径忽略内嵌 ICC。`crates/iris/src/photo.rs` 只读文件头，从 JPEG / PNG / WebP / HEIC / AVIF / TIFF 取出配置文件（HEIC 也认 nclx），由 `icc.rs` 按 D50 原色坐标和曲线匹配 sRGB、Display P3、Adobe RGB、BT.2020、ProPhoto；`ui.rs` 的 `load` 把结果作为 `loadfile` 单文件选项（`vf` 里 mpv 自带的 `format` 滤镜带上 `primaries` / `gamma`，外加 `gamut-mapping-mode=clip`）传给 mpv，换文件时自动撤销。照片超出 P3 的颜色直接裁剪，和 ColorSync 一致（mpv 默认的压缩会让 Adobe RGB 纯绿偏青）；视频保留 mpv 默认的映射。识别不了的配置文件仍按 sRGB。|
| 换屏 | 无需额外处理（当前方案） | 显示器转换由 Core Animation 按窗口所在屏幕完成。如果以后改成把显示器 ICC 交给 mpv（`MPV_RENDER_PARAM_ICC_PROFILE = 6`，`sys.rs` 里尚未声明），就必须在窗口换屏时重新读取。|
| HDR / EDR | 未开始 | 需要换到支持 EDR 的图层（如 `CAMetalLayer`）和 mpv 的对应输出，属于后续版本。|

### 长期：照片管线

照片走 mpv 时，内嵌 ICC、HEIC 方向和超大图都由 `photo.rs` 读文件头后用单文件选项补上：HEIF 的 `irot` / `imir` 写进 `format=rotate=…`（GPU 旋转；`video-rotate` 对分块 HEIC 无效）和 `hflip` / `vflip`；长边超过 8192 的图先经 `vf=scale` 在 CPU 上缩小。修补不了的是翻页慢和无法预解码下一张（见 TESTING.md）。长期方案是单独的原生照片层：macOS 上用 ImageIO 解码，把 `CGImage` 设为一个 `CALayer` 的内容，系统自动做色彩管理；看图时隐藏 mpv 视频层。**不要**用 GPUI 的图片元素显示照片：它是 8 位 sRGB、没有色彩管理，会丢掉 P3 色域。Windows 上对应 WIC + 原生层，尚未调研。

## 4. Windows 方案（计划，未验证）

Windows 视频层还没写，`crates/iris/src/video/mod.rs` 目前只有 macOS 实现，Windows 不能编译。手上也还没有 Windows 测试机。以下是只读源码得出的方案。

### GPUI 0.2.2 在 Windows 上的事实（`gpui-0.2.2/src/platform/windows/`）

- 窗口带 `WS_EX_NOREDIRECTIONBITMAP`（`window.rs:410`）。
- 交换链用 `CreateSwapChainForComposition`，alpha 为 PREMULTIPLIED（`directx_renderer.rs:1044`）；合成目标是 `CreateTargetForHwnd(hwnd, true)`，即 topmost 槽位（`directx_renderer.rs:793`）。
- 每帧把背景清成全透明（`directx_renderer.rs:189`）。
- 设了环境变量 `GPUI_DISABLE_DIRECT_COMPOSITION`（`directx_renderer.rs:29`）时，GPUI 改走不透明的普通交换链，下面的视频层就透不出来。
- `WindowBackgroundAppearance::Transparent` 会调用 `set_window_composition_attribute(hwnd, None, 2)`（`window.rs:768`），在 Windows 11 上会垫一层跟随系统强调色的底色（Zed issue #65402，仍未关闭）。

### mpv render API 的 D3D11 事实（mpv 0.42，`/opt/homebrew/include/mpv/render_d3d11.h`）

- 后端是 `MPV_RENDER_API_TYPE_D3D11`，参数是 `MPV_RENDER_PARAM_D3D11_INIT_PARAMS`（21）和 `MPV_RENDER_PARAM_D3D11_FBO`（22）。**没有** BACKEND 之类选择 gpu-next 的参数。不需要 ANGLE。
- mpv 用设备的 immediate context 渲染，**不恢复**设备上下文状态。
- 用 `D3D11_CREATE_DEVICE_SINGLETHREADED` 创建的设备不支持 d3d11va 直接硬解（只能 copy-back）。

### 方案：同一个 HWND 上的两个 DirectComposition 目标

- 每个 HWND 有两个合成目标槽位：topmost = true 和 topmost = false，false 永远在 true 下面。GPUI 占了 true。
- Iris 用**自己的** D3D11 设备（不带 SINGLETHREADED）和自己的 DComp 设备，在同一个 HWND 上建 `CreateTargetForHwnd(hwnd, false)`，放 mpv 渲染的交换链。不碰 GPUI 私有的视觉树。
- DComp 视觉树不截获鼠标和键盘，输入仍全部落在 GPUI 窗口上，和 macOS 一致。
- 实际证据：Ebitengine #3732 是对称的案例（游戏画面占 false 目标，另一个 DComp 设备在同一 HWND 的 true 目标上放 WebView2 界面），作者确认可用并测过拖动缩放。
- 绑定用微软的 `windows` crate（COM 对象自动 AddRef/Release），按需开 feature。

### 风险

1. **缩放不同步（最大风险）**：两个独立的 DComp 设备各自 Commit，互不同步。拖动缩放或最大化时，视频和界面可能差几帧。Ebitengine 那个案例里，单层画面最大化时都有 1~2 帧旧画面。只看源码证明不了，必须上机测。
2. **#65402 染色**：Iris 现在用 `WindowBackgroundAppearance::Opaque`（`crates/iris/src/main.rs`）。猜测 Opaque 不会调用染色的 API、但每帧仍清成透明，也许能直接避开 #65402，**未验证**。备选：在 false 目标的根部垫一层铺满客户区的不透明石墨色视觉，把底兜住。
3. **`GPUI_DISABLE_DIRECT_COMPOSITION`**：`main.rs` 在 Windows 上启动第一步清掉这个变量（已写，未在 Windows 上编译验证）。
4. **截图（`snap`）**：窗口带 NOREDIRECTIONBITMAP，GDI `BitBlt` 抓出来是黑的。可试 `Windows.Graphics.Capture`（默认有黄色边框，去掉需要较新的系统）或 `PrintWindow` + `PW_RENDERFULLCONTENT`。都没验证。
5. **调试管道**：`mkfifo` 在 Windows 上不存在，要换成命名管道（macOS 可同时改成 Unix socket），让一套测试脚本两边通用。

### 没有 Windows 机器时能做的

- 用 `cargo xwin check --target x86_64-pc-windows-msvc` 在 Mac 上做类型检查（`build.rs` 在 Windows 目标上不设默认路径，check 不链接，应能通过；未试）。
- 播放状态、列表、输入路由等与平台无关的工作不受影响。

## 5. 依赖

| 依赖 | 用途 | 规则 |
|---|---|---|
| `gpui = "=0.2.2"` | UI | crates.io 版，`default-features = false, features = ["font-kit", "runtime_shaders"]`，只需 Command Line Tools。锁死版本，主动升级。不引第三方 GPUI 组件库。|
| `iris-mpv`（本仓库） | libmpv FFI | 手写声明，不用 bindgen。|
| `objc 0.2`、`cocoa =0.26.0` | macOS 原生调用 | GPUI 0.2.2 内部也用这两个，迁到 objc2 后类型不互通，暂不迁移。|
| `futures` | 通道 | 用 GPUI 自带的 executor；Iris 自己不直接引 tokio（GPUI 的 HTTP 客户端会间接带进来，这不归 Iris 管）。|
| `raw-window-handle` | 取原生视图 | |

许可证检查用根目录的 `deny.toml`（`cargo deny check licenses`）。白名单除了常见宽松协议，还放行了 MPL-2.0（`dwrote`、`option-ext` 等，按文件生效的弱传染协议，可闭源链接）、CC0-1.0、BSL-1.0、NCSA、Unicode-3.0 和 LLVM-exception。

## 6. 调试控制管道

`crates/iris/src/debug.rs`：设置 `IRIS_CONTROL` 指向一个 FIFO 后，Iris 逐行读取命令（`key`、`move`、`click`、`snap`、`resize`、`open`、`state`、`get`、`mpv`、`quit`），命令在 `ui.rs` 里执行。用法见 README 和 TESTING.md。

- 合成鼠标事件：`Window::dispatch_event` 返回 crate 私有类型，外部调不了；GPUI 在自身 update 期间收到的平台输入会被丢弃。所以 `move`/`click` 在 `cx.update` 返回之后才把 NSEvent 发给 GPUI 的视图。
- `snap` 用 `CGWindowListCreateImage` 只截本窗口，不需要录屏权限（`platform.rs`）。

## 7. 安全边界

- **外部传进来的内容永远不能变成 mpv 选项或命令**（文件关联、URL scheme、拖入的路径都一样）。教训是 IINA 的 CVE-2026-47114（GHSA-w5xh-98j7-jp5q）：`iina://` 地址里的 mpv 参数没过滤，导致命令注入。
- 调试管道里的 `mpv <命令…>` 能执行任意 mpv 命令，必须只在调试时可用。目前只要设置了 `IRIS_CONTROL` 就会启用，发布前要收紧（见 DECISIONS.md）。
- Iris 对用户的媒体文件和目录只读，不写缓存或隐藏文件；个人数据放在系统的应用数据目录（目前只有 `settings.conf`）。
