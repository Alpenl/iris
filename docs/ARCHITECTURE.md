# Iris 架构

本文记录 Iris 的分层设计、macOS / Windows 原生视频合成、色彩管线、插件隔离架构与安全边界。决策和路线图见 [DECISIONS.md](DECISIONS.md)，历次决议讨论与取舍逻辑见 [DISCUSSIONS.md](DISCUSSIONS.md)，插件开发规范见 [PLUGINS.md](PLUGINS.md)，测试方法与实测数据见 [TESTING.md](TESTING.md)。

## 1. 分层

```
crates/
  iris-mpv/            libmpv 的手写 FFI（不用 bindgen）：client API + mpv_node 结构化传参 + RenderHandle 窄能力封装
  iris-api/            无头插件协议（protocol: 2 / ABI: 2）：根模块公共值类型与 v2/ 版本化线协议契约（仅依赖 serde）
  iris-base/           通用并发与网络底座：rt（CancelToken/Deadline/ThreadRole 受管线程与预算）+ net（HTTP/HTTPS 客户端、DNS/DoH、回环 HTTP、WebSocket/TCP 双工帧）
  iris-plugin-sdk/     Wasm 插件 Guest 侧 SDK：导出宏、类型化事件分发、异步任务/Blob 辅助与有界 JSON 校验（宿主零反向依赖）
  iris-plugin-guides/  官方构图辅助线插件（iris.guides）：使用公开 SDK 编译的可复现 Wasm 包
  iris-plugin-webdav/  官方 WebDAV 数据源插件（iris.webdav）：使用公开 SDK 编译的多实例远程存储 Wasm 包
  iris-core/           无头领域核心：纯状态机播放器会话、统一目录与独立队列、稀疏工作区预设、键位引擎、照片探测、凭据库与日志脱敏；不依赖 GPUI、iris-api 或插件运行时
  iris-plugin-host/    Wasm 插件宿主运行时（pulley64）：显式 PluginHost、单窗口会话生命周期、能力授权、有界网络/私有产物与三模流中继；不依赖 GPUI 或 iris-mpv
  iris/                GPUI 应用：窗口、ThemeTokens 动态令牌、ui/kit 共用原语、统一 VideoSurfaceBackend 平台视频层与系统桥接、单实例、调试管道
```

依赖方向严格单向：`iris` → `iris-plugin-host` → `iris-core` → `iris-base`，`iris-core` → `iris-mpv`，且 `iris` / `iris-plugin-host` → `iris-api`；`iris-plugin-sdk` 仅由 Wasm Guest 插件（含 `iris-plugin-guides`、`iris-plugin-webdav` 及 `examples/plugins/*`）引用，宿主零反向依赖（`iris` 仅在 `src/video/` 中通过 `clippy.toml` 豁免使用窄能力 `iris_mpv::RenderHandle` 与 `Session::video_recovery`，业务与 UI 代码由编译器强制禁止触碰 `iris_mpv` 或构造 `RenderHandle`）。`scripts/check-project.py` 与 `clippy` 自动检查：

1. Cargo 依赖图强制隔离：`iris-base` 零领域依赖，`iris-core` 不依赖 GPUI、`iris-plugin-host`、`iris-api` 或 `wasmtime`，`iris-plugin-host` 不依赖 GPUI 或 `iris-mpv`；
2. `iris-api` 常规依赖仅限 `serde`（不依赖 GPUI 或 `iris-mpv`）；
3. `crates/iris/clippy.toml`、`crates/iris-core/clippy.toml` 与 `crates/iris-plugin-host/clippy.toml` 通过 `disallowed-types` / `disallowed-methods` 强制约束 `iris_mpv::*` 与 `RenderHandle::new` / `Session::{render_handle, video_recovery}` 的调用边界，扫描全仓 `crates/*` 的 `src/` 与 `tests/` 禁止越权 `allow/expect(clippy::disallowed_*)`（仅显式豁免 `crates/iris/src/video/` 与 `crates/iris-core/src/player/mod.rs` 的 `render_handle` / `video_recovery` 两处），且全仓非测试生产代码禁止 `use super::*;`；
4. `crates/iris/src/ui/` 禁止散落 `target_os` 条件编译，且通用组件库 `crates/iris/src/ui/kit/` 禁止引用根窗口 `IrisWindow` 或业务宿主 `AppModel`；
5. 全仓生产代码禁止直接调用 `std::thread::spawn` 或未登记的 `std::thread::Builder`，所有 OS 线程必须通过 `iris_base::rt::{spawn_thread, spawn_thread_named}` 按 `ThreadRole` 登记，且每个角色的线程名必须记录在本文第 6 节；
6. `devtools` 控制管道门禁未被默认特性或普通发布版绕过；
7. 全仓 `.rs` 有效代码行（SLOC，剥离注释与空白行）预算（生产代码超过 650 SLOC 提示、硬上限 850 SLOC，尾部内联 `#[cfg(test)] mod tests` 硬上限 400 SLOC，独立测试文件硬上限 1,500 SLOC）、`crates/*/src` 顶层模块环单向棘轮、本文与双语 `README` 的成员同步门禁，以及图标、官方嵌入插件源码摘要隔离与示例插件清单一致性。

### 核心模块职责

- `crates/iris-mpv/src/{lib,event,render,sys}.rs`：纯手写 C 声明（`sys` 与 `Mpv::raw` 均为 `pub(crate)`，零常规依赖）与薄安全封装，不包含任何平台恢复或业务状态机。短属性名通过栈缓冲避免热路径堆分配。`Mpv::loadfile_with_options` 使用结构化节点传递单文件选项，不拼接 mpv 命令字符串；渲染层仅暴露 OpenGL 与 D3D11 共用的 `RenderHandle`。
- `crates/iris-mpv/build.rs`：优先使用 `MPV_LIB_DIR`。Windows x64 MSVC 开发构建回退到工作区 `target/deps/mpv/mpv.lib`；macOS 按目标架构查找对应 Homebrew 目录。始终动态链接。
- `crates/iris-api/src/`（根模块公共类型 vs `v2/` 协议子模块边界）：常规依赖仅 `serde`，零宿主 I/O 与零中文 UI 文案。
  - **根模块（`lib.rs` 及 `{manifest, overlay, source, workbench}.rs`）**：承载不绑定单版线协议帧外壳的共享声明式值类型与纯校验函数，供宿主侧（`iris`、`iris-plugin-host`）与 Guest SDK 直接经 `iris_api::*` 复用，包括 `PROTOCOL_VERSION = 2`、命令与数据源声明规格（`CommandSpec`、`SourceProviderSpec`、`is_valid_id`）、媒体与流变体值类型（`MediaKind`、`SourceEntryKind`、`SourceManageAction`、`StreamVariant`）、视口矢量叠层节点（`OverlayNode`、`TextAlign`）以及参数化工作台组件树与主题/栏位规格（`Widget` 含 `PixelGrid`、`PluginModal`、`SidebarPanel`、`SidebarSection`、`UiThemeSpec`、`ChromeLayoutSpec`、`ViewTimer`、`Color`、`is_valid_hex_color`）。
  - **`v2/` 协议子模块（`src/v2/{mod, manifest, wire, integer, source, stream, task, task_result, enhance, crypto}.rs`）**：收敛 `ABI_VERSION = 2` 的宿主↔Guest 版本化线协议契约与配额常量（`MESSAGE_LIMIT`、`CHUNK_LIMIT`、`JSON_DEPTH_LIMIT`、`COMMAND_LIMIT`、`ErrorCode`），包括包清单与权限/网络规则（`Manifest`、`Capability`、`NetworkDeclaration`、`HostRule`）、双向控制消息与句柄/整数编码（`HostEvent`、`HostCommand` 含 `Log`、`Id`、`Generation`、`WireU64` 十进制/数值双兼容）、目录分页与播放快照（`SourcePage`、`PlaybackSnapshot`）、三模流契约（`DirectHttp` / `RandomAccess` / `VirtualHls`）、异步任务请求与结果载荷（`Task`、`StoreRead`/`StoreWrite` 等）、媒体增强（轨道、LUT、章节、标注）及密码学原语规格。
- `crates/iris-base/src/{rt,net}/`：`rt` 收纳统一的取消令牌 `CancelToken`（无锁原子轮询快路径 + 嵌套子令牌与跨对象 `link` + TCP `shutdown(Both)` / `mio::Waker` / `Condvar` 唤醒回调）、单调截止时间 `Deadline` 与受管 OS 线程注册表（`ThreadRole`、`budget`、`spawn_thread` / `spawn_thread_named`）；`net` 收纳有界 HTTP/DoH 客户端、共用本地回环 HTTP 监听/Range 响应、WebSocket/原始 TCP 泵与零网络解析 `fuzz_net` 入口。
- `crates/iris-core/src/player/`：
  - `machine.rs` + `machine/reduce.rs`：不依赖 `libmpv` 即可独立单测的纯状态机 `PlayerMachine`、穷尽生命周期变体 `MediaSourceState::{Idle, Opening, Loaded, Failed}`（收拢 `entry_id`、`OpeningTransport` / `LoadedTransport` / `FailedTransport`、`in_flight_adds`、`deferred_local_subs`、`dirty`、`reattach_on_load`、`FailedEnhancements` 与 `eof_paused`，令非法标志位组合不可表达）、声明式副作用 `MpvOp` 与批次属性合并纯函数 `coalesce_updates`。
  - `recovery.rs`：跨平台视频轨恢复状态机 `VideoRecoveryHandle`（由 `EventPump` 驱动异步 `vid` 查询，供渲染层在设备重建后恢复活跃视频轨）。
  - `relay.rs` + `scoped.rs` + `enhance.rs`：会话级 HTTP(S) 密封回环中继（按 `RelayWorkerBudget` 限流）、`ScopedStream` / `MonitorLutFilter` 窄接口挂载及插件增强生命周期。
- `crates/iris-core/src/{media_uri,catalog,store,vault,preset}.rs`：
  - `media_uri.rs`：三元正交寻址 `MediaUri`（`Local` | `HttpUrl` | `Plugin`，经 `MediaUri::parse_persisted` 无损升级历史 `webdav://` URI）。
  - `catalog.rs`：跨本地、插件源与收藏统一的 `CatalogItem` 排序/过滤纯函数，以及与当前浏览文件夹彻底解耦的独立播放队列 `PlayQueue`。
  - `store.rs`：以稳定媒体身份保存收藏、续播和最近记录。写入时取得文件锁，重读磁盘、合并修改字段并原子替换。写入失败保留待保存变更，旧记录在迁移成功后退役。
  - `vault.rs`：系统安全凭据库 `CredentialVault`（macOS Keychain / Windows Credential Manager，单条上限 2048B，失败显式报错）。v2 插件按安装身份 `plugin-v2.<installation_id>` 隔离（兼容读取迁移期 `builtin.<id>` 与 `ext.<id>`），Windows 下采用 `app.iris.vault.v2:` 小写十六进制编码目标名以消除大小写不敏感碰撞并保护作用域/键名边界。`credential_index.json` 只保存作用域与键名，测试可用 `in_memory()`。
  - `preset.rs` + `preset/runtime.rs`：`1B+` 稀疏工作区预设（`Option<T>` 补丁 + 显式插件启停差量与退出指令），`settings.conf` 始终只存纯净个人基线，运行期通过内存 `PresetRevert` 自动回正（全局字号 `ui_font_scale` 属个人偏好不进预设）。
- `crates/iris-plugin-host/src/`：显式 `PluginHost` 实例（零全局单例，支持同进程多宿主并行测试）、有界 Wasmtime Pulley64 字节码运行时、单窗口会话生命周期、安装/能力授权、网络、三模流（`DirectHttp` / `RandomAccess` / `VirtualHls`）、轻量私有文件产物与哈希（`FileExecutor`）及媒体增强；`iris-plugin-sdk` 和两个官方插件 crate 使用同一套公开 Guest 入口。
- `crates/iris-core/src/photo/{mod,containers,heif,exif,icc}.rs`：魔数优先单遍照片头解析（JPEG/PNG/WebP/HEIF/TIFF 尺寸、方向、EXIF 摘要与内嵌 ICC 原色匹配）。
- `crates/iris-core/src/{keymap/,vfs/,logging/,input,geometry,library,settings,fs_util}.rs`：三方案键位目录与结构化 `ConflictTarget`、常驻位置书签、单一集中脱敏日志与结构化事件流（`iris.log` + `events.jsonl`）、指针手势策略、几何变换与慢速共享盘优化扫描。
- `crates/iris/src/video/{mod,mac,windows}.rs`：跨平台共享的 `VideoSurfaceBackend` trait（`attach`、`set_bounds`、`set_scale`、`set_visible`），在 macOS（`MacVideoSurface`）与 Windows（`WinVideoSurface`）上由编译器验证统一签名，并通过 `VideoSurface` 新类型对窗口层隐藏平台差异。
- `crates/iris/src/theme.rs` + `ui/`：GPUI `Global` `ThemeTokens` 引擎、与根视图彻底解耦的通用控件库（`ui/kit/`，通过泛型 `Entity<V>` 与 `TextFieldHost` 组合，零 `IrisWindow`/`AppModel` 依赖）、按领域归集的中文文案表（`ui/strings/`），以及围绕 `AppModel` 领域宿主与 5 个独立区域 `Render` 实体（`Sidebar`、`PluginPanel`、`SettingsPanel`、`ViewportView`、`DockView`）构建的零副作用渲染树——窗口尺寸/焦点观察与字幕内边距同步均挂在观察器及帧后回调中，1–4Hz 播放进度与插件槽位变更经 `DirtyRegions` 局部通知对应子视图。
- `crates/iris/src/instance.rs`：Windows 单实例，见第 9 节。macOS 由系统把打开请求发给已运行进程，经 GPUI `on_open_urls` 进来，只接受 `file://`。
- `crates/iris/src/platform/{mod.rs,macos/{mod,imageio}.rs,windows.rs}`：按 OS 拆分的原生系统桥接——自实现窗口拖动、窗口置顶、播放期防休眠 `SleepPreventer`、macOS 触控板双指捏合 `PinchMonitor`、媒体键与系统播放中心桥接 `MediaKeyBridge`（macOS `MediaPlayer.framework` / Windows WinRT `SystemMediaTransportControls`，Windows 统一使用 `windows 0.61` 绑定）、默认截图目录与照片内存缩略图解码 `decode_thumbnail`。
- `crates/iris/src/debug.rs`：调试控制管道（`IRIS_CONTROL`），见第 8 节。

### 播放器加载、切换与挂载状态机

`PlayerMachine` 将会话状态建模为显式变体，任何带有过期 `playlist_entry_id` 的底层事件在入口处即被丢弃，无法污染新媒体的生命周期或误消费待挂载轨道：

```mermaid
stateDiagram-v2
    [*] --> Idle

    Idle --> Opening: begin_load(entry_id, OpeningTransport)<br/>若 eof_paused 则自动解除 pause
    Opening --> Opening: 快速切歌 begin_load(new_id)<br/>取消旧 Relay / 停止异源 ScopedStream<br/>旧 entry_id 事件全部被栅栏丢弃
    Loaded --> Opening: 切换媒体 begin_load(new_id)<br/>回收活跃 LUT 并重置单文件后端状态
    Failed --> Opening: 重试或 GPU 重建恢复<br/>恢复 FailedEnhancements 快照并装填 reattach_on_load

    state Opening {
        [*] --> AwaitHooks
        AwaitHooks --> DemuxerOpen: on_load（HLS 临时开启 access-references=yes）
        DemuxerOpen --> AwaitLoaded: on_preloaded（立即恢复严格 file-local-options 默认值）
    }

    Opening --> Loaded: SessionEvent::Loaded (entry_id 匹配)<br/>promote_opening_to_loaded：<br/>DirectRemote 派发 pending_subs；<br/>PluginScoped 恢复挂载策略并发送带回执轨道挂载 -> in_flight_adds；<br/>重挂 reattach_on_load
    Opening --> Failed: SessionEvent::Error / 空闲溢出<br/>取消 DirectRemote 中继，保留可恢复源描述
    Opening --> Idle: stop() / 已取消 ScopedStream 失败

    state Loaded {
        [*] --> TracksSettling
        TracksSettling --> Ready: 无在途挂载 或 最后一个 CommandReply 到达<br/>自动冲刷 deferred_local_subs 并恢复受限挂载策略
        Ready --> TracksSettling: attach_plugin_track 发送新回执<br/>期间本地 LoadSubtitle 同步校验 UTF-8 后排入 deferred_local_subs
    }

    Loaded --> Loaded: Ended(true) 置 eof_paused=true；<br/>TogglePlayback(EOF) 推进 scoped seek 并回零续播
    Loaded --> Failed: SessionEvent::Error<br/>捕获 LUT/轨道/时间轴弱引用至 FailedEnhancements
    Loaded --> Idle: stop()<br/>保留 eof_paused 供下次打开自动续播
    Failed --> Idle: stop() / release_scoped()
```

### libmpv 升级检查清单

`crates/iris-core/src/player/relay/tests.rs` 中的契约测试 `hls_hook_contract_pins_mpv_version_restores_reference_policy_and_keeps_segments_readable` 将当前经过验证的 `mpv-version` 锁定在 `0.40..=0.42`。升级 `libmpv` 版本前必须逐项核对以下内部行为，确认无误后再同步更新版本区间：

1. **`on_load` / `on_preloaded` HLS 引用策略捕获时序**：确认 `on_load` 设置 `file-local-options/access-references=yes` 并在 `on_preloaded` 立即重置为 `no`（同时清空 `demuxer` / `demuxer-lavf-format` / `demuxer-lavf-o`）后，已打开的 HLS demuxer 仍能继续拉取后续分片，且加载完成后的外挂 `.m3u8` 字幕无法跟随远端引用。
2. **`loadfile` 结构化 `MPV_FORMAT_NODE_MAP` 参数名**：确认当前 client API 版本下 `loadfile` 的参数键名（`url`、`flags`、`index`、`options`）未发生破坏性变更。
3. **`playlist_entry_id` 事件围栏与溢出恢复**：确认 `StartFile`、`EndFile` 携带的 `playlist_entry_id` 与 `playlist-playing-pos` / `playlist/{pos}/id` 属性一致。
4. **Windows D3D11 设备丢失恢复**：在 Windows 实机执行 `sim_device_reset`，确认 `VideoRecoveryHandle` 的异步 `vid` 查询与 `reselect_after_surface_rebuild` 仍能无缝恢复硬解视频画面。

## 2. macOS 视频合成（已验证）

```
NSWindow contentView
├── GPUI 视图（Metal，非不透明，每帧清成透明）  ← 标题栏、目录、工具栏、浮层、插件矢量叠层
└── IrisVideoLayer 视图（CAOpenGLLayer）          ← mpv render API（OpenGL）
```

代码在 `crates/iris/src/video/mac.rs`（实现 `VideoSurfaceBackend`）。

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

`crates/iris/src/video/windows.rs`（实现 `VideoSurfaceBackend`）已实现 D3D11 + DirectComposition。独立渲染线程拥有 D3D immediate context 和 mpv render context；设备丢失恢复时的活跃视频轨追踪由 `iris_core::player::VideoRecoveryHandle` 维护；UI 只更新合并后的布局并唤醒线程，视频帧不经 CPU 或 GPUI。实机已验证 `d3d11va`、设备重置恢复、透明控件叠加、窗口／最大化／全屏、鼠标实时缩放及正常退出；不能由此保证所有 GPU 和系统版本。

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
- 绑定统一使用微软 `windows` / `windows-sys` `0.61` 同代绑定，COM 引用自动管理；关闭时先停止并 join 视频线程、释放 render context，再退出 mpv。
- `crates/iris/app.manifest` 启用 Common Controls v6 与 PerMonitorV2。缺少 v6 时 GPUI 导入的 `TaskDialogIndirect` 会令进程在 `main` 前失败，首次 Windows 上机已发现并修复。

### 已验证边界与剩余风险

1. **独立提交仍存在理论帧差**：窗口边框实拖过程中截图未见明显错位，最大化、全屏前后也正常；未用高速摄影证明每帧严格同步，不承诺零帧差。
2. **透明背景**：当前 `Opaque` 下 GPUI 仍清透明，视频透出；本机没有观察到系统强调色染色。保留平台行为说明，不增加猜测性的底层补丁。
3. **环境变量**：`main` 在创建线程前清掉 `GPUI_DISABLE_DIRECT_COMPOSITION`，避免环境把 GPUI 切换成不透明普通交换链。
4. **截图**：Windows 调试 `snap` 捕获窗口所在桌面矩形，可看见两个 DComp 目标；要求窗口可见、未被遮挡，不是离屏／后台窗口截图。macOS 仍为只截本窗口。
5. **调试流**：Windows 当前用户 SID 的保护 DACL、拒绝远程命名管道；macOS Unix socket 权限 0600 并检查 peer uid。单行 16 KiB、待处理队列最多 256 条。普通 release 不创建监听。
6. **SDR 色彩**：D3D 目标为 BGRA8；读取所在显示器的系统 ICC 交给 mpv / Little CMS，移动到另一显示器后更新。无配置文件时使用 SDR sRGB。HDR/EDR、任意 ICC 与 Windows ACM 全组合未验证，不宣称 HDR 支持。

## 5. Wasm 插件、授权网络与媒体管道

`iris-api::v2` 定义包和控制消息，`iris-plugin-sdk` 提供 Guest ABI；独立的 `iris-plugin-host` 负责 Pulley64 字节码执行、安装、资源账本、网络与流。单窗口绑定显式 `PluginHost` 实例（无全局单例），每安装身份一个 ControlStore，每路 Guest 流独立 StreamStore。所有耗时 I/O 返回请求 ID 后在后台完成，UI 不等待线程退出。

官方 `iris.guides` 与 `iris.webdav` 使用公开 SDK 编译成可复现 Wasm 包，构建时嵌入并校验摘要，遵守同样的授权与配额。A-B 循环归原生 Session；旧 BuiltinPlugin/stdio/Python 插件运行时已移除。用户包经设置中的宿主审核安装，更新增权不静默通过。

网络将传输能力与目的地范围分开（统一收口在 `iris-base::net` 的策略层），全链路检查初始 URL、重定向和实际连接地址。任意公网权限仍拒绝私网；NAS 使用用户确认的具体 Origin 与固定地址集合。播放、字幕与缩略图复用所属插件授权，跨源剥离敏感头。

DirectHttp、RandomAccess、VirtualManifest 共用受控播放准入。远端及虚拟 HLS 经结构化 `VirtualHls` 解析并生成本会话密封回环引用，拒绝未知必要语义且不向 libmpv 回退原始清单（零调用方的 DASH 与受限 HLS 文本通道已在 `0.0.2` 移除）。绑定资源持续到异步 mpv 加载确认和实际 I/O 结束，取消中的资源仍占额度。

主题、侧栏、弹窗（含正交 `Widget::PixelGrid` 与 `ViewTimer`）、时间轴、叠层使用受限声明式数据，插件可通过 `HostCommand::Log` 写入经宿主 20 条/秒限流与脱敏的结构化事件流；标记写应用数据库。零调用方的外部原生工具（BYOB）与文件导出模块已在 `0.0.2` 裁剪，当前仅保留受控私有产物读写与 `HashCurrentMedia`。详细权限/ABI/生命周期/配额见 [插件规范](PLUGIN_V2_ARCHITECTURE.md)，开发入口见 [插件指南](PLUGINS.md)，验证范围见 [实施证据](PLUGIN_V2_IMPLEMENTATION.md)。

## 6. 并发模型、线程清单与预算

所有生产环境的 OS 线程均在 `crates/iris-base/src/rt/mod.rs` 中由 `ThreadRole` 集中登记，并通过 `spawn_thread` / `spawn_thread_named` 创建。`scripts/check-project.py` 在静态检查阶段禁止 `crates/iris-base/src/rt/mod.rs` 以外的生产代码直接调用 `std::thread::spawn` 或 `std::thread::Builder`，并校验下表完整覆盖全部 18 类受管线程：

| `ThreadRole` | 线程名 / 前缀 | 局部上限 | 全局上限 | 超限与背压策略 | 生命周期与回收所有权 |
|---|---|---|---|---|---|
| `ControlPipe` | `iris-control` | 1 | 1 | `WouldBlock` 拒绝重复监听 | 仅 debug / `devtools` 启动；进程退出结束 |
| `SingleInstanceIpc` | `iris-open-files` | 1 | 1 | `WouldBlock` 拒绝重复监听 | Windows 主实例持有管道服务端，进程退出结束 |
| `VideoSurface` | `iris-video` | 1 / 视口 | 视口数 | 每视口单实例构造 | 视口关闭时先释放 render context 并同步 `join` |
| `MpvEventPump` | `iris-mpv-events` | 1 / `Session` | `Session` 数 | 每会话单实例构造 | `EventPump::stop` 发送 `quit` / `CancelToken` 并同步 `join` |
| `MediaRelayListener` | `iris-media-relay` | 8 / `Session` | 64 | `WouldBlock` 快速失败 | `Service::drop` 触发 `CancelToken` + `mio::Waker`，`iris-relay-reap` 回收 |
| `MediaRelayReaper` | `iris-relay-reap` | 8 / `Session` | 64 | 超限时回退同步 `join` | 短生命周期回收线程，负责非阻塞释放监听与在途请求线程 |
| `MediaRelayWorker` | `iris-media-request` | 8 / 中继 | 32 | 超限或起线程失败立即回 `503 Busy` | `CancelToken` 触发 `TcpStream::shutdown(Both)`，由 `MediaRelayReaper` `join` |
| `SystemDnsWorker` | `iris-http-dns` | 16 并发查询 | 64 | `ACTIVE_DNS_LOOKUPS` 超限拒绝新建 | 受 `Deadline` 约束，解析完成后自动退出并归还信号量 |
| `WasmEpochTimer` | `iris-wasm-epoch` | 1 / `RuntimeEngine` | `Engine` 数 | 起线程失败回滚活跃计数并返回 `Internal` | 无活跃 Guest 调用时 `Condvar` 挂起，`Engine` 析构时唤醒退出 |
| `ArtifactCleanup` | `iris-artifact-cleanup` | 1 | 1 | `WouldBlock` 跳过重复后台清理 | 启动期一次性扫描清理残留临时目录后立即退出 |
| `PluginCoordinator` | `iris-plugin-coordinator` | 1 / `PluginHost` | `PluginHost` 数 | 起线程失败 `PluginHost::new` 返回 `Internal` | `PluginHost` 释放后置 `shutdown`，待在途作业与退役实例归零后退出 |
| `PluginStoreActor` | `iris-v2-` (`Control` / `Stream`) | Control 1/插件、Stream 2/插件 | Control 64、Stream 8 | `Ledger` 预留失败返回 `QuotaExceeded` | `Actor::stop` 触发 `CancelToken` 与唤醒，`is_quiescent` 确认 `done && outstanding == 0` |
| `PluginPoolWorker` | `iris-v2-` (`{control,stream,package,setup}-io-{id}`) | 按宿主分片公平轮转 | 4 + 8 + 2 + 2 = 16 | 队列满返回 `Backpressure`；空闲 30s 自动缩容 | 按需懒启动，`Dispatcher::drop` 唤醒全部工作线程退出 |
| `PluginAuthCallback` | `iris-v2-auth` | 1 / 插件 | 4 | `Ledger` `Resource::Auth` 超限拒绝 | 单次 OAuth 回调或 `Deadline` 到期/`CancelToken` 取消后退出，`Activity` 追踪归零 |
| `PluginOriginDns` | `iris-origin-dns` | 6 / 插件 | 16 | `Ledger` `Resource::Dns` 超限拒绝 | 10s `Deadline` 与 `CancelToken` 双重约束，持槽至底层解析返回后释放 |
| `PluginSocketPump` | `iris-v2-socket` | 每类 4 / 插件 | 每类 16（WS + TCP 共 32） | `Ledger` 超限拒绝；读写槽单路互斥 | `Socket::close` / `NetworkGrant` 吊销触发 `CancelToken` + `mio::Waker`，`Activity` 归零 |
| `PluginRelayListener` | `iris-v2-relay` | 4 / 插件 | 12 | `Ledger` `Resource::Relay` 超限拒绝 | `Relay::stop` 取消会话并唤醒 `mio::Poll`，`Relay::is_quiescent` 确认强引用归零 |
| `PluginRelayWorker` | `iris-v2-range` | 8 / 中继 | 32 | 超限或起线程失败立即回 `503 Busy` | 子 `CancelToken` 绑定回环 `TcpStream`，Range 结束或取消后递减活跃计数退出 |

### 统一取消与截止时间原语

- **`CancelToken`**：内部采用 `AtomicBool`（`Acquire` 无锁轮询快路径，供 Wasm 宿主导入门禁、事件泵与紧循环零锁检查）配合受互斥锁保护的唤醒表（已注册 `TcpStream` 的 `shutdown(Shutdown::Both)`、`mio::Waker`、`Condvar` 唤醒回调及弱引用子令牌树 `child()` / `link()`）。取消动作在锁外触发回调与级联子令牌，消除锁反转风险；`HttpCancel` 即 `CancelToken` 的类型别名。
- **`Deadline`**：基于单调时钟 `Instant` 的统一截止时间封装，提供 `remaining()` / `remaining_capped(limit)` 与 `CancelToken::check(deadline)` / `remaining_timeout(deadline, limit)`，统一驱动 HTTP 连接/读写、DoH 回退、OAuth 回环监听、Origin DNS 审查、WebSocket/TCP 泵、串行存储票据与 Wasm Guest 执行超时判定。

## 7. 依赖

| 依赖 | 用途 | 规则 |
|---|---|---|
| `gpui = "=0.2.2"` | UI | crates.io 版，`default-features = false, features = ["font-kit", "runtime_shaders"]`，只需 Command Line Tools。锁死版本，主动升级。不引第三方 GPUI 组件库。|
| `iris-mpv`（本仓库） | libmpv FFI | 手写声明，零常规依赖，不用 bindgen。|
| `iris-api`（本仓库） | 插件协议 | 仅依赖 `serde`（测试用 `serde_json`），零 UI/媒体依赖。|
| `iris-base`（本仓库） | 并发与网络底座 | `rt` 并发原语 + `net` 网络协议，仅依赖网络/TLS/同步基础库，零领域依赖。|
| `iris-plugin-host`（本仓库） | Wasm 插件宿主 | 依赖 `wasmtime`（仅开 `pulley` 字节码解释器）、`iris-core` 与 `iris-api`，不依赖 GPUI 或 `iris-mpv`。|
| `windows = "0.61"` / `windows-sys = "0.61"` | Windows 原生绑定 | 全仓统一为 `0.61` 同代绑定，避免双代 Win32/COM crate 并存编译。|
| `objc 0.2`、`cocoa =0.26.0` | macOS 原生调用 | GPUI 0.2.2 内部也用这两个，迁到 objc2 后类型不互通，暂不迁移。|
| `futures` | 通道 | 用 GPUI 自带的 executor；Iris 自己不直接引 tokio（GPUI 的 HTTP 客户端会间接带进来，这不归 Iris 管）。|

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

2026-11-20 规划讨论稿提出分层结构并展望插件扩展；经过 MVP 验证、核心体验完善与架构演进后，实际结构与规划对照如下：

| 规划稿 | 实际 | 原因 |
|---|---|---|
| `iris-core` 不依赖 GPUI | 已拆分为 `iris-base` + `iris-core` + `iris-plugin-host` 并由 `check-project.py` 强制约束 | 核心领域、并发/网络底座与 Wasm 宿主各自独立编译单测 |
| `iris-api` crate 与控制面插件 | 已落地：`iris-api/src/v2/` + `iris-plugin-host/src/` + `ui/{palette.rs,plugin_ui/}` | 坚持不接触像素数据，同时保留高自由度 UI/数据源扩展并将官方 WebDAV 统一纳入 `iris.webdav` 插件 |
| `engine/` 下的 `MediaBackend` trait | 仍没有；由 `player::machine` 将状态转移与 `MpvOp` 纯函数化，`Session` 薄适配 `iris-mpv` | 只有 mpv 一套实现，纯状态机已让播放逻辑脱离 `libmpv` 独立可测 |
| `command.rs` / `state.rs` / `event.rs` 分文件 | `player/{mod,machine,model,prepare,backend,events,inspect,recovery,relay,scoped,enhance}.rs` | 按纯状态机、模型、准备、后端、事件、恢复、转发与增强拆分 |
| `viewport.rs`、`playlist.rs`、`config.rs` | `geometry.rs`、`library.rs` + `catalog.rs`、`settings.rs` | `catalog.rs` 统一跨源条目与独立 `PlayQueue` |
| `platform/` 下按 OS 分目录，加 `VideoUnderlay` trait | `video/{mod,mac,windows}.rs`（共用 `VideoSurfaceBackend` trait）+ `platform/{mod,macos/,windows.rs}` | 编译期统一跨平台视频层接口签名，内部实现按 OS 隔离 |
| `ui/` 拆成 workspace / titlebar / toolbar / sidebar / osd | `ui/` 下按区域与共用 `ui/kit/` 原语拆分 | `ui/kit/` 提供无宿主耦合原语，`check-project.py` 统一检查文件行数预算 |
| `control.rs`（原 `debug.rs`） | 仍叫 `debug.rs` | 只在 debug / `devtools` 构建启用 |
