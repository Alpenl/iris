# 媒体增强实现约束

本页记录 `0.0.2`（`protocol: 2 / ABI: 2`）媒体增强子系统的实现约束与边界（原零调用方的 `export_files` / `external_tool` 导出模块已于 `0.0.2` 移除）。

## 私有字节产物

`ArtifactBegin { format }` 返回 `artifact_writer`，随后 `ArtifactWrite { handle, blob, finish }` 每次最多写 1 MiB。同一 writer 只允许一个在途写入；排队失败、取消、关闭与未交付结果均回收 writer。完成后产生只读 artifact；每次读取仍受实例、安装身份和代次检查。产物格式为固定枚举，不接受扩展名、路径或滤镜字符串。

创建/读取私有产物需 `media_enhance` 权限；StreamStore 不开放这些操作。私有产物目录在首次写入时惰性创建，卸载、关闭或取消时由后台回收并归还磁盘/句柄配额，不触碰用户媒体文件或任意目录。

## 批注与选择范围

评分、颜色、时间标记保存在应用数据目录 `annotations.json`。每媒体最多 1,000 标记，整个存储最多 4,096 媒体 / 10,000 标记 / 16 MiB JSON。保存时在独立文件锁下重读并仅合并本实例修改的字段；并行修改保留，损坏文件不被静默替换。

SelectionGrants 只由宿主当前项或明确选择动作建立，每个授权最多 1,000 URI、五分钟期限，并绑定安装身份、控制实例和媒体代次。插件不能通过字符串自选文件或枚举库。批注快照序列化成最多 1 MiB 的计额 Blob，不返回不受账本控制的对象副本。标记修改不写媒体文件。

## 时间窗口叠层

OverlayBank 按媒体/seek 代次、窗口 revision 验证整批提交；窗口外返回 `OUT_OF_WINDOW`，过期代次返回 `STALE_HANDLE`。当前窗口为前 30 秒、后 120 秒，正常播放每 15 秒推进，seek 清空并重发窗口。单 cue 最长 30 秒、文本 1 KiB；矢量 cue 最多 16 节点 / 32 展开元素，无交互按钮。

所有轨道共享每安装身份 10,000 cue / 4 MiB 原始数据上限，宿主结构另计 HostBytes；每身份最多 8 轨道、全局 64。替换轨道先验证再原子替换租约，不能靠开新轨道绕过总额。每帧每身份最多 256 活跃 cue、全局 512，超额显示项计入 `omitted`；已接纳的时间窗口仍完整保留。UI 应展示拥挤提示并允许调整密度。

宿主时钟插值，不逐帧调用 Guest。暂停/缓冲冻结；无活动 cue 不启用动画，仅在下一条开始时唤醒。OverlayFrame 持有布局预算，UI 保留其文本或布局时须同时保留 frame。GPUI 绘制已接入主视口叠层（含文本字形缓存租约与矢量节点渲染），60/120Hz 实测尚未完成。

## 原生 A-B 与 LUT 预览

A-B 通过原生 `ab-loop` / `ab-loop-a` / `ab-loop-b` 属性执行；有界时间范围、非直播、已加载音视频才允许设置。切换/停止清除，属性观察更新 UI，无 Guest Tick 重复 seek。已接入快捷键配置、命令面板、右键菜单、播放栏状态与时间轴端点。默认 Ctrl+Shift+L 依次设 A / B / 清除，Ctrl+Alt+Shift+L 清除；快捷键沿用输入框隔离。

MonitorLut 只接受已经获准的私有 `.cube` artifact：最大 4 MiB，3D size 2..65、有限数值、精确 sample 数量、受限指令。宿主生成固定 `lut3d` 滤镜并分别转义 FFmpeg option、filter graph 和 mpv 参数层，不允许 Guest 指定原始滤镜或着色器。Session 保留 artifact 生命周期，停止/切集清除，翻转/旋转时组合受控滤镜。

这是 CPU 路径的预览变换，视频启用时请求 `auto-copy`；实际解码回退遵循 mpv。它不等于 Stage 3 的显示器 ICC / HDR / EDR 色彩管理，也没有任意素材实时性能承诺。当前 libmpv 输出不能直接套用仅适用于 gpu-next 的 LUT 选项，参见 [mpv 手册](https://mpv.io/manual/stable/)。

## 当前验证与待接入

Windows x64 的实际 libmpv 测试覆盖三次连续 A-B 循环、清除及 seek/停止回收；LUT 测试使用包含空格、逗号、分号、括号和引号的宿主路径，确认原生 RGB 滤镜加载及停止后账本归零。该测试不是视觉色彩校准或帧率验收。

SourceRequests 已校验请求身份、代次、翻页游标、重复条目与浏览取消；缩略图请求被取出执行后仍可随视图取消。控制命令入 UI 队列前验证数值、声明式组件、URL、资源计数，避免畸形数据进入布局。

上述媒体增强路径已随 `protocol: 2` 接入主程序 UI（含官方 `iris.guides` / `iris.webdav` 与示例插件），实际已验证与待验证平台/性能项见 [PLUGIN_V2_IMPLEMENTATION.md](PLUGIN_V2_IMPLEMENTATION.md) 与 [总 issue #2](https://github.com/Alpenl/iris/issues/2)。
