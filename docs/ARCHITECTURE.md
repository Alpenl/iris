# Iris 架构

本文记录 Iris 的分层设计、macOS / Windows 原生视频合成、色彩管线、插件隔离架构与安全边界。决策和路线图见 [DECISIONS.md](DECISIONS.md)，历次决议讨论与取舍逻辑见 [DISCUSSIONS.md](DISCUSSIONS.md)，插件开发规范见 [PLUGINS.md](PLUGINS.md)，测试方法与实测数据见 [TESTING.md](TESTING.md)。

## 1. 分层

```
crates/
  iris-mpv/   libmpv 的手写 FFI（不用 bindgen）：client API + mpv_node 结构化传参 + RenderHandle 窄能力封装
  iris-api/   无头插件协议（protocol: 1）：虚拟数据源（含动态多实例）、参数化 UI 工作台、视口叠层与线协议（仅依赖 serde）
  iris-core/  无头内核：会话、统一目录与独立队列、稀疏工作区预设、键位引擎、照片探测、插件运行时与通用网络/凭据底座；不许依赖 GPUI
  iris/       GPUI 应用：窗口、ThemeTokens 动态令牌、ui/kit 共用原语、平台视频层与系统桥接、单实例、调试管道
```

依赖方向严格单向：`iris` → `iris-core` → `iris-mpv`，且 `iris` / `iris-core` → `iris-api`（`iris` 仅在 `src/video/` 中使用窄能力 `iris_mpv::RenderHandle`，业务与 UI 代码显式引用 `iris_core::*`）。`scripts/check-project.py` 自动检查：

1. `iris-core` 不依赖 GPUI 或原生窗口/图像库；
2. `iris-api` 常规依赖仅限 `serde`（不依赖 GPUI 或 `iris-mpv`）；
3. `crates/iris/src/` 除 `video/` 外递归禁止出现 `iris_mpv` 或原始 mpv 调用；
4. `devtools` 控制管道门禁未被默认特性或普通发布版绕过；
5. 全仓 `.rs` 模块行数预算（超过 650 行生产代码提示，生产代码上限 850 行、总行数上限 1,000 行），以及图标与示例插件清单一致性。

### 核心模块职责

- `crates/iris-mpv/src/{lib,render,sys}.rs`：手写 C 声明（`sys` 与 `Mpv::raw` 均为 `pub(crate)`）与安全薄封装。短属性名通过栈缓冲避免热路径堆分配。`Mpv::loadfile_with_options` 使用结构化节点传递单文件选项，包括同源字幕，不拼接 mpv 命令字符串。渲染层仅暴露 OpenGL 与 D3D11 共用的 `RenderHandle`。
- `crates/iris-mpv/build.rs`：优先使用 `MPV_LIB_DIR`。Windows x64 MSVC 开发构建回退到工作区 `target/deps/mpv/mpv.lib`；macOS 按目标架构查找对应 Homebrew 目录。始终动态链接。
- `crates/iris-api/src/{lib,manifest,source,workbench,overlay,wire}.rs`：单版本正交插件协议（`PROTOCOL_VERSION = 1`），定义 `PluginManifest`（拒绝绝对路径、盘符与 `..` 穿越，强校验协议版本与权限）、`SourceProviderSpec`/`SourceDescriptor`（支持动态多实例 `SetSources` 与连接管理 `SourceManage`）、`PlaybackSnapshot`、`StreamSpec`、统一声明式控件树 `Widget`（13 种）、`OverlayNode`（7 种）、`UiThemeSpec`/`ChromeLayoutSpec`（保留全量高自由度字段，大命令变体内部 `Box` 化瘦身），以及带异步回调通道 `PluginResponder` 的 `BuiltinPlugin` trait。
- `crates/iris-core/src/player/`：`Session` 持有播放状态、本地目录和规范化 `MediaUri`。`relay.rs` 为所有 HTTP(S) 播放提供会话级转发和取消，按源发送凭据。独立授权字幕经有界预取挂载。`player::prepare` 在线程池上解析本地路径和目录。
- `crates/iris-core/src/{media_uri,catalog,store,vault,preset}.rs`：
  - `media_uri.rs`：三元正交寻址 `MediaUri`（`Local` | `HttpUrl` | `Plugin`，自动无损升级历史 `webdav://` URI）。
  - `catalog.rs`：跨本地、插件源与收藏统一的 `CatalogItem` 排序/过滤纯函数，以及与当前浏览文件夹彻底解耦的独立播放队列 `PlayQueue`。
  - `store.rs`：以稳定媒体身份保存收藏、续播和最近记录。写入时取得文件锁，重读磁盘、合并修改字段并原子替换。写入失败保留待保存变更，旧记录在迁移成功后退役。
  - `vault.rs`：系统安全凭据库 `CredentialVault`（macOS Keychain / Windows Credential Manager，单条上限 2048B，失败显式报错）。按 `builtin.<id>` 与 `ext.<id>` 隔离。`credential_index.json` 只保存作用域与键名，测试可用 `in_memory()`。
  - `preset.rs`：`1B+` 稀疏工作区预设（`Option<T>` 补丁 + 显式插件启停差量与退出指令），`settings.conf` 始终只存纯净个人基线，运行期通过内存 `PresetRevert` 自动回正。
- `crates/iris-core/src/plugin/`（`mod,types,process,slots,snapshot,builtins,webdav/`）：`PluginManager` 统一托管内置插件（`iris.guides`、`iris.ab-loop` 以及 100% 走标准协议的官方免挂载 `iris.webdav`，保留 `iris.*` 命名空间并通过 `PluginResponder` 后台非阻塞执行网络 `PROPFIND`）、零进程声明式主题插件与外部 JSON-lines stdio 子进程插件（单行上限 2 MiB，`stderr` 超 100 行静默 drain 不断管），通过 `DirtySlots` 值相等脏检测守住全屏 `0 Hz` 空转防线。
- `crates/iris-core/src/photo/{mod,containers,exif,icc}.rs`：魔数优先单遍照片头解析（JPEG/PNG/WebP/HEIF/TIFF 尺寸、方向、EXIF 摘要与内嵌 ICC 原色匹配）。
- `crates/iris-core/src/{keymap/,vfs/,input,geometry,library,settings,logging,fs_util}.rs`：三方案键位目录与文本输入保护、常驻位置书签与有界 HTTP 客户端、指针手势策略、几何变换与慢速共享盘优化扫描。
- `crates/iris/src/theme.rs` + `ui/`：GPUI `Global` `ThemeTokens` 引擎（方向 C 默认 + 插件圆角/栏高安全 clamp 与最低对比度兜底）；窗口仅保留 `IrisWindow` 为根协调器 `Entity`，收敛为粗粒度领域状态块（单会话字段全部归位进 `Viewport`），彻底移除子模块通配 `use super::*;`：
  - `ui/kit/{mod,text_buffer,text_field,form,list_row,modal}.rs`：共用 Direction C 基础控件库——全仓唯一单行 IME 输入框 `TextInputHub` + `render_text_field`、通用表单控件、统一媒体/目录行与居中模态框外壳，悬停提示动态查询当前 `Keymap`。
  - `ui/{mod,events,vfs_ctrl,plugin_ctrl,preset_ctrl,chrome,actions*,viewport*,titlebar,info,palette,context_menu,dialogs,debug_cmd}.rs`：核心窗口装配、`hide_titlebar` 三重安全护栏（顶缘 `absolute()` 悬浮唤醒标题栏、视口修饰键/阈值拖拽、`ResetAppearance` 逃生动作）、异步远程源拉取与命令面板。
  - `ui/{sidebar,controls,popover,plugin_ui,settings}/`：四模式统一侧栏（`CatalogItem` 单一渲染分支 + 唯一数据源管理页 + 独立队列）、响应式底栏与 60fps 时间轴、锚定浮层、插件 13 类声明式控件/非破坏性校验弹窗/7 类视口叠层，以及 7 标签设置中心。
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
- advanced control 模式下，渲染线程在窗口不可绘制时仍服务 mpv 更新。Windows 和 macOS 恢复显示时会重新绘制当前帧。
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

## 5. 全插件化远程数据源、通用网络底座与插件隔离架构

远程存储与插件系统坚持**“像素管线内核独占 + 本地仅留通用网络/凭据底座 + 所有远程协议皆插件”**边界，详细协议见 [PLUGINS.md](PLUGINS.md)：

```
                    HostEvent (JSON-lines stdin / PluginResponder, protocol = 1)
┌──────────────┐ ──────────────────────────────────────────────────▶ ┌─────────────────────────┐
│  Iris UI     │                                                     │  PluginManager          │
│  · ThemeTokens│ ◀────────────────────────────────────────────────── │  (iris-core)            │
│  · 无边框护栏  │  DirtySlots 门控后的 HostCommand (主题/多源/弹窗/流)  │  ├── iris.webdav (内置) │
│  · ui/kit 原语│                                                     │  ├── iris.guides/ab-loop│
│  · 统一侧栏    │  结构化 loadfile 命令                              │  ├── DeclarativeTheme   │
└──────────────┘ ───▶ libmpv（HTTP(S) 流经会话专属转发器）                  │ └── Vault (builtin/ext)│
                      └── 远程图片按预算探测（photo::probe_remote）       └─────────────────────────┘
```

- **绝不接触实时像素，保留高自由度控制面定制**：视频与照片帧始终留在原生视频层；本体默认遵循方向 C 模块面板，同时允许插件通过 `UiTheme` / `ChromeLayoutSpec` 自由定制色板、圆角/栏高度量与 `hide_titlebar` 无标题栏影院窗（宿主强制提供顶缘 `absolute()` 悬浮唤醒标题栏、视口修饰键/阈值拖拽与最低对比度逃生护栏）。
- **播放传输**：每个 HTTP(S) 播放会话创建一个仅绑定回环地址的转发器。HLS 对带参数的扩展标签使用白名单，要求属性列表使用明确的逗号和引号语法，并把 URI 改写为会话专属路由。转发器按源筛选请求头，VFS 在每次连接前检查解析后的地址，局域网或高端口回环源的访问范围限于原始源。中继满载时短暂等待工作槽释放，超时才返回 `503`。宿主单独预取授权字幕，普通媒体和字幕禁用外部引用。
- **全协议插件化与分桶凭据托管（`SetState` + `CredentialVault`）**：内核与 UI 零具体网盘/WebDAV 特判（官方 `iris.webdav` 100% 按标准 `iris-api` 协议实现，经 `PluginResponder` 在后台工作线程执行 `PROPFIND`）。敏感凭据按 `builtin.<id>` 与 `ext.<id>` 物理分桶存入系统安全凭据库（单条上限 2048B，失败显式报错，严禁降级写明文文件）。
- **异步防卡顿与 `DirtySlots` 全屏 `0 Hz` 门控**：外部子进程单行上限提升至 2 MiB，`stderr` 超 100 行后静默 drain 不断管；仅在槽位实际发生值变化且对当前视图可见时才触发 UI 重绘。

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

`deny.toml` 约束 RustSec 公告、许可证、依赖选型和来源。C 媒体库配方另外检查锁定源码的 HTTPS 来源、摘要和 SPDX 许可。

## 7. 调试控制管道

`crates/iris/src/debug.rs`：debug 或显式 `--features devtools` 构建中，`IRIS_CONTROL` 指向本地流端点（macOS Unix socket、Windows `\\.\pipe\iris-...`）。逐行读取 `key`、`move`、`click`、`snap`、`resize`、`open`、`filter`、`state`、`get`、`mpv`、`quit`。客户端为 `scripts/control-client.py`；回复保留在 stderr，运行日志另外走 `log`。

- 合成鼠标事件必须在 GPUI update 之外发出：macOS 发送 NSEvent，Windows 发送原生窗口消息，均经 GPUI 正常命中测试。
- `snap`：macOS 用 `CGWindowListCreateImage`；Windows 捕获窗口占据的可见桌面区域，不能用于被遮挡窗口的自动像素对比。

## 8. 安全边界

- 外部文件路径和网络地址以数据参数传给 mpv。插件只能发送预定义的 `HostCommand`，由宿主校验权限。`RenderHandle` 限制 UI 能调用的渲染接口。
- **外部插件与凭据隔离**：`plugin.json` 拒绝绝对路径、`..` 穿越与保留前缀 `iris.*`；凭据库按 `builtin.<id>` 与 `ext.<id>` 物理分桶且严禁明文文件降级；stdout 单行限 2 MiB、单次轮询限 256 条消息，`stderr` 超限后静默排空防 `BrokenPipeError`；Windows 子进程启用 `CREATE_NO_WINDOW`。
- 任意 mpv 控制仅用于 debug／显式 `devtools` 构建，同用户访问，普通 release 不监听；不得把性能测试二进制当发行包。
- 媒体和所在目录只读；设置、日志、收藏进度（`records.tsv`）、最近打开（`recent.tsv`）与外部插件（`plugins/`）全部保存在系统应用数据目录，不往媒体目录写隐藏文件或缓存（缩略图底条仅驻留内存）。
- Windows 用 `Local\Iris.Player.Batch.<用户 SID>` 互斥量识别当前登录会话的播放器。后启动的进程通过同用户双向管道 `\\.\pipe\iris-open-batch-v2-<会话号>-<用户 SID>` 发送绝对路径并授予前台窗口权限。消息使用有界 UTF-16LE 路径批次，两端核对会话号，主窗口接收后确认。连接、读写或确认超时后，后启动的进程以失败状态退出。`IRIS_SETTINGS` 实例独立运行。
- 命令行参数、Finder 的本地文件 URL、文件选择器和视口拖放都把一次选择交给 `open_paths`。单个媒体沿用同目录浏览。多个媒体按选择顺序排队，所选目录在队列中展开一层，同一文件保留首次出现的位置。混选的外挂字幕等待本次媒体加载后依次挂载，单独选中的字幕挂载到正在播放或正在打开的音视频。切换媒体会使上一批待挂载字幕失效。

## 9. 规划目录树与实际代码

2026-11-20 规划讨论稿提出分层结构并展望插件扩展；经过 MVP 验证、核心体验完善与控制面插件系统落地后，实际结构与规划对照如下：

| 规划稿 | 实际 | 原因 |
|---|---|---|
| `iris-core` 不依赖 GPUI | 已拆分并由 `check-project.py` 强制约束 | `cargo test -p iris-core` 无窗口运行 |
| `iris-api` crate 与控制面插件 | 已落地：`iris-api/src/{lib,manifest,source,workbench,overlay,wire}.rs` + `iris-core/src/plugin/` + `ui/{palette.rs,plugin_ui/}` | 坚持不接触像素数据，同时保留高自由度 UI/数据源扩展并将官方 WebDAV 统一纳入 `iris.webdav` 插件 |
| `engine/` 下的 `MediaBackend` trait | 仍没有；`player::Session` 直接用 `iris-mpv` | 只有 mpv 一套实现，等真有第二套原生照片/解码后端再提炼（DECISIONS 4.5） |
| `command.rs` / `state.rs` / `event.rs` 分文件 | `player/{mod,model,prepare,backend,inspect,relay}.rs` | 按会话、模型、准备、后端、检查与转发拆分 |
| `viewport.rs`、`playlist.rs`、`config.rs` | `geometry.rs`、`library.rs` + `catalog.rs`、`settings.rs` | `catalog.rs` 统一跨源条目与独立 `PlayQueue` |
| `platform/` 下按 OS 分目录，加 `VideoUnderlay` trait | `video/{mac,windows}.rs` + `platform/{mod,macos/,windows.rs}`，按 `cfg` 隔离 | 两个平台的生命周期差别大，暂不定公共 trait |
| `ui/` 拆成 workspace / titlebar / toolbar / sidebar / osd | `ui/{mod,events,vfs_ctrl,plugin_ctrl,preset_ctrl,chrome,actions*,viewport*,titlebar,info,palette,context_menu,dialogs,debug_cmd}.rs` + `ui/{kit,sidebar,controls,popover,plugin_ui,settings}/` | `ui/kit/` 提供共用原语，`check-project.py` 统一检查文件行数预算 |
| `control.rs`（原 `debug.rs`） | 仍叫 `debug.rs` | 只在 debug / `devtools` 构建启用 |
