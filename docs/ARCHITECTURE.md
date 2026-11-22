# Iris 架构

本文记录 Iris 的分层设计、macOS / Windows 原生视频合成、色彩管线、插件隔离架构与安全边界。决策和路线图见 [DECISIONS.md](DECISIONS.md)，插件开发规范见 [PLUGINS.md](PLUGINS.md)，测试方法与实测数据见 [TESTING.md](TESTING.md)。

## 1. 分层

```
crates/
  iris-mpv/   libmpv 的手写 FFI（不用 bindgen）：client API + render API
  iris-api/   无头插件协议：控制面事件、权限模型、语义命令与四类结构化 UI 槽位（仅依赖 serde）
  iris-core/  无头内核：会话、照片探测与 EXIF、目录扫描、设置、记录、插件运行时；不许依赖 GPUI 或原生窗口 API
  iris/       GPUI 应用：窗口、界面、平台视频层与系统桥接、单实例、调试管道
```

依赖方向严格单向：`iris` → `iris-core` → `iris-mpv`，且 `iris` / `iris-core` → `iris-api`。`iris/src/main.rs` 把 core 的模块原样再导出（`use iris_core::{player, …}`），界面代码仍写 `crate::player`。`scripts/check-project.py` 自动检查：

1. `iris-core` 不依赖 GPUI；
2. `iris-api` 既不依赖 GPUI 也不依赖 `iris-mpv`；
3. 界面代码里不出现 raw mpv 调用；
4. `devtools` 控制管道门禁未被普通发布版绕过。

### 核心模块职责

- `crates/iris-mpv/src/lib.rs` & `sys.rs`：手写的 C 声明与薄封装。短属性名路径采用栈缓冲（`with_cstr`）避免热路径堆分配，`wait_event` 防御空指针属性载荷。render API 同时声明了 OpenGL 和 D3D11 两套常量（`MPV_RENDER_API_TYPE_D3D11`、`MPV_RENDER_PARAM_D3D11_INIT_PARAMS = 21`、`MPV_RENDER_PARAM_D3D11_FBO = 22`）。
- `crates/iris-mpv/build.rs`：macOS 默认链接 `/opt/homebrew/lib` 的 libmpv（Homebrew 的 pkg-config 因 libarchive 是 keg-only 而失败）；其他平台必须用 `MPV_LIB_DIR` 指定。始终动态链接。
- `crates/iris-api/src/lib.rs`：控制面与矢量叠加面协议 v1（`PROTOCOL_VERSION = 1`），定义 `PluginManifest`（拒绝绝对路径、Windows 盘符前缀与 `..` 路径穿越，校验协议版本）、`Permission`、`PlaybackSnapshot`、`HostEvent`、`HostCommand`、`SidebarPanel`、`StatusItem`、`OverlayShape`、`HostMessage`/`HostMessageRef`/`PluginMessage` 与 `BuiltinPlugin` trait。
- `crates/iris-core/src/player.rs`：`Session` 持有播放状态、目录、子文件夹列表与当前绝对路径，统一处理语义命令（精确/大步跳转、倍速、音量、静音、循环、音轨、字幕显隐/切轨/偏移、`sub-auto=fuzzy`、章节切换、画面比例、旋转与镜像、截图、多文件后台准备队列 `PreparedQueue`）、照片加载选项和 mpv 事件翻译，并汇总 `MediaStats`（打开详情面板时按 1 Hz 增量刷新动态解码指标）与友好错误信息。音轨/字幕轨与章节列表采用单次 JSON FFI 读取（`track-list` / `chapter-list`）并在事件批次末尾合并刷新；切轨事件优先原地更新选中状态。设置修改走细粒度 setter，不再全量重写所有 mpv 属性。打开分两步：`player::prepare` / `prepare_queue` 在后台线程完成目录扫描与照片头解析，`Session::open` / `open_prepared_queue` 在 UI 线程执行 `loadfile`，界面以代数（generation）丢弃过期结果。
- `crates/iris-core/src/plugin/{mod,builtins}.rs`：`PluginManager` 管理内置官方插件（`iris.guides` 构图辅助线、`iris.ab-loop` A-B 循环与标记，带 400ms 循环防抖与小时格式）以及外部 JSON-lines stdio 子进程插件。外部插件采用非阻塞独立写线程（有界队列、满载时丢弃 `PositionTick`）、有界读线程（`fill_buf`/`consume` 单行上限 64 KiB + 事件驱动 `WakeSignal` 唤醒 UI，消除空转轮询）、有界 stderr 日志转发、符号链接逃逸校验与崩溃检测，退出时先发 `Shutdown` 等待宽限期再回收。
- `crates/iris-core/src/store.rs`：收藏与播放进度（`records.tsv`）以及最近打开历史（`recent.tsv`），采用查表式预分配十六进制路径编码与共用原子写（唯一临时后缀 + Windows 短重试）。记录脏标记 `dirty` 并支持后台线程异步写快照；片尾自动清理非收藏进度行。
- `crates/iris-core/src/{photo,icc}.rs`：带解析上限与防溢出保护的照片文件头解析（尺寸、EXIF 方向、HEIF 方向、内嵌 ICC、JPEG APP1 EXIF 摘要）与栈上原色匹配，见第 3 节。
- `crates/iris-core/src/{input,geometry,library,settings,logging}.rs`：
  - `input.rs`：指针与手势策略解析（含视频单击暂停、照片双击 1:1、双指捏合缩放）。
  - `geometry.rs`：窗口逻辑坐标、媒体像素与缩放／平移变换。
  - `library.rs`：扫描文件夹（支持视频、照片、音频 `Kind::Audio` 与子文件夹 `FolderEntry`；先检查 `file_type()` 与扩展名再 `stat`），零分配自然排序（`&str` 切片）及按名称 / 修改时间 / 大小升降序。
  - `settings.rs`：共用原子落盘的 `key = value` 偏好设置（目录侧边与宽度、排序、单曲循环、播放/跳转/解码/字幕/截图、照片默认视图与缩略图条、窗口置顶、禁用插件列表等）。
  - `logging.rs`：GUI 发布版运行日志（单次启动上限 4 MiB），默认启用 `iris=info,iris_core=info,iris_mpv=info`。
- `crates/iris/src/ui.rs` + `ui/`：窗口持有 `Vec<Viewport>`（当前长度 1）；视口拥有会话、原生画面层、布局矩形和照片变换。键盘上下文由 `render()` 状态纯函数推导，且单键 `Workspace` 快捷键（`Tab`/`F`/`T`/`Esc`）限定为 `Workspace && !TextInput`，避免拦截文本输入与 IME 组合。事件循环按批消费 mpv 事件，时间轴重绘按秒变或像素变化触发（短视频上限 4 Hz，长视频降至 1 Hz，全屏隐藏工具栏与空状态为 0 Hz）。
  - `ui/state.rs`：内聚状态结构体——`OverlayState`（互斥浮层状态机）、`ScrubState`、`DirectoryCache`（排序结果、照片切片与标签计数缓存）、`ThumbCache`（LRU 上限 80 项 + 并发上限 3 的优先级解码队列）与 `TextInputState`（光标/选区/剪贴板与 UTF-16 IME 组合状态）。
  - `ui/actions.rs`：全部 GPUI Action 声明、上下文快捷键绑定与命令处理器。
  - `ui/debug_cmd.rs`：`IRIS_CONTROL` 调试管道命令解析与状态输出。
  - `ui/titlebar.rs`：按平台习惯排布的窗口控制按钮（macOS 左、Windows 右）、当前文件与媒体元信息、窗口置顶图钉。
  - `ui/directory.rs`：基于 GPUI `uniform_list` 的虚拟化列表 + 右缘细滚动条、支持中文 IME 与剪贴板的目录搜索栏（`⌘F` / `Ctrl+F`）、排序切换、子文件夹/`..` 导航、目录/插件侧栏视图切换，以及边缘拖拽调宽手柄。
  - `ui/viewport.rs`：视频/照片区探针、独立轻量 OSD 胶囊（全屏不拉起工具栏）、顶部 Toast、音频播放卡片、空状态最近打开列表、照片内存缩略图底条（`G`）、单击暂停与照片双击 1:1 交互。
  - `ui/controls.rs`：44px 单行工具栏、60fps 跟手拖拽预览时间轴与章节刻度、连续可拖拽 0–100% 音量条、小窗响应式折叠、按钮锚点探针。
  - `ui/popover.rs`：按钮正上方锚定的字幕（含偏移微调）、音轨、倍速与更多操作浮层。
  - `ui/context_menu.rs`：画面与目录行方向 C 右键上下文菜单。
  - `ui/info.rs`：`I` / `⌘I` 媒体详情、缓存式解码统计与照片 EXIF 浮层（全屏置于目录对侧避免重叠）。
  - `ui/settings.rs`：居中多标签设置弹窗（外观、播放、字幕与音频、看图、插件、快捷键、数据管理）。
  - `ui/palette.rs`：`⌘P` / `Ctrl+P` 命令面板，支持中文 IME、剪贴板与 `ScrollHandle` 键盘跟随滚动，统一检索与触发内核和插件命令。
  - `ui/plugin_ui.rs`：宿主渲染的 3 个视觉插件槽位（视口矢量叠层、底栏状态徽章、侧栏插件面板）。
  - `ui/widgets.rs`：共用 SVG 图标、无框按钮、分隔线与悬停提示。图标是 `Resources/Icons/` 下的 16px SVG，经 `assets.rs` 编译进二进制。GPUI 0.2.2 的悬停提示按指针移入时的文本与位置缓存在元素状态中，所以点击后文字或位置会变的按钮把状态写进元素 `id`，确保旧提示随旧元素丢弃。
- `crates/iris/src/instance.rs`：Windows 单实例，见第 8 节。macOS 由系统把打开请求发给已运行进程，经 GPUI `on_open_urls` 进来，只接受 `file://`。
- `crates/iris/src/platform/{mod,macos,windows}.rs`：按 OS 拆分的原生系统桥接——自实现窗口拖动（GPUI 0.2.2 `start_window_move()` 在 macOS/Windows 上为空函数）、窗口置顶（macOS `NSFloatingWindowLevel` / Win32 `SetWindowPos`）、播放期防休眠 `SleepPreventer`（macOS `IOKit` / Win32 `SetThreadExecutionState`）、macOS 触控板双指捏合 `PinchMonitor`（持有 `NSView` 强引用避免 UAF）、macOS 媒体键与控制中心桥接 `MediaKeyBridge`（`MediaPlayer.framework`，同步真实倍速与清空状态）、默认截图目录，以及照片内存缩略图解码 `decode_thumbnail`（macOS `ImageIO` 原生 sRGB 缩放并反预乘为直通 BGRA、Windows 原生 WIC 缩放 + EXIF 方向校正，跨平台 `image` crate 带内存/尺寸上限回退）。
- `crates/iris/src/debug.rs`：调试控制管道（`IRIS_CONTROL`），见第 7 节。

## 2. macOS 视频合成（已验证）

```
NSWindow contentView
├── GPUI 视图（Metal，非不透明，每帧清成透明）  ← 标题栏、目录、工具栏、浮层、插件矢量叠层
└── IrisVideoLayer 视图（CAOpenGLLayer）          ← mpv render API（OpenGL）
```

代码在 `crates/iris/src/video/mac.rs`。

- 视频视图用 `addSubview:positioned:NSWindowBelow relativeTo:gpui_view` 插在 GPUI 视图下面。视频帧不经过 GPUI，界面重绘也不影响视频。
- 视频视图的 frame 等于 GPUI 布局里量出来的视频区矩形（canvas 探针），并设置宽高自动伸缩以跟随实时缩放。字幕和 OSD 因此永远落在画面内。曾试过用 `video-margin-ratio-*` 定位视频，结果字幕跑到目录栏下面，已放弃。
- mpv 的 render 更新回调通过容量 1 的有界通道唤醒 `iris-video` 渲染线程，绘制循环包在 `autoreleasepool` 中避免 60fps 长期播放泄漏 Objective-C 临时对象；关闭时严格按“释放 render context → join 渲染线程 → 清空 ivar 并移除视图”顺序执行，消除关窗数据竞争。
- 时间显示按秒变或进度条像素步进触发重绘（短视频上限 4 Hz，长视频降至 1 Hz，全屏隐藏工具栏与空状态为 0 Hz）。
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

照片主视图走 mpv 时，内嵌 ICC、HEIC 方向和超大图都由 `photo.rs` 读文件头后用单文件选项补上：HEIF 的 `irot` / `imir` 写进 `format=rotate=…`（GPU 旋转；`video-rotate` 对分块 HEIC 无效）和 `hflip` / `vflip`；长边超过 8192 的图先经 `vf=scale` 在 CPU 上缩小。底部可选的缩略图底条仅用于小图预览导航（160px 内存缓存），不替代主视口的色彩准确管线。长期方案仍是独立的原生照片主视图层：macOS 上用 ImageIO 解码，把 `CGImage` 设为一个 `CALayer` 的内容，系统自动做色彩管理；看图时隐藏 mpv 视频层。**不要**用 GPUI 的图片元素替代主视口照片显示：它是 8 位 sRGB、没有色彩管理，会丢掉 P3 色域。Windows 上对应 WIC + 原生层，尚未调研。

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

## 5. 插件系统与隔离架构

插件系统坚持**控制面 + 归一化矢量叠加面**边界，详细协议与开发示例见 [PLUGINS.md](PLUGINS.md)：

```
                  HostEvent (JSON-lines stdin / Rust trait)
┌──────────────┐ ─────────────────────────────────────────▶ ┌───────────────────────┐
│  Iris UI     │                                            │  PluginManager        │
│  4 宿主槽位   │ ◀───────────────────────────────────────── │  (iris-core)          │
│  · 矢量叠层   │   过滤后的 HostCommand & 槽位状态缓存        │  ├── BuiltinPlugin    │
│  · 底栏徽章   │                                            │  │   (guides/ab-loop) │
│  · 侧栏面板   │                                            │  └── ExternalProcess  │
│  · 命令面板   │                                            │      (stdio 子进程)    │
└──────────────┘                                            └───────────────────────┘
```

- **绝不接触实时像素**：视频与照片帧始终留在原生视频层；插件只能在视口层提交 `[0..1]` 归一化矢量图元（矩形、直线、网格、十字准星、角标），由 GPUI 透明叠层无阻塞绘制且不拦截鼠标事件。
- **内置与外部同协议**：内置官方插件（`iris.guides`、`iris.ab-loop`）和外部进程插件共享同一套 `HostEvent` / `HostCommand` / `Permission` 模型，并在 **设置 → 插件** 统一管理启停并持久化到 `settings.conf`。
- **宿主掌控视觉语法**：插件提交的是结构化数据（`SidebarPanel`、`StatusItem`、`OverlayShape`、`CommandSpec`），由 `crates/iris/src/ui/{plugin_ui,palette}.rs` 严格按方向 C 风格渲染。
- **异步防卡顿**：外部子进程由独立读线程（有界单行 64 KiB）和独立非阻塞写线程（有界队列，满载丢弃 `PositionTick`）通信，通过事件驱动 `WakeSignal` 唤醒 UI，不再空转 250ms 轮询；子进程阻塞、超长输出或崩溃均不会冻结主界面或视频渲染。

## 6. 依赖

| 依赖 | 用途 | 规则 |
|---|---|---|
| `gpui = "=0.2.2"` | UI | crates.io 版，`default-features = false, features = ["font-kit", "runtime_shaders"]`，只需 Command Line Tools。锁死版本，主动升级。不引第三方 GPUI 组件库。|
| `iris-mpv`（本仓库） | libmpv FFI | 手写声明，不用 bindgen。|
| `iris-api`（本仓库） | 插件协议 | 仅依赖 `serde`（测试用 `serde_json`），零 UI/媒体依赖。|
| `serde` / `serde_json` | 插件协议与清单序列化 | 已存在于 GPUI 依赖树中（`MIT OR Apache-2.0`），不新增非宽松许可证。|
| `image` | 跨平台照片缩略图回退解码 | 已在依赖树中，启用常见图片格式特征用于非 macOS / ImageIO 回退缩略图。|
| `objc 0.2`、`cocoa =0.26.0` | macOS 原生调用 | GPUI 0.2.2 内部也用这两个，迁到 objc2 后类型不互通，暂不迁移。|
| `futures` | 通道 | 用 GPUI 自带的 executor；Iris 自己不直接引 tokio（GPUI 的 HTTP 客户端会间接带进来，这不归 Iris 管）。|
| `raw-window-handle` | 取原生视图 | |

许可证检查用根目录的 `deny.toml`（`cargo deny check licenses`）。白名单除了常见宽松协议，还放行了 MPL-2.0（`dwrote`、`option-ext` 等，按文件生效的弱传染协议，可闭源链接）、CC0-1.0、BSL-1.0、NCSA、Unicode-3.0 和 LLVM-exception。

## 7. 调试控制管道

`crates/iris/src/debug.rs`：debug 或显式 `--features devtools` 构建中，`IRIS_CONTROL` 指向本地流端点（macOS Unix socket、Windows `\\.\pipe\iris-...`）。逐行读取 `key`、`move`、`click`、`snap`、`resize`、`open`、`filter`、`state`、`get`、`mpv`、`quit`。客户端为 `scripts/control-client.py`；回复保留在 stderr，运行日志另外走 `log`。

- 合成鼠标事件必须在 GPUI update 之外发出：macOS 发送 NSEvent，Windows 发送原生窗口消息，均经 GPUI 正常命中测试。
- `snap`：macOS 用 `CGWindowListCreateImage`；Windows 捕获窗口占据的可见桌面区域，不能用于被遮挡窗口的自动像素对比。

## 8. 安全边界

- **外部传进来的内容永远不能变成 mpv 选项或命令**（文件关联、URL scheme、拖入的路径、以及**插件消息**都一样）。教训是 IINA 的 CVE-2026-47114（GHSA-w5xh-98j7-jp5q）：`iina://` 地址里的 mpv 参数没过滤，导致命令注入。插件侧仅允许预定义强类型 `HostCommand` 且经权限白名单校验。
- **外部插件隔离**：`plugin.json` 拒绝绝对路径和 `..` 路径穿越；仅允许声明过的权限生效；stdout 单行限 64 KiB、单次轮询限 256 条消息；Windows 子进程启用 `CREATE_NO_WINDOW`。
- 任意 mpv 控制仅用于 debug／显式 `devtools` 构建，同用户访问，普通 release 不监听；不得把性能测试二进制当发行包。
- 媒体和所在目录只读；设置、日志、收藏进度（`records.tsv`）、最近打开（`recent.tsv`）与外部插件（`plugins/`）全部保存在系统应用数据目录，不往媒体目录写隐藏文件或缓存（缩略图底条仅驻留内存）。
- Windows 单实例：`Local\Iris.Player` 互斥量判断是否已有实例；后来的进程把绝对路径（UTF-16，长度前缀，上限 32767 个单位）写进 `\\.\pipe\iris-open-<会话号>-<用户 SID>` 后退出。管道和调试管道共用 `platform::pipe::create_inbound`：只建第一个实例、拒绝远程客户端、DACL 只给当前用户。客户端先用 `GetNamedPipeServerSessionId` 确认服务端在同一登录会话。管道里只有路径，进到界面后和拖入的路径走同一条 `open_path`。设置了 `IRIS_SETTINGS` 的测试实例不参加单实例。

## 9. 规划目录树与实际代码

2026-11-20 规划讨论稿提出分层结构并展望插件扩展；经过 MVP 验证、核心体验完善与控制面插件系统落地后，实际结构与规划对照如下：

| 规划稿 | 实际 | 原因 |
|---|---|---|
| `iris-core` 不依赖 GPUI | 已拆分并由 `check-project.py` 强制约束 | `cargo test -p iris-core` 无窗口运行 |
| `iris-api` crate 与控制面插件 | 已落地：`iris-api` + `iris-core/src/plugin/{mod,builtins}.rs` + `ui/{palette,plugin_ui}.rs` | 播放器体验完善后，按用户要求落地控制面与矢量叠加面插件系统，坚持不接触像素数据 |
| `engine/` 下的 `MediaBackend` trait | 仍没有；`player::Session` 直接用 `iris-mpv` | 只有 mpv 一套实现，等真有第二套原生照片/解码后端再提炼（DECISIONS 4.5） |
| `command.rs` / `state.rs` / `event.rs` 分文件 | 都在 `player.rs`（`Command`、`PlaybackState`、`SessionEvent`） | 模块边界清晰，无需过度拆分 |
| `viewport.rs`、`playlist.rs`、`config.rs` | `geometry.rs`、`library.rs`、`settings.rs` | 只是名字不同 |
| `platform/` 下按 OS 分目录，加 `VideoUnderlay` trait | `video/{mac,windows}.rs` + `platform/{mod,macos,windows}.rs`，按 `cfg` 隔离 | 两个平台的生命周期差别大，暂不定公共 trait |
| `ui/` 拆成 workspace / titlebar / toolbar / sidebar / osd | `ui.rs` + `ui/{state,actions,debug_cmd,titlebar,directory,viewport,controls,popover,context_menu,info,settings,palette,plugin_ui,widgets}.rs` | 按独立交互表面与内聚状态拆分为清晰子模块，workspace 收口在 `ui.rs` |
| `control.rs`（原 `debug.rs`） | 仍叫 `debug.rs` | 只在 debug / `devtools` 构建启用 |
