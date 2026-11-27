# 决策记录与路线图

来源：2026-11-20 的长期规划讨论稿、对其逐条复核（对照本机 mpv 0.42 头文件、GPUI 0.2.2 源码、仓库代码和 `Cargo.lock`），以及 2026-11-22 的播放器体验完善、控制面插件系统落地与中期架构校准。这里只记录复核后成立、或明确拍板的结论；**每次决议背后的核心考量、候选方案对比与为什么这样取舍详见 [DISCUSSIONS.md](DISCUSSIONS.md)**；被复核否定的说法单独列在第 8 节，避免以后再被当成依据。

状态：**已完成** / **进行中** / **计划**（已同意，未开始）/ **待决**（需要用户决定）。

## 1. 宏观方向

| # | 结论 | 状态 |
|---|---|---|
| 1.1 | 定位：高性能本地照片 / 视频 / 音频播放器内核 + 高自由度控制面、界面形态与虚拟数据源插件扩展。本体默认保持克制与视觉一致，解码与色彩渲染由内核独占；本地内核只留通用网络流传输与安全凭据底座，所有具体远程存储协议（含官方标配 WebDAV）一律由插件实现。 | 方向已定 |
| 1.2 | 插件系统演进：MVP 阶段先把播放器本体做扎实；v1 曾验证 JSON-lines stdio 原型，现已由 [PLUGIN_V2_DECISIONS.md](PLUGIN_V2_DECISIONS.md) 取代并统一升级为 **`protocol: 2 / ABI: 2`（Wasmtime 沙箱 + 官方 Wasm 包 `iris.guides` / `iris.webdav` + 宿主单向隔离边界）**，同时 `A-B 循环` 回归播放内核原生能力。 | 已被 `PLUGIN_V2_DECISIONS.md` 取代（现为 `protocol: 2`） |
| 1.3 | 插件只在控制面、虚拟数据源与参数化工作台工作，**不经手实时像素数据**；像素管线由内核独占。 | 已落实 |
| 1.4 | 界面的渲染管线与焦点纪律由内核掌握，插件通过统一的 `Widget` 控件树（侧栏面板与异步校验弹窗共用）、`StatusBadge`、`ToolbarButton`、`OverlayNode` 视口叠层、`UiThemeSpec`/`ChromeLayoutSpec`（允许自由定制配色、圆角/栏高与 `hide_titlebar` 无边框影院窗，宿主提供顶缘悬浮唤醒、视口拖拽与对比度逃生护栏）及支持动态多实例管理的 `MediaSource`（`SetSources` / `SourceManage`）契约与宿主通信。 | 已落实 |
| 1.5 | 不做多轨合成的非线性剪辑；mpv 是单管线前向播放，做不了。 | 方向已定 |
| 1.6 | UI 发语义化命令 → 会话改状态 → 语义事件 → UI 重绘；UI 不直接使用 mpv 属性（渲染层仅持窄能力 `RenderHandle`）；坐标换算显式建模；机制和策略分开。 | 已收口到 `Session` / `RenderHandle` / `geometry.rs` / `input.rs` / `plugin_v2/` |
| 1.7 | 当前不做：Lua/JS 脚本引擎、SQLite 媒体库、Linux 移植。外部扩展已演进为受控 Wasmtime 沙箱（详见 [PLUGIN_V2_DECISIONS.md](PLUGIN_V2_DECISIONS.md)），取消旧版无沙箱 Python/stdio 子进程模型。 | 已更新（见 `PLUGIN_V2_DECISIONS.md`） |
| 1.8 | 本体默认界面保持方向 C 的配色和整体风格（细节参照 Zed 和 Sooty：默认直角、36px 标题栏、44px 单行工具栏、60fps 跟手拖拽时间轴、16px SVG、可调宽虚拟滚动侧栏），但**不以此限制插件的定制自由度**；圆角与栏高经 `ThemeTokens` 统一封装进 `ui/kit/` 原语。照片的缩放比例按原始像素算，1:1 即 100%。 | 已完成（2026-11-23 Windows 11 双屏实机核验通过；macOS 本轮新界面人工走查待补） |

## 2. 技术栈与依赖

| # | 结论 | 状态 |
|---|---|---|
| 2.1 | 保留 GPUI，`gpui = "=0.2.2"` 锁死版本，主动升级，不跟上游 main。 | 已完成 |
| 2.2 | 不引第三方 GPUI 组件库（如 gpui-component），控件全部手写（含设置弹窗、右键菜单、命令面板、锚定浮层、虚拟目录滚动条）。 | 已遵守 |
| 2.3 | 保留手写的 `iris-mpv`，不换社区 libmpv crate，不用 bindgen。 | 已完成 |
| 2.4 | macOS 继续用 `cocoa` / `objc`，**暂不迁移 objc2**：GPUI 0.2.2 内部用的是旧库，迁移后类型不互通。警告已用 check-cfg 压住（`crates/iris/Cargo.toml`）。 | 已定 |
| 2.5 | Windows 使用微软 `windows` crate 管理图形／窗口 COM 与防休眠（`Win32_System_Power`），命名管道使用 `windows-sys`。 | 已完成 |
| 2.6 | Iris 自己不直接引 tokio，用 GPUI 自带的 executor。注：GPUI 0.2.2 经 `gpui_http_client` → `zed-reqwest` 间接带入 tokio，这不归 Iris 管。 | 已遵守 |
| 2.7 | 设置文件继续用手写的 `key = value`（原子替换写回）；插件清单 `plugin.json` 与 stdio 协议使用 `serde` / `serde_json`（已在依赖树中，MIT/Apache-2.0）。 | 已遵守 |
| 2.8 | 文件对话框用 GPUI 自带的 `prompt_for_paths`。 | 已遵守 |
| 2.9 | 目录按自然顺序排序（`ep2` 在 `ep10` 前），手写比较器、不加依赖，并支持按修改时间与文件大小升降序切换。 | 已完成（`library.rs`，含单元测试）|
| 2.10 | 运行日志统一走 `log` + 结构化本地 JSONL 事件流（`logging/`），GUI 发布版写入系统应用日志目录（4 MiB × 5 份按大小轮转），全部文本与事件经 `redact_text`/`redact_json` 单一脱敏收口。 | 已完成（`logging/`）；控制协议回复仍写 stderr |
| 2.11 | 调试控制使用跨平台本地流：macOS Unix socket，Windows 同用户命名管道。 | 已完成；普通 release 忽略 `IRIS_CONTROL`，测试需 `devtools` |
| 2.12 | `deny.toml` 检查 RustSec 公告、许可证和依赖来源。C 依赖由锁定的 HTTPS 源码、SPDX 许可表和发布清单约束。 | CI `Release recipe policy` 持续检查 |
| 2.13 | Fake-IP 代理下公共 DoH 回退**默认关闭**（`fake_ip_doh_fallback = false`），避免自建 WebDAV / NAS 等私有源主机名在未经显式同意时发往公共解析器或绕过代理的域名分流规则；用户显式开启后可经 `doh_resolvers` 配置数字公网 IP / HTTPS DoH 端点（强制校验 `AddressKind::Public`）；`198.18.0.0/15` 与 `2001:2::/48` 永久保持 `AddressKind::Forbidden`，不开放直连信任 Fake-IP 段的开关。 | 已完成（`doh.rs` + `settings.rs`） |

## 3. 五个“单向门”

| # | 结论 | 状态 |
|---|---|---|
| 3.1 | **会话挂在视口上，不挂在窗口上**：窗口持有独立的 `viewport: Viewport` 结构体（封装独立会话、画面层、矩形和照片变换，待真正实现同屏对比时再扩为多视口集合）。 | 已完成；不提前实现同屏对比 |
| 3.2 | **上下文驱动的输入**：键盘按 `Workspace` / `Viewport` / `SearchInput` / `CommandPalette` 与 `media` 匹配；指针与捏合手势经同样的状态策略解析。 | 已完成（`input.rs` + `platform/`） |
| 3.3 | **许可走路线 B**：主程序 `MIT OR Apache-2.0`，保留商业 / 闭源扩展可能；媒体动态库独立遵守 LGPL。 | 已落实，见第 5 节 |
| 3.4 | **媒体只读 + 路径做主键**：内核对媒体文件和目录永远只读，个人数据（设置、收藏、进度、最近打开、插件）全放应用数据目录，照片缩略图仅驻留内存。内容指纹重新匹配尚未做。 | 只读已遵守；收藏、进度与最近打开已实现（`store.rs`） |
| 3.5 | **色彩准确是内核的硬指标**：视频层与照片内嵌 ICC 色彩管理归内核独占，插件与缩略图条不干扰主视口色彩管线。 | 进行中（SDR 完成，HDR 待后续版本） |

### 3.2 输入上下文细则

- 上下文分层：`Workspace`（全局）→ `Viewport`（视口共性）+ 状态标签 `media=video|image|none`、`zoomed` → 最高层的 `SearchInput` / `CommandPalette` / 浮层拦截。
- 上下文由当前状态在 `render()` 里纯函数推导，不手动 `set_mode`。
- GPUI 的 `KeyBinding` 只管键盘。滚轮、拖拽、单击、双击、双指捏合统一由 `resolve_pointer_input(event, context) -> Option<PointerCommand>` 解析；触控板像素滚动增加阻尼与累加，避免滑过头。
- 侧栏、浮层、设置弹窗、右键菜单、命令面板和缩略图条均阻断滚轮与点击冒泡，避免误触发画布缩放或单击暂停。
- 动作按意图命名，不按按键命名；跨平台快捷键用 GPUI 的 `secondary-*` 修饰符（macOS 是 ⌘，Windows 是 Ctrl）。
- 复核补充的约束与落实：
  - GPUI 0.2.2 没有原生双指捏合（magnify）事件，已在 `crates/iris/src/platform/macos.rs` 通过 macOS `NSMagnificationGestureRecognizer`（`PinchMonitor`）原生接入。
  - `start_window_move()` 在 macOS / Windows 上是空函数，窗口拖动继续用 `platform/` 自己的实现。
  - **焦点纪律**：工具栏、标题栏、浮层按钮点击后不保留键盘焦点，焦点回到视口；仅目录搜索栏（`search_focus`）和命令面板（`palette_focus`）在激活时持有文本输入焦点。
  - 单击视频暂停采用 200ms 延迟触发并在双击全屏到达时自动取消，消除单击/双击冲突。

## 4. 推进顺序

| # | 步骤 | 状态 |
|---|---|---|
| 4.1 | 在 Mac 上实测照片走 mpv 的五项边界（EXIF 方向、HEIC 分块、内嵌 ICC、超过 16384 像素、翻页延迟）。 | 已完成，结果见 TESTING.md |
| 4.1a | 顺带：在 Mac 上手动拖动窗口边缘，看视频和界面有没有错位（和 Windows 同步风险同类）。 | 待用户手测 |
| 4.2 | v0.1 MVP 验收范围与目录/收藏/续播收口。 | 已完成 |
| 4.3 | mpv 控制和事件翻译收口到 `player::Session`，会话归视口，绝对路径作身份，交互后焦点回视口。 | 已完成 |
| 4.4 | 在拆分 `iris-core` 之前验证 Windows 原生视频合成。 | 已完成首轮：D3D11 硬解、窗口/最大化/全屏/实时缩放/退出已实测 |
| 4.5 | 拆出 `iris-core`（无头内核）；UI 只发语义命令，不定义假想 `MediaBackend`。 | 已完成 |
| 4.6 | 成熟播放器细节打磨：可调宽虚拟目录、搜索与排序、子目录导航、设置面板、60fps 跟手进度条与章节标记、连续音量条、音频支持、字幕自动加载与偏移、画面比例/旋转/翻转/截图、防休眠、窗口置顶、独立轻量 OSD、右键菜单、媒体信息与 EXIF、照片缩略图底条、macOS 捏合缩放与媒体键。 | 已完成（2026-11-22，CI 双端通过） |
| 4.7 | 落地统一控制面与矢量叠加面插件运行时：`iris-api` 无头协议 crate（`PROTOCOL_VERSION = 1`）、`PluginManager` 与外部 stdio 子进程运行时、内置官方插件与外部参考插件。 | 已完成（2026-11-22） |
| 4.8 | 中期方向校准、工作区预设重塑与全远程协议插件化收口（见 `DISCUSSIONS.md`）。HTTP(S) 播放通过会话专属 `PlaybackRelay`，内部仍用结构化 libmpv 命令。WebDAV 按标准插件协议实现，凭据按作用域存入系统安全凭据库。工作区预设保护个人基线，统一 `MediaUri`、`CatalogItem`、`PlayQueue` 和 `Store`。生产代码行数由 `check-project.py` 按 650 行提示、850 行上限检查。 | 已完成 |

## 5. 许可：路线 B

用户已选路线 B：主程序用宽松协议，保留以后出商业版或闭源插件的可能。落地要求：

| # | 结论 | 状态 |
|---|---|---|
| 5.1 | 主仓库采用 `MIT OR Apache-2.0`；补齐两份标准许可全文。 | 已落实（含新增 `iris-api` crate） |
| 5.2 | 分发用的 libmpv / FFmpeg **自行源码构建为 LGPL**：关闭 GPL/version3/nonfree/Chromaprint，不引 FFTW。 | `scripts/release/` 和 CI 已实现 |
| 5.3 | **不用 mpv 官方的 LGPL 预编译包**：官方 LGPL 包启用了 Chromaprint 并配 GPL 的 FFTW3，必须自建并关掉 Chromaprint。 | 已定 |
| 5.4 | Homebrew 的 libmpv 是 GPL 构建，**只能开发时用**，不能拿去分发。 | 已定 |
| 5.5 | 只能动态链接：macOS 放 `Iris.app/Contents/Frameworks/*.dylib`，Windows 放主程序同级 `*.dll`。绝不静态链进 `iris`。 | 已遵守 |
| 5.6 | 每个二进制分发附带所用 C 库对应源码、构建脚本、配置、校验值及许可证。源码可作为下载附件随同发行，安装程序和 ZIP 共用该附件。保留动态库替换权。 | 构建和打包脚本强制产出 |
| 5.7 | `deny.toml` 检查 Rust 依赖的安全公告、许可证、依赖选型和来源。 | CI 持续检查 |
| 5.8 | 维护两套平台的 LGPL 构建流水线是**持续成本**，要计入里程碑。 | 已记录 |
| 5.9 | 宽松协议本身**不保证**“只有我能出商业版”：是否需要 CLA 或版权归属安排仍待决。 | 待决 |

## 6. 色彩

| # | 结论 | 状态 |
|---|---|---|
| 6.1 | render API 下 `target-colorspace-hint` 管不到 Iris 的图层，色彩空间由 Iris 自己设置。 | 已定 |
| 6.2 | 视频层标注色彩空间，且与 mpv 输出目标一致：macOS 上图层标 Display P3，mpv 输出 `target-prim=display-p3`、`target-trc=srgb`。 | 已完成；sRGB / BT.709 误差 ≤ 3 色阶，CPU 无变化 |
| 6.3 | 照片内嵌 ICC 逐张覆盖；超出目标色域裁剪。 | 已完成；Windows P3／Adobe RGB 的 PNG／WebP／TIFF 六组截图误差 ≤ 2 色阶 |
| 6.4 | macOS 换屏由 Core Animation 处理；Windows 读取当前显示器 ICC，跨显示器时更新 mpv 配置。 | 已实现并完成 Windows 双屏跨显示器切换实测；广色域/HDR 多屏组合待后续扩展验证 |
| 6.5 | HDR / EDR。 | 计划（后续版本） |
| 6.6 | 长期照片主视图管线是原生图层（macOS：ImageIO + `CALayer` 的 `CGImage` 内容），不是 GPUI 纹理；目前 `ImageIO` 已率先用于底部 160px 内存缩略图预览。 | 主视图原生层计划于后续版本 |

## 7. 照片走 mpv 的修补

| # | 问题 | 做法 | 状态 |
|---|---|---|---|
| 7.1 | 视频层没有色彩管理 | 见 6.2 | 已完成 |
| 7.2 | HEIC 的方向被忽略，竖拍照片横着显示 | 读出 HEIF 的 `irot` / `imir`，用 `vf=format=rotate=…` + `hflip` / `vflip` | 已完成（`photo.rs`） |
| 7.3 | 超过 16384 像素的图蓝屏、着色器报错 | 长边超过 8192 先经 `vf=scale` 缩小 | 已完成（`photo.rs`） |
| 7.4 | 照片内嵌 ICC 被忽略 | 见 6.3 | 已完成 |
| 7.5 | 翻页慢（HEIC 约 300 ms）、无法预解码 | 留给原生照片主视图管线（6.6） | 已记录 |

## 8. 被复核否定的说法（不是共识）

| # | 讨论稿的说法 | 复核结论 |
|---|---|---|
| 8.1 | D3D11 渲染要设 `MPV_RENDER_PARAM_BACKEND = "gpu-next"`，且 libplacebo ≥ 7.387 | `render.h` 没有 BACKEND 参数，是编的。真正要注意的是不恢复设备状态、不能用 SINGLETHREADED。|
| 8.2 | 用 GPUI 的 `window.start_window_move()` 拖窗口 | 0.2.2 在 macOS / Windows 上是空函数。|
| 8.3 | GPUI 视口层自带双指捏合缩放 | GPUI 0.2.2 没有 magnify 事件；已在 `platform/macos.rs` 通过原生 `NSMagnificationGestureRecognizer` 自行桥接。|
| 8.4 | 立刻从 cocoa / objc 迁到 objc2 | GPUI 0.2.2 自己用旧库，迁移后类型不互通；不是当前的事。|
| 8.5 | `deny.toml` 只放行 MIT / Apache / BSD / ISC / Zlib / Unicode | 现有依赖树会直接报红，必须加 MPL-2.0 等。|
| 8.6 | 用 `target-colorspace-hint` 透传 HDR 和色彩空间 | render API 下管不到 Iris 的图层。|
| 8.7 | IINA 漏洞编号是 CVE-2026-5433 | IINA 的是 CVE-2026-47114。漏洞教训本身成立（外源输入与插件消息绝不拼接为 mpv 参数）。|
| 8.8 | `MediaId` 用“文件大小 + 头尾各 64 KB 哈希”做主键，“不到 0.1 ms” | 只在 SSD 且已缓存时成立；改 EXIF 或旋转照片 ID 就变。改为路径主键（3.4）。|
| 8.9 | 内核里定义 `MediaBackend` trait | 抽象层级不对：以后图片不走 mpv 时，要换的是底层画面层，不是内核里的后端接口。不写假想接口。|
| 8.10 | 一个窗口对应一个会话 | 应挂在视口上（3.1）。|
| 8.11 | 打包用 `codesign --deep -s -` | 只是本机自签名，过不了 Gatekeeper，Apple 也不推荐 `--deep`。分发要 Developer ID 签名、开 hardened runtime、逐个签 dylib、公证。|
| 8.12 | “Windows 方案有 90% 以上把握” | 是估出来的数字。缩放不同步只能上机验证（ARCHITECTURE.md 第 4 节）。|
| 8.13 | 放弃 Intel Mac、最低 Win10 1903、商业模式走 Open-Core | 未拍板，列入待决。|
| 8.14 | 长期照片主视图用系统解码后当 GPUI 纹理上屏 | GPUI 图片元素是 8 位 sRGB、无色彩管理。主视图应改用原生图层（6.6）；GPUI 图片元素仅可用于低成本缩略图底条预览。|
| 8.15 | MVP 阶段把滤镜栈仲裁、插件插槽、命令面板、HDR/ICC、反向逐帧全部塞进内核 | 违反分阶段原则。实际按顺序先完成 MVP 与成熟播放器打磨，再以独立 `iris-api` + 控制面/矢量叠加面边界落地插件插槽与命令面板；滤镜栈仲裁、HDR 与反向逐帧仍不塞入当前内核。|

## 9. v0.1 验收范围与扩展交付

- **v0.1 基础内核验收**（2026-11-21）：打开方式、视频精确跳转与重播、照片缩放平移与色彩/方向/超大图修补、音轨字幕、自然排序目录、全屏字幕避让、设置落盘、Windows D3D11 原生合成、LGPL 源码自建 SDK 与未签名本地包。
- **成熟播放器体验完善**（2026-11-22）：可调宽虚拟化目录、搜索/排序/子目录导航、窗内设置中心、60fps 跟手时间轴与章节刻度、连续音量条、音频播放卡片、外挂字幕模糊自动加载与延迟调节、画面比例/旋转/镜像/截图、防休眠、窗口置顶、独立轻量 OSD、右键上下文菜单、媒体详情与 EXIF、照片内存缩略图底条、macOS 原生捏合缩放与媒体键。
- **模块化工作台、全插件化远程数据源与中期工程收敛**（2026-11-22，后于 `0.0.2` 统一演进为 `protocol: 2`）：`PresetManager` + `PresetRuntime`（`1B+` 稀疏补丁与个人基线自动回正）、`Keymap` 多方案与 `!TextInput` 打字安全防火墙、`ThemeTokens` GPUI 全局令牌与无标题栏三重安全护栏、三元 `MediaUri`（`Local | HttpUrl | Plugin`）+ 统一 `CatalogItem` + 独立 `PlayQueue` + 单表 `Store`、慢速共享盘免 `stat` 扫描、`mpv_command_node_async` 零注入远程流直通、`CredentialVault` 系统凭据分桶托管、`photo::probe_bytes` 远程照片色彩与方向探测、音频输出设备切换、画面均衡器、缓冲条/直播状态 UI；原 v1 插件底座与示例已于 `0.0.2` 统一切换为 Wasm `protocol: 2`（官方包 `iris.guides`、`iris.webdav`，原生 `Session` A-B 循环，以及 `examples/plugins/{cinema-ui,watch-stats,virtual-audio,bilibili,baidupan}`）。

## 10. 待决问题

- **版权安排**：许可证已定为 `MIT OR Apache-2.0`；是否需要 CLA 或版权归属安排仍待决（5.9）。
- **macOS 新功能实机走查**：Windows 11 双屏实体机已于 2026-11-23 完成全量 72 项测试、三套 Clippy、原生 D3D11（含连续 3 次设备丢失重置自愈并恢复 `d3d11va` 硬解与看图状态）/跨屏 ICC/WIC 缩略图/窗口控制与 `WM_CLOSE` 落盘/单实例转发、`1B+` 稀疏预设、`iris.webdav`（Windows 凭据管理器）及外部插件/无边框护栏实测核销（见 `TESTING.md`）；macOS 侧的触控板双指捏合、系统媒体键以及中期重构后的 GUI 仍待在实体 Mac 上人工点验。
- **最低系统版本**：打包脚本当前以 macOS 11 为构建目标；实际支持下限仍需对应系统验证。
- **商业模式**：是否走 Open-Core，仍未决定。
- **正式分发签名**：`scripts/release/package.py` 与发行工作流已实现未签名本地包（`--mode local`）及正式签名发布模式（`--mode release`：macOS Developer ID 逐项签名与 `notarytool` 公证装订、Windows Authenticode 签名）；实际产出已签名发行包需提供对应发行证书凭据。
- **跨显示器**：窗口移到另一块屏幕时刷新率和任意多屏 ICC 组合尚未完整验证。
- **控制协议边界**：仅 debug 或显式 `devtools` 构建启用，限定同用户；普通 release 不监听。
- **未复现的问题**：早期一次全新启动后先开图片黑屏的历史记录，此后未能复现。
