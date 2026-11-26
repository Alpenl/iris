# Iris 插件开发指南（protocol: 2 / ABI: 2）

当前源码使用 Wasm 插件协议 2。旧 Python/stdio 插件和 `BuiltinPlugin` 已退役；零调用方的外部工具（BYOB `external_tool` / `export_files`）、DASH 与受限 HLS 文本通道已于 `0.0.2` 精简移除。完整契约、配额与验收项见 [架构规范](PLUGIN_V2_ARCHITECTURE.md)，实际测试和未验证范围见 [实施证据](PLUGIN_V2_IMPLEMENTATION.md)。

## 1. 安装与官方插件

在「设置 → 插件 → 安装插件包」选择 `.irisplugin`，审核宿主显示的身份、摘要、能力和网络范围后安装。宿主先校验不可变包快照和 Wasm，不在批准前执行模块。更新增加能力需重新授权；失败保留已安装版本。禁用/卸载撤销该安装的窗口、流、网络和 UI 贡献，后台资源结束后释放配额。

| 包/功能 | 实现 |
|---|---|
| `iris.guides` | 官方 Rust/SDK Wasm：构图参考线与交互叠层 |
| `iris.webdav` | 官方 Rust/SDK Wasm：多连接、授权 Origin、凭据、分页目录、播放与外挂字幕（经 `SourceDescriptor.badge` 声明 `"WebDAV"` 徽标，宿主零硬编码） |
| A-B 循环 | 原生 Session 能力；不再依赖计时插件 |
| `example.watch-stats` | 窗口计时、状态栏和右键重置；不持久化 |
| `example.virtual-audio` | 独立 StreamStore 的 RandomAccess PCM 示例，无外网请求 |
| `example.cinema-ui` | 纯声明式主题，无 Wasm 实例 |
| `community.bilibili` | 视频源、弹幕、清晰度与 `PixelGrid` + `ViewTimer` 扫码登录示例 |
| `community.baidupan` | 网盘目录与结构化 `VirtualHls` 播放源示例 |

官方包来自 `crates/iris-plugin-guides`、`crates/iris-plugin-webdav`，在构建前生成到 `Resources/Plugins/` 并嵌入宿主。宿主使用编译时摘要验证身份；第三方包不能自称 `iris.*`。官方包遵守同样的能力与资源检查。

用户包存放于应用数据目录 `plugins-v2/`（开发时可用 `IRIS_PLUGINS_V2` 指定受管安装目录）；不要手工修改其中的安装授权记录。状态位于 `plugin-v2-state/`，临时产物位于 `plugin-v2-temp/`。敏感凭据按安装身份保存在系统凭据库，不降级写明文。旧 `plugin_state/` 仅作为官方迁移输入，成功前保留旧资料和凭据；第三方插件不能借同 ID 领取旧凭据。

## 2. 从公开 SDK 开始

```text
rustup target add wasm32-unknown-unknown
python -B scripts/build-example-plugins.py
python -B scripts/test-plugin-v2-examples.py --release   # 默认即为 --release
```

输出为 `target/plugin-v2/examples/*.irisplugin`。源代码见 [examples/plugins](../examples/plugins/README.md)。Rust Guest 使用 `iris-plugin-sdk` 的 `export_plugin!`、`guest` 与 `client`，目标为 `wasm32-unknown-unknown`，无 WASI。C 的最小 ABI 夹具见 `crates/iris-plugin-sdk/tests/fixtures/contract.c`。官方 WebDAV 展示完整异步任务、Origin 确认、凭据、状态保存和目录分页。

包为 ZIP（Stored/Deflate）：根目录 `plugin.json`、可选 `plugin.wasm` 和 `assets/`。可执行 entry 必须为 `plugin.wasm`；纯主题不需要 entry。包校验拒绝路径穿越、链接、重复路径、ZIP64、过大条目及不支持的 ABI。安装容量、压缩/展开限制以架构规范为准。

```json
{
  "id": "example.timer",
  "name": "窗口计时",
  "version": "0.0.2",
  "protocol": 2,
  "abi": 2,
  "entry": "plugin.wasm",
  "permissions": ["read_state", "ui_status"],
  "commands": [{"id":"reset","title":"重置计时","context_menu":true}]
}
```

命令面板、快捷键与当前视口右键共用声明的命令；每插件最多 16 个右键项，菜单总共最多 32 个。右键命令不自动获得文件列表选择或额外权限。没有 `read_state` 时，命令事件的播放快照仍为空。声明式 UI 使用 `Widget`（含通用 `PixelGrid` 矩阵网格）、`ViewTimer`（`1_000..=300_000 ms` 视图级定时回调，适用于二维码状态轮询或看板刷新）、`OverlayNode`、`UiThemeSpec` 和 `ChromeLayoutSpec`；宿主保留对比度、恢复默认外观和无边框逃生入口。插件可无需额外权限发送 `HostCommand::Log { level, message }`，由宿主统一 20 条/秒限流、脱敏并写入结构化日志。

## 3. 运行时、ABI 与异步生命周期

每个安装身份共享一个 ControlStore；宿主按窗口封装事件，最多四个活跃窗口。窗口共享同一插件内存，不是不同插件之间的隔离边界。每路需要 Guest 读取的播放会话有独立 StreamStore，共享已编译 Module；目录或配置等待不占播放 Store。

控制面采用有界、严格 JSON；宿主导入 `host_submit` 立即返回请求 ID，后台完成以事件交付。回调不能阻塞等待网络或用户。64 位 ID/长度以十进制字符串编码，`u64(ptr,len)` 为高 32 位指针、低 32 位长度。消息上限 64 KiB、单次 Blob 1 MiB，错误表及导出签名见 [ABI 规范](PLUGIN_V2_ARCHITECTURE.md#4-abi-v2控制编码字节与所有权)。

Guest 输出和 Blob 需遵循转移/关闭约定；任何跨 Guest 调用保存 `Memory` 借用的实现都不符合契约。`memory.grow` 后必须重新取得范围。短读、EOF、取消各有明确返回值，不能把空缓冲当作未完成轮询。SDK 负责基础编解码与内存交接，但业务请求和资源仍须正确结束。

请求、窗口、媒体、seek、流上下文分别有作用域与代次。普通 seek 取消旧流读取，保留同窗口的目录/配置请求；切集或关闭撤销媒体作用域。晚到结果在交付和应用前复核；取消中的作业仍占账本，直到实际退出。Epoch 是执行中断的墙钟机制，不能代替宿主 I/O 超时与取消。

## 4. 网络、登录和文件权限

网络同时需要传输能力（`net_http`/`net_ws`/`net_tcp`）与目的地授权（`net_hosts` 或 `net_public`）。`net_public` 仅允许公网，不自动允许私网。初始请求、每次重定向、DNS 解析后实际连接及宿主代发流/字幕/缩略图都使用所属插件的策略；公网请求不能借 NAS 授权跳进内网。

NAS 等私有服务经宿主确认具体 Origin 与地址集合。HTTP 明文凭据另行确认，DNS 改变不能悄悄扩大授权。跨源时剥离敏感头；凭据、Cookie、OAuth state/PKCE 均受各自作用域、配额和期限约束。插件表单和域名匹配声明本身不算授权。

`read_state` 仅授予播放快照及当前授权媒体指纹（`HashCurrentMedia`），不授予任意文件读取。`annotations` 仅操作用户选定媒体的应用标记；私有临时产物（`ArtifactBegin` / `ArtifactWrite` / `ArtifactRead`）受配额目录隔离，仅作为惰性数据句柄使用。

## 5. 三种流源与媒体增强

| 流源 | 约定 |
|---|---|
| DirectHttp | 宿主拉取授权 HTTP 流（含远端 HLS 自动重写），校验备用表示与范围语义，可附独立音轨 |
| RandomAccess | Guest 根据 offset/len 提供有界字节，宿主处理 Range/HEAD/EOF |
| VirtualManifest | 结构化 `VirtualHls`；宿主生成仅含本会话能力 URL 的清单 |

原始 `.m3u8` 不能直接交给 libmpv；不支持的标签/格式明确失败，不回退未经清洗文本。会话令牌支持合法的重复 Range，不能跨会话复用。插件只负责字节和元数据，实际解复用、解码和硬件加速依赖媒体库与驱动。

`media_enhance` 允许受限字幕、音轨、监看 LUT 与时间轴元数据。时间轴包含最多 1,000 个章节、256 个热力区间、64 个跳过区间；区间有序、不重叠，最多覆盖 7 天。跳过是用户点击操作，不会因提交元数据自动 seek。

TimedOverlayTrack 使用宿主播放时钟和驻留窗口，默认过去 30 秒至未来 120 秒；每插件至多 10,000 条/4 MiB，窗口外提交返回明确错误。长节目通过窗口分页加载；暂停/空轨道不持续驱动自身动画。标记、评分和颜色写应用数据库，不修改媒体文件。60/120Hz 与 4K 吞吐需另有实测报告。

## 6. 验证与贡献

```text
python -B scripts/check-project.py
cargo test --locked --workspace --future-incompat-report   # 含默认启用的纯 WAT ABI v2 契约测试
cargo clippy --locked --workspace --all-targets -- -D warnings
python -B scripts/test-plugin-v2.py --zig <Zig-0.14.1可执行文件>
python -B scripts/test-plugin-v2-official.py
python -B scripts/test-plugin-v2-examples.py
iris --check-plugin-runtime
```

最后一个命令校验嵌入包并在实际宿主进程执行 JIT/实例化/回调，输出 JSON；不启动 UI 或读取用户插件。CI 在 Windows、macOS Intel/ARM 上运行真实 Guest；发布脚本对签名后的实际产物重复执行冒烟检查。配置了流水线不等于平台已验证，详见实施证据与 [总 issue #2](https://github.com/Alpenl/iris/issues/2)。
