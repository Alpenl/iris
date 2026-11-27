# Iris 架构

本文记录 Iris 的分层设计、macOS / Windows 原生视频合成、色彩管线、插件隔离架构与安全边界。决策和路线图见 [DECISIONS.md](DECISIONS.md)，历次决议讨论与取舍逻辑见 [DISCUSSIONS.md](DISCUSSIONS.md)，插件开发规范见 [PLUGINS.md](PLUGINS.md)，测试方法与实测数据见 [TESTING.md](TESTING.md)。

## 1. 分层

```
crates/
  iris-mpv/   libmpv 的手写 FFI（不用 bindgen）：client API + mpv_node 结构化传参 + RenderHandle 窄能力封装
  iris-api/   无头插件协议（protocol: 2 / ABI: 2）：虚拟数据源（含动态多实例）、参数化 UI 工作台、视口叠层与线协议（仅依赖 serde）
  iris-core/  无头内核：会话、统一目录与独立队列、稀疏工作区预设、键位引擎、照片探测、插件运行时与通用网络/凭据底座；不许依赖 GPUI
  iris/       GPUI 应用：窗口、ThemeTokens 动态令牌、ui/kit 共用原语、平台视频层与系统桥接、单实例、调试管道
```

依赖方向严格单向：`iris` → `iris-core` → `iris-mpv`，且 `iris` / `iris-core` → `iris-api`；`iris-plugin-sdk` 仅由 Wasm Guest 插件引用，宿主零反向依赖（`iris` 仅在 `src/video/` 中使用窄能力 `iris_mpv::RenderHandle`，业务与 UI 代码显式引用 `iris_core::*`）。`scripts/check-project.py` 自动检查：

1. `iris-core` 不依赖 GPUI、`iris-plugin-sdk` 或原生窗口/图像库；
2. `iris-api` 常规依赖仅限 `serde`（不依赖 GPUI 或 `iris-mpv`）；
3. `crates/iris/src/` 除 `video/` 外递归禁止出现 `iris_mpv` 或原始 mpv 调用，且 `crates/iris/src/` 与 `crates/iris-core/src/` 全量非测试代码禁止 `use super::*;`；
4. `crates/iris-core/src/` 中 18 个核心域模块（含其单元测试）禁止反向依赖 `plugin_v2` 或 `iris_api`；
5. 全仓生产代码禁止直接调用 `std::thread::spawn` 或未登记的 `std::thread::Builder`，所有 OS 线程必须通过 `iris_core::concurrency::{spawn_thread, spawn_thread_named}` 按 `ThreadRole` 登记，且每个角色的线程名必须记录在本文第 6 节；
6. `devtools` 控制管道门禁未被默认特性或普通发布版绕过；
7. 全仓 `.rs` 模块行数预算（超过 650 行生产代码提示，生产代码上限 850 行、总行数上限 1,000 行），以及图标、官方嵌入插件摘要与示例插件清单一致性。

### 核心模块职责

- `crates/iris-mpv/src/{lib,render,sys}.rs`：手写 C 声明（`sys` 与 `Mpv::raw` 均为 `pub(crate)`）与安全薄封装。短属性名通过栈缓冲避免热路径堆分配。`Mpv::loadfile_with_options` 使用结构化节点传递单文件选项，包括同源字幕，不拼接 mpv 命令字符串。渲染层仅暴露 OpenGL 与 D3D11 共用的 `RenderHandle`。
- `crates/iris-mpv/build.rs`：优先使用 `MPV_LIB_DIR`。Windows x64 MSVC 开发构建回退到工作区 `target/deps/mpv/mpv.lib`；macOS 按目标架构查找对应 Homebrew 目录。始终动态链接。
- `crates/iris-api/src/v2/`：protocol/ABI 2 的 manifest、source（含通用 `badge`）、stream（`DirectHttp` / `RandomAccess` / `VirtualHls`）、task、wire（含 `HostCommand::Log`）、enhance 与密码学契约；公共 `Widget`（含 `PixelGrid`）、`ViewTimer`、`OverlayNode`、`UiThemeSpec` 位于 crate 根，常规依赖仅 serde，零中文 UI 文案。
- `crates/iris-core/src/concurrency.rs`：全仓统一的取消令牌 `CancelToken`（无锁原子轮询快路径 + 嵌套子令牌与跨对象 `link` + TCP `shutdown(Both)` / `mio::Waker` / `Condvar` 唤醒回调）、单调截止时间 `Deadline` 与受管 OS 线程注册表（`ThreadRole`、`budget`、`spawn_thread` / `spawn_thread_named`，见第 6 节）。
- `crates/iris-core/src/loopback_http.rs`：共用本地回环 HTTP 监听、Range 响应与自适应清单探测原语，由原生播放转发器与插件流中继统一复用。
- `crates/iris-core/src/player/`：`Session` 持有播放状态、结构化 `MediaSourceState` / `PluginEnhancements` / `VisualTransform` 状态块、规范化 `MediaUri` 及语义化 `PlayerActionError`。`relay.rs` 为所有 HTTP(S) 播放提供会话级转发和取消，按源发送凭据。`player::scoped` 通过 trait `ScopedStream` 与 `MonitorLutFilter` 与 `plugin_v2` 彻底解耦。
- `crates/iris-core/src/{media_uri,catalog,store,vault,preset}.rs`：
  - `media_uri.rs`：三元正交寻址 `MediaUri`（`Local` | `HttpUrl` | `Plugin`，经 `MediaUri::parse_persisted` 无损升级历史 `webdav://` URI）。
  - `catalog.rs`：跨本地、插件源与收藏统一的 `CatalogItem` 排序/过滤纯函数，以及与当前浏览文件夹彻底解耦的独立播放队列 `PlayQueue`。
  - `store.rs`：以稳定媒体身份保存收藏、续播和最近记录。写入时取得文件锁，重读磁盘、合并修改字段并原子替换。写入失败保留待保存变更，旧记录在迁移成功后退役。
  - `vault.rs`：系统安全凭据库 `CredentialVault`（macOS Keychain / Windows Credential Manager，单条上限 2048B，失败显式报错）。v2 插件按安装身份 `plugin-v2.<installation_id>` 隔离（兼容读取迁移期 `builtin.<id>` 与 `ext.<id>`），Windows 下采用 `app.iris.vault.v2:` 小写十六进制编码目标名以消除大小写不敏感碰撞并保护作用域/键名边界。`credential_index.json` 只保存作用域与键名，测试可用 `in_memory()`。
  - `preset.rs` + `preset/runtime.rs`：`1B+` 稀疏工作区预设（`Option<T>` 补丁 + 显式插件启停差量与退出指令），`settings.conf` 始终只存纯净个人基线，运行期通过内存 `PresetRevert` 自动回正（全局字号 `ui_font_scale` 属个人偏好不进预设）。
- `crates/iris-core/src/plugin_v2/`：有界 Wasmtime 运行时（Pulley64 字节码解释器）、安装/能力授权、网络、三模流（`DirectHttp` / `RandomAccess` / `VirtualHls`）、轻量文件产物与哈希（`FileExecutor`）、媒体增强与共享 Hub；`iris-plugin-sdk` 和两个官方插件 crate 是同一套公开 Guest 入口。
- `crates/iris-core/src/photo/{mod,containers,heif,exif,icc}.rs`：魔数优先单遍照片头解析（JPEG/PNG/WebP/HEIF/TIFF 尺寸、方向、EXIF 摘要与内嵌 ICC 原色匹配）。
- `crates/iris-core/src/{keymap/,vfs/,logging/,input,geometry,library,settings,fs_util}.rs`：三方案键位目录与结构化 `ConflictTarget`、常驻位置书签与有界 HTTP 客户端、单一集中脱敏日志与结构化事件流（`iris.log` + `events.jsonl`）、指针手势策略、几何变换与慢速共享盘优化扫描。
- `crates/iris/src/theme.rs` + `ui/`：GPUI `Global` `ThemeTokens` 引擎（方向 C 默认 + 扁平 `text_xs/sm/md/lg` / `control_h_sm/md` / `danger` / `ui_font_scale` `0.85..=1.35` 字号缩放与防裁切下限 + 插件圆角/栏高安全 clamp 与最低对比度兜底）；`#root` 采用统一 5 层 Z-Order 渲染栈与唯一 `Esc` 逐层退出管线：
  - `ui/kit/{mod,text_buffer,text_field,form,list_row,modal}.rs`：共用 Direction C 基础控件库——全仓唯一单行 IME 输入框 `TextInputHub` + `render_text_field`、4 变体×3 尺寸 `button` / `icon_button_disabled`、`select_row`、`panel_header` / `surface_card` / `section_label`、`tab_strip` 与居中 `modal_shell`（支持内联错误区与提交禁用）。
  - `ui/strings/{mod,actions,presets,errors,chrome}.rs`：按领域归集的共享枚举、菜单、错误、预设与通用操作中文文案表，彻底剥离 `iris-core` 与 `iris-api` 中的硬编码中文 UI 文本（其余视图局部文案保留在各 UI 模块内）。
  - `ui/{mod,events,vfs_ctrl,plugin_ctrl,preset_ctrl,chrome,actions*,viewport*,titlebar,info,palette,context_menu,dialogs,debug_cmd}.rs`：核心窗口装配、`hide_titlebar` 三重安全护栏、3 秒两步内联危险操作确认、异步远程源拉取与命令面板。
  - `ui/{sidebar,controls,popover,plugin_ui,settings}/`：四模式统一侧栏（零硬编码插件特判，统一读取 `SourceDescriptor.badge` / `icon`）、响应式底栏与 60fps 时间轴、锚定浮层、插件声明式控件（含正交 `Widget::PixelGrid` + `ViewTimer`）/非破坏性校验弹窗/视口叠层，以及 7 标签设置中心。
- `crates/iris/src/instance.rs`：Windows 单实例，见第 9 节。macOS 由系统把打开请求发给已运行进程，经 GPUI `on_open_urls` 进来，只接受 `file://`。
- `crates/iris/src/platform/{mod.rs,macos/{mod,imageio}.rs,windows.rs}`：按 OS 拆分的原生系统桥接——自实现窗口拖动（GPUI 0.2.2 `start_window_move()` 在 macOS/Windows 上为空函数）、窗口置顶（macOS `NSFloatingWindowLevel` / Win32 `SetWindowPos`）、播放期防休眠 `SleepPreventer`（macOS `IOKit` / Win32 `SetThreadExecutionState`）、macOS 触控板双指捏合 `PinchMonitor`（持有 `NSView` 强引用避免 UAF）、macOS 媒体键与控制中心桥接 `MediaKeyBridge`（`MediaPlayer.framework`，同步真实倍速与清空状态）、默认截图目录，以及照片内存缩略图解码 `decode_thumbnail`（macOS `ImageIO` 原生 sRGB 缩放并反预乘为直通 BGRA、Windows 原生 WIC 缩放 + EXIF 方向校正，跨平台 `image` crate 带内存/尺寸上限回退）。
- `crates/iris/src/debug.rs`：调试控制管道（`IRIS_CONTROL`），见第 8 节。

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
| 照片内嵌 ICC | 已完成 | mpv 的 libmpv OpenGL 路径忽略内嵌 ICC。`crates/iris-core/src/photo/` 只读文件头，从 JPEG / PNG / WebP / HEIC / AVIF / TIFF 取出配置文件（HEIC 也认 nclx），由 `icc.rs` 按 D50 原色坐标和曲线匹配 sRGB、Display P3、Adobe RGB、BT.2020、ProPhoto；`player/` 的 `prepare` / `load` 把结果作为 `loadfile` 单文件选项（`vf` 里 mpv 自带的 `format` 滤镜带上 `primaries` / `gamma`，外加 `gamut-mapping-mode=clip`）传给 mpv，换文件时自动撤销。照片超出 P3 的颜色直接裁剪，和 ColorSync 一致（mpv 默认的压缩会让 Adobe RGB 纯绿偏青）；视频保留 mpv 默认的映射。识别不了的配置文件仍按 sRGB。|
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

## 5. Wasm 插件、授权网络与媒体管道

`iris-api::v2` 定义包和控制消息，`iris-plugin-sdk` 提供 Guest ABI；`iris-core::plugin_v2` 负责 JIT、安装、资源账本、网络与流。窗口共用后台 Hub，每安装身份一个 ControlStore，每路 Guest 流独立 StreamStore。所有耗时 I/O 返回请求 ID 后在后台完成，UI 不等待线程退出。

官方 `iris.guides` 与 `iris.webdav` 使用公开 SDK 编译成 Wasm 包，构建时嵌入并校验摘要，遵守同样的授权与配额。A-B 循环归原生 Session；旧 BuiltinPlugin/stdio/Python 插件运行时已移除。用户包经设置中的宿主审核安装，更新增权不静默通过。

网络将传输能力与目的地范围分开，全链路检查初始 URL、重定向和实际连接地址。任意公网权限仍拒绝私网；NAS 使用用户确认的具体 Origin 与固定地址集合。播放、字幕与缩略图复用所属插件授权，跨源剥离敏感头。

DirectHttp、RandomAccess、VirtualManifest 共用受控播放准入。远端及虚拟 HLS 经结构化 `VirtualHls` 解析并生成本会话密封回环引用，拒绝未知必要语义且不向 libmpv 回退原始清单（零调用方的 DASH 与受限 HLS 文本通道已在 `0.0.2` 移除）。绑定资源持续到异步 mpv 加载确认和实际 I/O 结束，取消中的资源仍占额度。

主题、侧栏、弹窗（含正交 `Widget::PixelGrid` 与 `ViewTimer`）、时间轴、叠层使用受限声明式数据，插件可通过 `HostCommand::Log` 写入经宿主 20 条/秒限流与脱敏的结构化事件流；标记写应用数据库。零调用方的外部原生工具（BYOB）与文件导出模块已在 `0.0.2` 裁剪，当前仅保留受控私有产物读写与 `HashCurrentMedia`。详细权限/ABI/生命周期/配额见 [插件规范](PLUGIN_V2_ARCHITECTURE.md)，开发入口见 [插件指南](PLUGINS.md)，验证范围见 [实施证据](PLUGIN_V2_IMPLEMENTATION.md)。

## 6. 并发模型、线程清单与预算

所有生产环境的 OS 线程均在 `crates/iris-core/src/concurrency.rs` 中由 `ThreadRole` 集中登记，并通过 `spawn_thread` / `spawn_thread_named` 创建。`scripts/check-project.py` 在静态检查阶段禁止 `concurrency.rs` 以外的生产代码直接调用 `std::thread::spawn` 或 `std::thread::Builder`，并校验下表完整覆盖全部 18 类受管线程：

| `ThreadRole` | 线程名 / 前缀 | 局部上限 | 全局上限 | 超限与背压策略 | 生命周期与回收所有权 |
|---|---|---|---|---|---|
| `ControlPipe` | `iris-control` | 1 | 1 | `WouldBlock` 拒绝重复监听 | 仅 debug / `devtools` 启动；进程退出结束 |
| `SingleInstanceIpc` | `iris-open-files` | 1 | 1 | `WouldBlock` 拒绝重复监听 | Windows 主实例持有管道服务端，进程退出结束 |
| `VideoSurface` | `iris-video` | 1 / 视口 | 视口数 | 每视口单实例构造 | 视口关闭时先释放 render context 并同步 `join` |
| `MpvEventPump` | `iris-mpv-events` | 1 / `Session` | `Session` 数 | 每会话单实例构造 | `EventPump::stop` 发送 `quit` / `CancelToken` 并同步 `join` |
| `MediaRelayListener` | `iris-media-relay` | 1 / `Service` | 8 | `WouldBlock` 快速失败 | `Service::drop` 触发 `CancelToken` + `mio::Waker`，`iris-relay-reap` 回收 |
| `MediaRelayReaper` | `iris-relay-reap` | 1 / `Service::drop` | 8 | 超限时回退同步 `join` | 短生命周期回收线程，负责非阻塞释放监听与在途请求线程 |
| `MediaRelayWorker` | `iris-media-request` | 8 / 中继 | 32 | 超限或起线程失败立即回 `503 Busy` | `CancelToken` 触发 `TcpStream::shutdown(Both)`，由 `MediaRelayReaper` `join` |
| `SystemDnsWorker` | `iris-http-dns` | 16 并发查询 | 64 | `ACTIVE_DNS_LOOKUPS` 超限拒绝新建 | 受 `Deadline` 约束，解析完成后自动退出并归还信号量 |
| `WasmEpochTimer` | `iris-wasm-epoch` | 1 / `RuntimeEngine` | `Engine` 数 | 起线程失败回滚活跃计数并返回 `Internal` | 无活跃 Guest 调用时 `Condvar` 挂起，`Engine` 析构时唤醒退出 |
| `ArtifactCleanup` | `iris-artifact-cleanup` | 1 | 1 | `WouldBlock` 跳过重复后台清理 | 启动期一次性扫描清理残留临时目录后立即退出 |
| `PluginCoordinator` | `iris-plugin-coordinator` | 1 / `Hub` | `Hub` 数 | 起线程失败 `Hub::new` 返回 `Internal` | 最后一个 `Hub` 连接释放后置 `shutdown`，待在途作业与退役实例归零后退出 |
| `PluginStoreActor` | `iris-v2-` (`Control` / `Stream`) | Control 1/插件、Stream 2/插件 | Control 64、Stream 8 | `Ledger` 预留失败返回 `QuotaExceeded` | `Actor::stop` 触发 `CancelToken` 与唤醒，`is_quiescent` 确认 `done && outstanding == 0` |
| `PluginPoolWorker` | `iris-v2-` (`{control,stream,package,setup}-io-{id}`) | 按宿主分片公平轮转 | 4 + 8 + 2 + 2 = 16 | 队列满返回 `Backpressure`；空闲 30s 自动缩容 | 按需懒启动，`Dispatcher::drop` 唤醒全部工作线程退出 |
| `PluginAuthCallback` | `iris-v2-auth` | 1 / 插件 | 4 | `Ledger` `Resource::Auth` 超限拒绝 | 单次 OAuth 回调或 `Deadline` 到期/`CancelToken` 取消后退出，`Activity` 追踪归零 |
| `PluginOriginDns` | `iris-origin-dns` | 6 / 插件 | 16 | `Ledger` `Resource::Dns` 超限拒绝 | 10s `Deadline` 与 `CancelToken` 双重约束，持槽至底层解析返回后释放 |
| `PluginSocketPump` | `iris-v2-socket` | 每类 4 / 插件 | 每类 16（WS + TCP 共 32） | `Ledger` 超限拒绝；读写槽单路互斥 | `Socket::close` / `NetworkGrant` 吊销触发 `CancelToken` + `mio::Waker`，`Activity` 归零 |
| `PluginRelayListener` | `iris-v2-relay` | 4 / 插件 | 12 | `Ledger` `Resource::Relay` 超限拒绝 | `Relay::stop` 取消会话并唤醒 `mio::Poll`，`Relay::is_quiescent` 确认强引用归零 |
| `PluginRelayWorker` | `iris-v2-range` | 8 / 中继 | 32 | 超限或起线程失败立即回 `503 Busy` | 子 `CancelToken` 绑定回环 `TcpStream`，Range 结束或取消后递减活跃计数退出 |

### 统一取消与截止时间原语

- **`CancelToken`**：内部采用 `AtomicBool`（`Acquire` 无锁轮询快路径，供 Wasm 宿主导入门禁、事件泵与紧循环零锁检查）配合受互斥锁保护的唤醒表（已注册 `TcpStream` 的 `shutdown(Shutdown::Both)`、`mio::Waker`、`Condvar` 唤醒回调及弱引用子令牌树 `child()` / `link()`）。取消动作在锁外触发回调与级联子令牌，消除锁反转风险；`vfs::HttpCancel` 即 `CancelToken` 的类型别名。
- **`Deadline`**：基于单调时钟 `Instant` 的统一截止时间封装，提供 `remaining()` / `remaining_capped(limit)` 与 `CancelToken::check(deadline)` / `remaining_timeout(deadline, limit)`，统一驱动 HTTP 连接/读写、DoH 回退、OAuth 回环监听、Origin DNS 审查、WebSocket/TCP 泵、串行存储票据与 Wasm Guest 执行超时判定。

## 7. 依赖

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

## 8. 调试控制管道

`crates/iris/src/debug.rs`：debug 或显式 `--features devtools` 构建中，`IRIS_CONTROL` 指向本地流端点（macOS Unix socket、Windows `\\.\pipe\iris-...`）。逐行读取 `key`、`move`、`click`、`snap`、`resize`、`open`、`filter`、`state`、`ui`、`get`、`mpv`、`sim_device_reset`、`quit`。客户端为 `scripts/control-client.py`；回复保留在 stderr，运行日志另外走 `log`。

- 合成鼠标事件必须在 GPUI update 之外发出：macOS 发送 NSEvent，Windows 发送原生窗口消息，均经 GPUI 正常命中测试。
- `snap`：macOS 用 `CGWindowListCreateImage`；Windows 捕获窗口占据的可见桌面区域，不能用于被遮挡窗口的自动像素对比。

## 9. 安全边界

- 外部文件路径和网络地址以数据参数传给 mpv。插件只能发送预定义的 `HostCommand`，由宿主校验权限。`RenderHandle` 限制 UI 能调用的渲染接口。
- **插件边界**：包、Guest 输出和网络输入均校验；凭据按安装身份分桶，网络和文件能力逐请求检查，取消中资源持续计账，插件日志经宿主限流与统一脱敏。
- 任意 mpv 控制仅用于 debug／显式 `devtools` 构建，同用户访问，普通 release 不监听；不得把性能测试二进制当发行包。
- 媒体和所在目录只读；设置、日志、收藏进度（`records.tsv`）、最近打开（`recent.tsv`）与外部插件（`plugins-v2/`）全部保存在系统应用数据目录，不往媒体目录写隐藏文件或缓存（缩略图底条仅驻留内存）。
- Windows 用 `Local\Iris.Player.Batch.<用户 SID>` 互斥量识别当前登录会话的播放器。后启动的进程通过同用户双向管道 `\\.\pipe\iris-open-batch-v2-<会话号>-<用户 SID>` 发送绝对路径并授予前台窗口权限。消息使用有界 UTF-16LE 路径批次，两端核对会话号，主窗口接收后确认。连接、读写或确认超时后，后启动的进程以失败状态退出。`IRIS_SETTINGS` 实例独立运行。
- 命令行参数、Finder 的本地文件 URL、文件选择器和视口拖放都把一次选择交给 `open_paths`。单个媒体沿用同目录浏览。多个媒体按选择顺序排队，所选目录在队列中展开一层，同一文件保留首次出现的位置。混选的外挂字幕等待本次媒体加载后依次挂载，单独选中的字幕挂载到正在播放或正在打开的音视频。切换媒体会使上一批待挂载字幕失效。

## 10. 规划目录树与实际代码

2026-11-20 规划讨论稿提出分层结构并展望插件扩展；经过 MVP 验证、核心体验完善与控制面插件系统落地后，实际结构与规划对照如下：

| 规划稿 | 实际 | 原因 |
|---|---|---|
| `iris-core` 不依赖 GPUI | 已拆分并由 `check-project.py` 强制约束 | `cargo test -p iris-core` 无窗口运行 |
| `iris-api` crate 与控制面插件 | 已落地：`iris-api/src/v2/` + `iris-core/src/plugin_v2/` + `ui/{palette.rs,plugin_ui/}` | 坚持不接触像素数据，同时保留高自由度 UI/数据源扩展并将官方 WebDAV 统一纳入 `iris.webdav` 插件 |
| `engine/` 下的 `MediaBackend` trait | 仍没有；`player::Session` 直接用 `iris-mpv` | 只有 mpv 一套实现，等真有第二套原生照片/解码后端再提炼（DECISIONS 4.5） |
| `command.rs` / `state.rs` / `event.rs` 分文件 | `player/{mod,model,prepare,backend,inspect,relay}.rs` | 按会话、模型、准备、后端、检查与转发拆分 |
| `viewport.rs`、`playlist.rs`、`config.rs` | `geometry.rs`、`library.rs` + `catalog.rs`、`settings.rs` | `catalog.rs` 统一跨源条目与独立 `PlayQueue` |
| `platform/` 下按 OS 分目录，加 `VideoUnderlay` trait | `video/{mac,windows}.rs` + `platform/{mod,macos/,windows.rs}`，按 `cfg` 隔离 | 两个平台的生命周期差别大，暂不定公共 trait |
| `ui/` 拆成 workspace / titlebar / toolbar / sidebar / osd | `ui/{mod,events,vfs_ctrl,plugin_ctrl,preset_ctrl,chrome,actions*,viewport*,titlebar,info,palette,context_menu,dialogs,debug_cmd}.rs` + `ui/{kit,sidebar,controls,popover,plugin_ui,settings}/` | `ui/kit/` 提供共用原语，`check-project.py` 统一检查文件行数预算 |
| `control.rs`（原 `debug.rs`） | 仍叫 `debug.rs` | 只在 debug / `devtools` 构建启用 |
