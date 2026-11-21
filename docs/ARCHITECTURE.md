# Iris 架构

本文记录 macOS / Windows 原生合成、视口会话和色彩管线。决策和进度见 [DECISIONS.md](DECISIONS.md)，测试方法和实测数据见 [TESTING.md](TESTING.md)。Windows 已有实机证据；本轮 macOS 变更仍缺运行环境复验。

## 1. 分层

```
crates/
  iris-mpv/   libmpv 的手写 FFI（不用 bindgen）：client API + render API
  iris-core/  无头内核：会话、照片探测、目录扫描、设置、记录；Cargo.toml 不许依赖 GPUI 或原生窗口 API
  iris/       GPUI 应用：窗口、界面、平台视频层、单实例、调试管道
```

依赖方向只能是 `iris` → `iris-core` → `iris-mpv`。`iris/src/main.rs` 把 core 的模块原样再导出（`use iris_core::{player, …}`），界面代码仍写 `crate::player`。`scripts/check-project.py` 检查 `iris-core` 没有 GPUI 依赖、界面文件里没有 mpv 调用。

- `crates/iris-mpv/src/sys.rs`：手写的 C 声明。render API 同时声明了 OpenGL 和 D3D11 两套常量（`MPV_RENDER_API_TYPE_D3D11`、`MPV_RENDER_PARAM_D3D11_INIT_PARAMS = 21`、`MPV_RENDER_PARAM_D3D11_FBO = 22`）。
- `crates/iris-mpv/build.rs`：macOS 默认链接 `/opt/homebrew/lib` 的 libmpv（Homebrew 的 pkg-config 因 libarchive 是 keg-only 而失败）；其他平台必须用 `MPV_LIB_DIR` 指定。始终动态链接。
- `crates/iris-core/src/player.rs`：`Session` 持有播放状态、目录、当前绝对路径，统一处理语义命令、照片加载选项和 mpv 事件翻译。UI 不直接调用 mpv 属性或命令。打开分两步：`player::prepare` 做目录扫描和照片文件头读取，只有阻塞文件 I/O，界面放到后台线程跑；`Session::open` 在 UI 线程发 `loadfile`。连续快速打开时界面用代数（generation）丢弃过期结果。
- `crates/iris-core/src/store.rs`：收藏和播放进度，按绝对路径做主键，存在设置文件旁的 `records.tsv`（路径十六进制编码，写临时文件再改名）。只记录已加载完成的视频进度（`Session::progress`），续播点取 5 秒以后、片尾 5 秒以前。
- `crates/iris-core/src/{photo,icc}.rs`：照片文件头和内嵌配置文件，见第 3 节；`input.rs` 解析指针策略，`geometry.rs` 管理窗口逻辑坐标、媒体像素和缩放／平移；`settings.rs`、`logging.rs` 是设置和运行日志。
- `crates/iris-core/src/library.rs`：扫描所在文件夹，按自然顺序排序（`ep2` 在 `ep10` 前）。
- `crates/iris/src/ui.rs` + `ui/`：窗口持有 `Vec<Viewport>`（当前长度 1）；视口拥有会话、原生画面层、布局矩形和照片变换。键盘上下文由 render 状态推导。`ui.rs` 是窗口状态和动作；`ui/titlebar.rs`、`ui/directory.rs`（目录和全部／视频／照片／收藏过滤）、`ui/viewport.rs`（画面区）、`ui/controls.rs`（工具栏）只负责渲染。
- `crates/iris/src/instance.rs`：Windows 单实例，见第 7 节。macOS 由系统把打开请求发给已运行的进程，经 GPUI 的 `on_open_urls` 进来，只接受 `file://`。
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
| 照片内嵌 ICC | 已完成 | mpv 的 libmpv OpenGL 路径忽略内嵌 ICC。`crates/iris-core/src/photo.rs` 只读文件头，从 JPEG / PNG / WebP / HEIC / AVIF / TIFF 取出配置文件（HEIC 也认 nclx），由 `icc.rs` 按 D50 原色坐标和曲线匹配 sRGB、Display P3、Adobe RGB、BT.2020、ProPhoto；`player.rs` 的 `prepare` / `load` 把结果作为 `loadfile` 单文件选项（`vf` 里 mpv 自带的 `format` 滤镜带上 `primaries` / `gamma`，外加 `gamut-mapping-mode=clip`）传给 mpv，换文件时自动撤销。照片超出 P3 的颜色直接裁剪，和 ColorSync 一致（mpv 默认的压缩会让 Adobe RGB 纯绿偏青）；视频保留 mpv 默认的映射。识别不了的配置文件仍按 sRGB。|
| 换屏 | 无需额外处理（当前方案） | 显示器转换由 Core Animation 按窗口所在屏幕完成。如果以后改成把显示器 ICC 交给 mpv（`MPV_RENDER_PARAM_ICC_PROFILE = 6`，`sys.rs` 里尚未声明），就必须在窗口换屏时重新读取。|
| HDR / EDR | 未开始 | 需要换到支持 EDR 的图层（如 `CAMetalLayer`）和 mpv 的对应输出，属于后续版本。|

### 长期：照片管线

照片走 mpv 时，内嵌 ICC、HEIC 方向和超大图都由 `photo.rs` 读文件头后用单文件选项补上：HEIF 的 `irot` / `imir` 写进 `format=rotate=…`（GPU 旋转；`video-rotate` 对分块 HEIC 无效）和 `hflip` / `vflip`；长边超过 8192 的图先经 `vf=scale` 在 CPU 上缩小。修补不了的是翻页慢和无法预解码下一张（见 TESTING.md）。长期方案是单独的原生照片层：macOS 上用 ImageIO 解码，把 `CGImage` 设为一个 `CALayer` 的内容，系统自动做色彩管理；看图时隐藏 mpv 视频层。**不要**用 GPUI 的图片元素显示照片：它是 8 位 sRGB、没有色彩管理，会丢掉 P3 色域。Windows 上对应 WIC + 原生层，尚未调研。

## 4. Windows 原生视频层

`crates/iris/src/video/windows.rs` 已实现 D3D11 + DirectComposition。独立渲染线程拥有 D3D immediate context 和 mpv render context；UI 只更新合并后的布局并唤醒线程，视频帧不经 CPU 或 GPUI。实机已验证 `d3d11va`、透明控件叠加、窗口／最大化／全屏、鼠标实时缩放及正常退出；不能由此保证所有 GPU 和系统版本。

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

### 实现：同一个 HWND 上的两个 DirectComposition 目标

- 每个 HWND 有两个合成目标槽位：topmost = true 和 topmost = false，false 永远在 true 下面。GPUI 占了 true。
- Iris 用**自己的** D3D11 设备（不带 SINGLETHREADED）和自己的 DComp 设备，在同一个 HWND 上建 `CreateTargetForHwnd(hwnd, false)`，放 mpv 渲染的交换链。不碰 GPUI 私有的视觉树。
- DComp 视觉树不截获鼠标和键盘，输入仍全部落在 GPUI 窗口上，和 macOS 一致。
- 绑定使用微软 `windows` crate，COM 引用自动管理；关闭时先停止并 join 视频线程、释放 render context，再退出 mpv。
- `crates/iris/app.manifest` 启用 Common Controls v6 与 PerMonitorV2。缺少 v6 时 GPUI 导入的 `TaskDialogIndirect` 会令进程在 `main` 前失败，首次 Windows 上机已发现并修复。

### 已验证边界与剩余风险

1. **独立提交仍存在理论帧差**：窗口边框实拖过程中截图未见明显错位，最大化、全屏前后也正常；未用高速摄影证明每帧严格同步，不承诺零帧差。
2. **透明背景**：当前 `Opaque` 下 GPUI 仍清透明，视频透出；本机没有观察到系统强调色染色。保留平台行为说明，不增加猜测性的底层补丁。
3. **环境变量**：`main` 在创建线程前清掉 `GPUI_DISABLE_DIRECT_COMPOSITION`，避免环境把 GPUI 切换成不透明普通交换链。
4. **截图**：Windows 调试 `snap` 捕获窗口所在桌面矩形，可看见两个 DComp 目标；要求窗口可见、未被遮挡，不是离屏／后台窗口截图。macOS 仍为只截本窗口。
5. **调试流**：Windows 当前用户 SID 的保护 DACL、拒绝远程命名管道；macOS Unix socket 权限 0600 并检查 peer uid。单行 16 KiB、待处理队列最多 256 条。普通 release 不创建监听。
6. **SDR 色彩**：D3D 目标为 BGRA8；读取所在显示器的系统 ICC 交给 mpv / Little CMS，移动到另一显示器后更新。无配置文件时使用 SDR sRGB。HDR/EDR、任意 ICC 与 Windows ACM 全组合未验证，不宣称 HDR 支持。


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

`crates/iris/src/debug.rs`：debug 或显式 `--features devtools` 构建中，`IRIS_CONTROL` 指向本地流端点（macOS Unix socket、Windows `\\.\pipe\iris-...`）。逐行读取 `key`、`move`、`click`、`snap`、`resize`、`open`、`filter`、`state`、`get`、`mpv`、`quit`。客户端为 `scripts/control-client.py`；回复保留在 stderr，运行日志另外走 `log`。

- 合成鼠标事件必须在 GPUI update 之外发出：macOS 发送 NSEvent，Windows 发送原生窗口消息，均经 GPUI 正常命中测试。
- `snap`：macOS 用 `CGWindowListCreateImage`；Windows 捕获窗口占据的可见桌面区域，不能用于被遮挡窗口的自动像素对比。

## 7. 安全边界

- **外部传进来的内容永远不能变成 mpv 选项或命令**（文件关联、URL scheme、拖入的路径都一样）。教训是 IINA 的 CVE-2026-47114（GHSA-w5xh-98j7-jp5q）：`iina://` 地址里的 mpv 参数没过滤，导致命令注入。
- 任意 mpv 控制仅用于 debug／显式 `devtools` 构建，同用户访问，普通 release 不监听；不得把性能测试二进制当发行包。
- 媒体和所在目录只读；设置、日志和个人记录（`records.tsv`）保存在系统应用数据目录，不往媒体目录写隐藏文件或缓存。
- Windows 单实例：`Local\Iris.Player` 互斥量判断是否已有实例；后来的进程把绝对路径（UTF-16，长度前缀，上限 32767 个单位）写进 `\\.\pipe\iris-open-<会话号>-<用户 SID>` 后退出。管道和调试管道共用 `platform::pipe::create_inbound`：只建第一个实例、拒绝远程客户端、DACL 只给当前用户。客户端先用 `GetNamedPipeServerSessionId` 确认服务端在同一登录会话。管道里只有路径，进到界面后和拖入的路径走同一条 `open_path`。设置了 `IRIS_SETTINGS` 的测试实例不参加单实例。

## 8. 规划目录树与实际代码

2026-11-20 规划讨论稿里有一棵三层目录树（`iris-mpv` / `iris-core` / `iris`）。三层结构已经采纳；其余部分按“内核克制、不过早抽象”调整如下：

| 规划稿 | 实际 | 原因 |
|---|---|---|
| `iris-core` 不依赖 GPUI | 已拆分（Windows 原生合成验证之后） | 和规划一致；`cargo test -p iris-core` 不起窗口 |
| `engine/` 下的 `MediaBackend` trait | 没有；`player::Session` 直接用 `iris-mpv` | 只有 mpv 一套实现，等真有第二套再提炼（DECISIONS 4.5） |
| `command.rs` / `state.rs` / `event.rs` 分文件 | 都在 `player.rs`（`Command`、`PlaybackState`、`SessionEvent`） | 文件还小，拆开没有收益 |
| `viewport.rs`、`playlist.rs`、`config.rs` | `geometry.rs`、`library.rs`、`settings.rs` | 只是名字不同 |
| `platform/` 下按 OS 分目录，加 `VideoUnderlay` trait | `video/{mac,windows}.rs` + `platform.rs`，按 `cfg` 隔离 | 两个平台的生命周期差别大，暂不定公共 trait |
| `ui/` 拆成 workspace / titlebar / toolbar / sidebar / osd | `ui.rs` + `ui/{titlebar,directory,viewport,controls}.rs` | 提示和空状态还在 `ui.rs` 里，量小 |
| `control.rs`（原 `debug.rs`） | 仍叫 `debug.rs` | 只在 debug / `devtools` 构建启用 |
| `iris-api` crate、插件 | 没有 | 插件是中长期目标，现在不写插件代码 |

