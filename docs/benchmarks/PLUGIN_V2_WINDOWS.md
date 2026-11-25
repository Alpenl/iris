# 插件 v2 Windows 测量记录

这些记录用于建立可复现的局部回归基线，不表示 A15 的跨平台、解码和界面指标全部通过。

## 优化宿主 JIT（2026-11-24）

原始记录：[五次采样 JSON](plugin-v2-windows-jit-2026-11-24.json)。采样结束于北京时间 2026-11-24 23:57:49；生产代码对应原子切换提交 `77a47c7`，采样工作树为 `9c0c859`（额外变更仅为传输测试夹具）。JSON 保存宿主二进制与官方包 SHA-256、实际源码提交、工具链和机器信息。

Windows 11 build 26300、Intel Family 6 Model 154、16 逻辑处理器、Rust 1.100.0；宿主为 `release`。每次启动新进程，依次编译两个真实官方包，分别再次查询内存缓存、实例化并执行 shutdown 回调。未清空操作系统文件缓存；不读用户配置、不联网、不打开 UI。

| 指标 | p50 | p95（nearest rank） |
| --- | ---: | ---: |
| 整个进程墙钟时间 | 1,719 ms | 1,796 ms |
| 整个进程峰值工作集 | 54.68 MiB | 54.76 MiB |
| Guides 冷编译 | 496.90 ms | 538.84 ms |
| WebDAV 冷编译 | 1,060.87 ms | 1,080.21 ms |
| Guides 缓存命中 | 0.35 ms | 0.52 ms |
| WebDAV 缓存命中 | 0.70 ms | 0.85 ms |
| Guides 实例化 | 0.20 ms | 0.24 ms |
| WebDAV 实例化 | 0.16 ms | 0.18 ms |

峰值工作集每 20 ms 查询 Windows 的 OS 累计峰值；CPU 样本可能漏掉退出前最后一小段。仅五个样本时 p95 就是最大值，不是总体分位数估计；没有测量分配次数、播放或渲染帧时间。

首轮冻结同机器、OS、工具链、构建 profile 下的试运行预算：墙钟 p95 ≤ 2,245 ms，峰值工作集 p95 ≤ 71,777,280 bytes（原始 p95 的 1.25 倍）。这只是后续回归比较阈值，不能推广成所有机器的性能承诺。

```text
cargo build --locked --release -p iris
python -B scripts/benchmark-plugin-v2.py target/release/iris.exe --runs 5 --output target/v2-contracts/jit-benchmark.json --baseline docs/benchmarks/plugin-v2-windows-jit-2026-11-24.json
```

运行时须按仓库说明设置 `MPV_LIB_DIR` 和动态库搜索路径。机器或工具链不一致时脚本拒绝直接比较预算。macOS Intel/ARM 签名后 JIT 必须在对应发行环境运行 `--check-plugin-runtime`，Windows 记录不能替代它。

## 4K 素材十分钟传输（2026-11-25）

原始记录：[传输采样 JSON](plugin-v2-windows-4k-transport-2026-11-25.json)，源码 `7680e39`，工作树干净。与上述 JIT 相同的 Windows/CPU/工具链，使用默认 **debug 测试二进制**；没有混用 release JIT 的 profile。

固定素材由 FFmpeg 8.0.1 生成：3840×2160、24 fps、H.264/AAC、十秒、16,252,655 bytes。命令配置 12 Mbit/s 视频 CBR；实际供给预算按 FFprobe 的音视频 packet bytes 计算，以首个 DTS 起的固定一秒分桶测得峰值 2,470,894 bytes/s，目标为其 1.25 倍，即 3,088,617.5 bytes/s。该定义不是滑动窗口或单帧瞬时峰值。素材 SHA-256：`07f8fb30ee3a9e3d37033c219e82a160b030a5c8d2d165bcc79e45fc51bad8db`；工具摘要、编码命令、每次读取和回收记录均在 JSON 中。

测试在一个进程中运行内存 Origin 和真实 `DirectHttp` 授权/密封中继，按预算节奏重复读取完整素材，每次核对 SHA-256。所有连接均走明确确认的回环 Origin；没有绕过生产网络校验。此场景不执行 Wasm 字节转换、解码、GPU 输出或 UI 绘制，不注入 WAN 延迟；内存 Origin 与测试校验器也计入进程 CPU/RSS。

| 指标 | 结果 |
| --- | ---: |
| 持续时间 / 完整读取 | 600.25 s / 115 次 |
| 总传输字节 | 1,869,055,325 |
| 包含节奏等待的供给速率 | 3.114 MB/s |
| 单次读取吞吐 p50 / p05 / 最低 | 54.052 / 42.066 / 37.925 MB/s |
| 传输预算超时 | 0 |
| 测试进程峰值工作集 | 39,243,776 bytes |
| 测试进程累计 CPU 样本 | 29.406 s |
| 采样宿主字节账本峰值 | 7,936,036 bytes |
| 停止至已检查资源回收 | 6.022 ms |

MB/s 使用十进制。一次完整读取若超过“素材大小 ÷ 目标供给速率”才记传输预算超时；它不是播放器实际下溢计数。回收验证覆盖宿主字节、播放会话、中继工作线程和句柄。一次十分钟试运行的 CPU/RSS/回收值没有重复样本分位数。

冻结本场景预算：单次读取供给不少于 3,088,617.5 bytes/s、零预算超时、相同设备/profile 的进程峰值工作集不高于 49,054,720 bytes（本次值 × 1.25）。源码测试检查前两项，JSON 保存内存预算供后续对比。

```text
python -B scripts/test-plugin-v2-transport.py --ffmpeg <verified-ffmpeg> --ffprobe <verified-ffprobe>
```

这补充了 A15 的本地 DirectHttp 传输证据。真实公网 CDN、长直播、Wasm 解密转换吞吐、4K 持续解码及 60/120Hz 界面仍须各自测量，不能从本结果外推。
