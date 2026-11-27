# 2026-11-24 · 插件系统 v2 战略转向与决策记录

> **状态：源码已切换 protocol: 2 并于 `0.0.2` 完成精简重构与沙箱加固（Wasmtime 切换为 `pulley64` 只读字节码解释执行、主进程零 JIT 可执行内存豁免；协议整数拆分为句柄 `Id` 与数值 `WireU64`；跨平台官方插件字节级确定性复现；移除零调用方的外部工具 BYOB、DASH 与受限 HLS 文本通道，收敛为 `VirtualHls`、`PixelGrid` + `ViewTimer`、`SourceDescriptor.badge` 与 `HostCommand::Log`）。** 已实现产品行为以 [PLUGINS.md](PLUGINS.md) 与 [ARCHITECTURE.md](ARCHITECTURE.md) 为准。
> 本文记录方向、取舍及复审结论；接口、配额、错误语义、迁移顺序与验收矩阵以 [PLUGIN_V2_ARCHITECTURE.md](PLUGIN_V2_ARCHITECTURE.md) 为准。开发入口的实现和验证记录见 [PLUGIN_V2_IMPLEMENTATION.md](PLUGIN_V2_IMPLEMENTATION.md)；未记录通过的契约仍待验收，性能目标不是实测结果。

## 一、为何提前建设插件 v2

旧路线把远程存储与生产力功能放在插件沙箱之前，容易让新增连接器继续进入 `iris-core`。现有外部运行方式还要求用户拥有脚本解释器，或由作者分别构建、分发原生程序；解释器探测、签名、公证和用户环境都会影响交付，不能假定所有机器有相同环境。

继续遵循 [DECISIONS.md](DECISIONS.md) 的首发前收敛单版本协议原则：在没有对外稳定兼容承诺的阶段准备新底座，待替代物验收完成后一次切换，不发布长期并存的两套插件协议。

项目保持路线 B：主程序 `MIT OR Apache-2.0`，媒体动态库按仓库的 LGPL 构建与分发流程交付。外部插件有助于独立扩展和分发，但运行时选择、动态加载或 BYOB 本身都不能替代依赖及分发审查。

**决议：优先完成插件 v2 契约、运行时与官方连接器自举，再扩张连接器生态。**

## 二、生态优先级

1. **远程存储与媒体服务连接器**：WebDAV、OpenList、S3/R2、SMB/SFTP、Emby/Jellyfin/Plex、Immich、云盘与在线音视频服务。
2. **实用扩展**：字幕搜索/翻译/转写、时间轴弹幕与歌词、审片标记导出、切片、同步观影、观看进度同步。
3. **外观扩展**：保留现有声明式控件、主题与布局能力；主题改为单一激活槽，避免多插件按字典序混合覆盖。新增视觉能力服务于媒体工作流，不开放实时像素管线或任意原生 UI 代码。

这些是能力方向，不构成对某网站、任意编解码格式或任意硬件的兼容保证。

## 三、运行时与算力分工

### 3.1 运行时分工与边界

| 层次 | 拍板方案 | 边界与依据 |
|---|---|---|
| 插件逻辑、协议解析与重封装 | `wasmtime` `pulley64` 内存字节码解释执行，控制与播放实例分开 | 跨平台分发 Wasm 字节码并在后台编译为只读 `pulley64` 字节码（零 `PROT_EXEC` / `MAP_JIT` 可执行页）；编译、实例化、执行都离开 UI 线程，接受宿主能力、配额与执行预算限制。 |
| 哈希、随机数、对称密码学与私有产物 | 有界宿主原语 + 轻量 `FileExecutor` | 可以利用可用的原生实现和硬件加速；本地文件哈希是单独授权的异步作业，不能借纯计算接口读取任意文件。原第三层 BYOB 外部原生工具适配器已于 `0.0.2` 移除。 |

不继续扩展 JSON-lines 子进程作为二进制数据面，原因是分发和传输复杂度；不能据此断言任何子进程方案都无法播放 4K。QuickJS/Luau 不作为本轮路线，也不以未经同机测试的体积、性能数字否定它们。

#### 3.1.1 Wasmtime `pulley64` 对比 Cranelift JIT 与独立 Helper 进程评估（#14）

主进程同时链接 FFmpeg / libmpv 并解析不可信远端流，若开启 `com.apple.security.cs.allow-unsigned-executable-memory` 会削弱 macOS Hardened Runtime 的 W^X 纵深防御。我们在同一台机器（Windows 11 26300 / Intel Family 6 Model 154 / 16 LP / Rust 1.100.0 `release`）对比实测了 Cranelift 原生 JIT 与 Wasmtime 内置 `pulley64` 字节码解释器：

| 负载 | Cranelift JIT | Pulley (`pulley64`) | 结论 |
|---|---:|---:|---|
| 整机烟测墙钟 p50 / p95（5 次独立冷进程） | 1,719 ms / 1,828 ms | 2,344 ms / 2,485 ms | 后台一次性冷编译，不阻塞 UI |
| 整机峰值工作集 p50 / p95 | 54.86 MiB / 55.11 MiB | 49.96 MiB / 50.05 MiB | `pulley64` 降低约 5.0 MiB 峰值内存 |
| `iris.guides`（517 KiB）编译 / 实例化 / 1,000 次 `iris_on_event` | 637.2 ms / 0.33 ms / 4.14 ms | 571.3 ms / 0.16 ms / 26.45 ms | 单次调度 26.5 µs |
| `iris.webdav`（1.09 MiB）编译 / 实例化 / 1,000 次 `iris_on_event` | 1,404.9 ms / 0.17 ms / 1.91 ms | 1,133.8 ms / 0.12 ms / 29.65 ms | 单次调度 29.6 µs |
| `community.bilibili`（1.61 MiB）编译 / 实例化 / 1,000 次 `iris_on_event` | 1,301.6 ms / 0.11 ms / 2.41 ms | 1,395.7 ms / 0.12 ms / 34.24 ms | 单次调度 34.2 µs |

独立签名 Plugin Host Helper 进程方案对比：即便拆分出 Helper 进程，Helper 自身仍需持有未签名可执行内存权限，且须为 `ControlStore`、`StreamStore`、Blob 句柄表、私有产物与网络流跨越进程 IPC 重建全套流控、孤儿进程清理及双二进制签名/公证/安装包校验，工程复杂度与故障面显著上升，而每次控制事件仅节省约 25–32 µs（媒体解复用、解密哈希与硬解均已在宿主原生层执行）。

**决议：直接切换 `RuntimeEngine` 目标为 `pulley64`（只读内存 `PROT_READ` / `PAGE_READONLY`），从 `Resources/Iris.entitlements` 移除 `com.apple.security.cs.allow-unsigned-executable-memory`（保持空 `<dict/>` 并在打包阶段强校验签名 entitlements 完全一致）。**

`iris-api` 的常规依赖仍仅限 `serde`；`iris-core` 不直接依赖 `tokio`。选用同步 Wasmtime 调用接口，宿主 I/O 通过任务和完成事件异步化；“不直接依赖 tokio”不等于禁止异步协议。新增依赖按实际版本、特性和传递依赖运行许可证门禁。

### 3.2 删除磁盘 AOT 缓存

使用安全编译 API，从经过校验的 Wasm 在后台编译；已编译 `Module` 在有界内存缓存中复用。首版不保存、不加载预编译机器码，不调用 `Module::deserialize*` 或信任预编译文件的加载入口。

该决议减少缓存兼容性和可信机器码加载边界，不基于“编译只需 5–15ms”的假设。编译、实例化的 p50/p95、峰值内存和冷启动开销均需实测；不能把宿主使用安全 API 表述为运行时、JIT 或整个应用内部均无 unsafe。

### 3.3 双实例与事件化 I/O

每插件一个 `ControlStore`；需要插件处理字节流的每个播放会话拥有独立 `StreamStore`，复用编译后的 `Module`，不共享 Guest 线性内存。

两种实例均通过“提交宿主任务 → 立即返回操作 ID → 完成事件 → 有界 Guest 计算”访问网络与文件哈希。播放侧也必须支持音视频、清单刷新、拖动请求的并发等待，不能在唯一 StreamStore 内同步等网络。

播放与控制使用独立有界队列、保留播放执行容量，并受全局公平调度约束。实例分离消除直接串行阻塞，不保证共享 CPU、内存、网络或凭据刷新失败时播放完全不受影响。

初始流上下文只能含必要数据及能力引用；支持有版本的地址、密钥、鉴权更新。控制实例普通异常时，流可使用仍有效的上下文继续工作；无法刷新时明确报错。停用、卸载或撤销授权必须级联取消有关实例和资源。

取消遵循“撤销能力、拒绝新请求 → 中断 I/O 与 Guest 执行 → 后台等待在途调用退出 → 释放资源”。UI 不阻塞等待析构。Epoch 是粗粒度时间中断；Fuel 可作为执行量预算，两者都不是精确 CPU 秒数，也不能代替宿主 I/O 超时。

### 3.4 显式 `PluginHost`、单窗口收敛与 `Inbox` 单锁快照（#13）

1. **移除进程级可变单例**：废除 `static HUB`、`Resources::global()` 以及播放器侧 `NEXT_COMMAND_RECEIPT` / `ACTIVE_WORKERS` 全局可变静态变量。应用在启动时显式构造 `PluginHost` 并传入窗口，退出与析构通过 `PluginHost::shutdown` / `Drop` 按序回收，单元测试与集成测试各自独立持有隔离的宿主与播放器实例。
2. **多窗口收回到单窗口会话**：依据 [DECISIONS.md](DECISIONS.md) §3.1，Iris 的同屏对比采用**单系统窗口内的多视口模型**，而非打开多个系统窗口。原先运行时提前支持的 4 窗口并发多路复用（`Hub`、`Control`、`ControlServices`、`AuthExecutor`、`StorageExecutor`、`Mailbox` 各维护一张窗口表）在无多窗口产品需求下增加了生命周期与背压复杂度。决议将宿主运行时收敛为**单一活跃窗口会话**（并发打开第二个窗口直接拒绝）；线上协议保留单调递增的 `window: Id` 会话纪元令牌与 `WindowOpened` / `WindowClosed` 事件，用于关窗重开时的旧句柄隔离并保持已发布 ABI v2 Guest 零改动兼容。
3. **`Inbox` 单锁状态快照**：将窗口 `Inbox` 原有的 7 把互斥锁与 2 个原子布尔合并为单一 `Mutex<InboxState>`，使 `(generation, access, cancelled, closed)` 读取天然构成一致快照，并消除 `Client::poll()` 中的跨锁顺序隐式约定。

## 四、四项复审抉择的最终结论

### 4.1 外部工具：已于 `0.0.2` 精简移除

早期规划曾考虑版本化、强类型的 BYOB 外部工具适配器（方案 A，涵盖 FFmpeg/Whisper/yt-dlp）与完全信任本地命令（方案 B）。鉴于零生产插件调用且维护子进程代理/工具绑定复杂度高，`0.0.2` 精简重构已将 `external_tool`、`export_files`、`tools/` 与 `process_tree/` 整体移除，宿主仅保留轻量的 `FileExecutor` 私有临时产物读写（`ArtifactBegin`/`ArtifactWrite`/`ArtifactRead`）和 `HashCurrentMedia`。未来若确实引入外部工具能力，须重新立项并独立完成安全边界与 OS 隔离验收。

### 4.2 网络：协议能力与目的地授权正交

采用 `net_http / net_ws / net_tcp` 协议能力，与以下目的地授权组合：

- `net_hosts`：限定公网域名/后缀与允许的协议、端口。
- `net_public`：允许相应协议范围内的公网目标，满足动态 CDN 与通用网页解析需求；需宿主明确展示并取得授权。
- `PrivateOriginGrant`：宿主可信交互确认的私有服务 Origin、端口与锁定地址集合；插件表单的字符串不自动生成授权。

所有 RawHttp、WebSocket、RawSocket、主备流、音轨、字幕、缩略图、远程探测和清单子请求都携带调用者身份与请求级网络作用域。初始 URL、重定向、DNS 结果及实际连接统一检查；公网请求不能借用同一插件另一个 NAS 授权跳入私网。

目的地权限不授予凭据转发权限。凭据按 Origin/资源绑定，跨 Origin 默认去除敏感及未明确允许转发的头。地址变化需重新授权，撤销要取消在途请求。宿主中继和 OAuth 使用专属回环能力，不开放插件的通用回环网络权限。

“公网”是规范化地址和连接策略，不等于物理网络之外；全球单播 IPv6 或端口映射可能指向本地设备。因此不承诺“100% 隔绝所有本地服务”。`net_public` 也允许上传插件已获准读取的数据；未授予 `read_state` 仅保证不提供完整播放快照，其他数据能力必须分别审计。

### 4.3 虚拟清单：结构化 `VirtualHls`，所有引用重新授权

保留 `DirectHttp / RandomAccess / VirtualManifest` 三模；`VirtualManifest` 收敛为结构化 `VirtualHls`（同时支持远端 HLS 经同一受控模型解析与重写；零调用方的 `VirtualDash` 与受限 HLS 文本通道已于 `0.0.2` 移除）。未经校验的原始 M3U8 不直接进入解复用器。

结构化模型必须覆盖轨道、初始化片段、时间基准、时间线不连续与直播窗口；不把“分片 ID + 时长”当作完整流媒体规范。支持子集及能力发现明确列出，缺少必要语义时返回不支持，不回退原始清单。

每个子资源都映射到当前会话绑定、可撤销、可重复读取的资源令牌。密码学 nonce 唯一不等于 URL 只能消费一次；重试、Range 和拖动需要重复访问。协议/格式白名单是纵深限制，`access-references=no` 不能代替资源验证和授权。

密码学与字节管道提供传输、解密和重封装能力；是否可播放、可硬解仍取决于输出容器、解复用器、解码器、硬件与驱动。硬解不可用时按播放器能力回退软解或报错，不承诺所有平台内容均可播放。

### 4.4 ABI、宿主资源账本与验收成为切换前置条件

ABI 明确 wasm32 内存模型、UTF-8 JSON 控制编码、`u64 (ptr, len)` 布局、所有权、错误表、异步操作、短读、EOF、取消与过期句柄。数据面使用有界裸字节，不做媒体 Base64 封装；跨异步边界使用宿主持有的缓冲，禁止跨 Guest 调用保存 Memory 切片，不能声称全链路零拷贝。

除了 Wasm 内存，宿主统计实例总量、缓冲、句柄、队列、临时文件和叠层驻留量。10,000 条 / 4 MiB 是每插件叠层驻留窗口的默认预算，不是整部影片总量上限；直播与点播均支持窗口更新、分页、拖动重载，超限明确背压，不静默丢失数据。

超时、取消、越权、资源耗尽、清单逃逸与迁移回归必须有可复现测试。现有工程门禁通过仅说明现有工程约束通过，不说明 v2 接口已经实现或性能达标。

### 4.5 协议编码语义拆分、插件敏感键声明与 WIT / Component Model 重评条件（#18）

1. **句柄 `Id` 与数值 `WireU64` 分离**：协议层将能力句柄/请求标识/单调版本号保留为 `Id`（宿主能力句柄额外满足 `1..=i64::MAX`），将字节大小、偏移、长度、毫秒时间戳/时长、码率与序列号迁移至语义明确的 `WireU64`。两者在 JSON 线上均采用规范无符号十进制字符串表示（避免 IEEE-754 精度丢失且完全保持线上兼容）。
2. **去硬编码脱敏**：宿主 `logging::redact` 移除针对单一站点（如 Bilibili `sessdata`、`bili_jct` 等）的硬编码 Cookie 名，改由双保险机制覆盖：（a）插件在 `plugin.json` 的 `sensitive_keys` 字段声明专属敏感键名，包加载时自动注册到宿主脱敏器；（b）宿主 `CredentialVault` 在读写机密时自动追踪机密值（≥ 8 字节），即使日志中未携带可识别键名也会按值抹除。
3. **WIT / WebAssembly Component Model 重评触发条件**：当前保持“有界严格 JSON 控制面 + `Id`/`WireU64` + 裸字节 `Blob` 句柄表”以维持 `iris-api` 仅依赖 `serde`、支持无 `wit-bindgen` 的轻量 C/Zig/Rust Guest。仅当满足以下任一条件时启动 WIT / Component Model 迁移重评：
   - 官方同时维护 2 套及以上非 Rust 语言插件 SDK，且手写 JSON / `WireU64` 契约同步出现重复偏差；
   - 新增双向流式类型化资源接口，导致控制面 JSON 编解码在实测播放/控制路径 CPU 占比超过 5% 或手工胶水变体超过 30 个；
   - Wasmtime Component Model 在 `pulley64` 零可执行页目标下的体积、冷编译耗时与无 `tokio` 同步宿主调用开销均满足同机回归预算。

### 4.6 官方插件跨平台逐字节可复现构建基准（#19）

实测表明，在钉死 `Rust 1.100.0` 并通过 `--remap-path-prefix` 完整消除宿主工作区、Cargo 注册表及 `rust-src` 路径与分隔符差异（Section 11 `.rodata` 字符串完全一致）后，不同宿主目标二进制（`x86_64-pc-windows-msvc`、`aarch64-apple-darwin`、`x86_64-apple-darwin`、`x86_64-unknown-linux-gnu`）的编译器与链接器在 Wasm Section 3/9 的内部单态化函数表顺序上仍存在宿主相关排序差异。

因此采用“**唯一规范构建目标（`x86_64-unknown-linux-gnu` 固定容器）+ 全平台逐字节校验**”方案：

1. `scripts/build-official-plugins.py` 将 SHA-256 锁定的 `linux/amd64` 镜像（`rust:1.100.0-slim-bookworm@sha256:5d3170f8660a07782d5ff42abc55601d2df51d49e5397a8f22247341a658e128`）定为唯一规范构建环境：Linux `x86_64` 直接原生构建，macOS（Intel / Apple Silicon）与 Windows 自动调度 Docker/Podman `linux/amd64` 容器（Windows 无容器时自动回退 WSL2 `x86_64`），任何开发主机均可重建出与仓库逐字节一致的 `.irisplugin` 产物。
2. 移除 `--verify-embedded` 的平台跳过分支：`check.yml` 与 `release-build.yml` 先在锁定 `linux/amd64` 容器内从源码重编并逐字节比对 `Resources/Plugins`，再将重编产物分发至 macOS（ARM64 / Intel）与 Windows 节点执行逐字节归档比对、本机无宿主路径泄漏编译检查及 `pulley64` 运行时自检。

## 五、存量内置插件：二拆一升与原子切换

- `iris.webdav` 与 `iris.guides` 迁为随包外部插件，使用相同宿主 API 和权限规则；官方身份仅用于来源验证、命名空间和受限凭据迁移，不提供运行时绕权。
- A-B 循环进入 `Session`，由 libmpv 原生属性实现；移除插件 Tick 轮询依赖，具体精度、边界和无缝程度通过媒体夹具验证。
- 星级、色标、时间点/区间标记进入 `Store`；新增通用源图标字段，清理 WebDAV UI 特判。迁移覆盖连接配置、稳定 source/media ID、收藏/续播、禁用状态及凭据，不能只迁密码。
- 并列开发运行时、SDK、官方包和示例期间保持产品 `PROTOCOL_VERSION = 1`；v2 测试入口不成为发布的双版本接口。
- 替代物通过验收后，在一个切换变更中同步协议号、示例、包发现/发布打包、门禁和文档，移除 `BuiltinPlugin` 及旧进程运行时。初次设计修订没有执行切换；后续实施已将上述变更作为一个源码切换提交，发布仍须完成平台验收。

## 六、文件保护与能力范围

Wasm 不获得任意本地文件读写句柄。当前媒体哈希（`HashCurrentMedia`）仅由宿主读取当前获准媒体并返回摘要；私有临时产物（`ArtifactBegin` / `ArtifactWrite` / `ArtifactRead`）位于按安装身份隔离、惰性创建的受配额目录，受取消和卸载清理约束，不触碰用户源文件。

字幕、LUT、图片、清单和 UI 数据按格式及累计资源检查；异步增强结果绑定媒体代次，切集后拒绝旧字幕、旧叠层和旧目录响应。暂停或空轨道应停止叠层自身持续重绘，其他 UI 事件与后台任务仍可唤醒应用。

## 七、性能、发布与执行跟踪

保留插件优先、连接器生态、HDR/原生照片管线、多视口与平台扩展的演进顺序。路线图中的版本是规划目标；现有仅允许 `0.0.x` 的门禁在取得对应发布授权前保持不变。

删除未测的 JIT 毫秒数、实例化微秒数、解密 CPU 百分比、任意格式必硬解、零延迟、内存瞬间归零及普遍免系统拦截承诺。macOS JIT 权限、签名、公证、Windows 安装与各架构行为均纳入实际构建验收。

实施任务及依赖见 [架构规范的实施跟踪](PLUGIN_V2_ARCHITECTURE.md#implementation-tracking) 与 [总跟踪 issue #2](https://github.com/Alpenl/iris/issues/2)，已拆为 #3–#9 七个实施任务。只有对应验收证据完成后，任务才可标记为实现或通过。
