# 测试与实测记录

## 构建

```bash
brew install mpv                 # 开发用的 libmpv（GPL 构建，不能拿去分发）
cargo build --release --features devtools  # 性能/控制测试专用，不能作为发行包
cargo test -p iris                        # 自然排序、照片解析、ICC、几何与字幕安全区
cargo deny check licenses        # 依赖许可证检查，需要先 cargo install cargo-deny
```

libmpv 不在 `/opt/homebrew/lib` 时用 `MPV_LIB_DIR` 指定。

## 测试素材

两个脚本都把素材写到 `test-media/`（已忽略，不进仓库）。

| 脚本 | 内容 | 依赖 |
|---|---|---|
| `scripts/make-test-media.sh` | 1080p60 H.264、4K30 HEVC 10bit、带中文字幕的 2.39:1 MKV、几张大照片 | ffmpeg |
| `scripts/make-photo-test-media.sh` | `test-media/photo/` 下的照片边界素材：EXIF 方向 1/3/6/8 的 JPEG 和方向 6 的 HEIC；48 MP 分块 HEIC；无标签 / sRGB / P3 / Adobe RGB 的纯色块；20000×3000、4000×40000、12000×12000 的巨图；JPEG 和 HEIC 各 20 张约 48 MP 的翻页图（每张宽度不同，用来识别是哪一张）| ffmpeg、exiftool、sips、swift（Command Line Tools）|

## 照片实测脚本

`scripts/photo-harness.py` 启动一个**独立的** Iris 测试实例（私有 Unix socket／Windows 命名管道，独立 `IRIS_SETTINGS`），不会连接用户已有窗口。它会把窗口调成 1440×900，然后用 `open` / `get` / `state` / `snap` / `key` 跑测试。不要在运行中操作这个测试窗口，否则输入会改变测试状态。

```bash
cargo build --release --features devtools
scripts/make-photo-test-media.sh
python3 scripts/photo-harness.py out/ exif heic icc huge flip
python3 scripts/icc-report.py out/
```

- 测试项：`probe`（冒烟）、`exif`、`heic`、`icc`、`huge`、`flip`，可任选几个。
- 输出：`out/results.json`（每次运行追加一行 JSON）、`out/<测试>-<文件>.png` 截图、`out/iris-stderr.log`。
- `--bin PATH` 可以换一个二进制，用来对比改动前后（例如先把旧版复制出来）。
- 翻页延迟的计时截止到 mpv 报出新文件的 `video-params` 为止，不含大纹理上传和上屏，真实延迟只会更长。
- `scripts/icc-report.py out/` 读取 `icc` 测试的截图，取视频区中心 100×100 像素的平均值，和 ColorSync 从源配置文件转换到截图内嵌的显示器配置文件的预期值比较。误差在 3 个色阶左右算一致。需要 numpy 和 Pillow（`pip3 install --user numpy pillow`）；Pillow 装在用户目录时不要用 `python3 -I`。
- Windows 的 `snap` 是可见桌面矩形截图，窗口必须置顶、未被遮挡；不要将混入其他窗口的截图当成色彩或布局通过证据。`icc-report.py` 依赖 macOS ColorSync，Windows 不运行它来冒充系统色彩验证。

## 调试控制管道

见 README 的“调试控制管道”一节，命令在 `crates/iris/src/debug.rs` 和 `crates/iris/src/ui.rs`。

**驱动别人正在用的窗口之前，先发 `state` 看当前状态**：用户可能正在用那个测试窗口，合成的点击和按键会打断他。能用独立实例（如上面的实测脚本）就不要驱动用户的窗口。

## 实测结果

环境：Apple Silicon，P3 屏，release 构建，窗口 1440×900，Homebrew mpv 0.42。

### 照片边界（2026-11-20）

| 项目 | 结果 | 说明 |
|---|---|---|
| JPEG 的 EXIF 方向（1/3/6/8） | 通过 | 四种方向都正确 |
| HEIC 分块（48 MP） | 通过 | 完整拼出 8064×6048 |
| HEIC 方向 | 修复后通过 | 修复前：文件带 90° 旋转标记，mpv 报告 rotate 0，竖拍照片横着显示。`video-rotate=90` 对分块 HEIC 也无效（JPEG 有效）。修复后用 `vf=format=rotate=90`，`video-out-params/rotate` = 90，截图与 EXIF 方向 6 的 JPEG 一致 |
| 超过 16384 像素 | 修复后通过 | 修复前：20000×3000 和 4000×40000 只显示蓝屏，mpv 报着色器错误；12000×12000 能显示，但有创建纹理 / 帧缓冲失败的报错，内存约 760 MB。修复后长边超过 8192 的图先缩小：三张都正常显示，mpv 无报错；加载 20000×3000 约 1.1 s、4000×40000 约 2.9 s，进程内存最高约 1.07 GB（解码整图时） |
| 翻页延迟（48 MP，19 次） | 偏慢 | JPEG 中位数 88 ms，偶尔 300~440 ms；HEIC 稳定在约 300 ms。对照：系统 ImageIO 把同样的 HEIC 解码并缩到屏幕尺寸约 100 ms |
| 连按 10 次翻页 | 通过 | mpv 丢掉中途没解完的图，不会越积越多 |
| 内嵌 ICC | 通过（修复后） | 见下 |

### 色彩（2026-11-21）

- **修复前**：视频层没有标注色彩空间，系统把 mpv 的输出当成屏幕原始值。不管图片带什么配置文件，像素值都一样；sRGB 纯红显示为 (252,0,0)，应为 (234,51,35)。所有 BT.709 视频在 P3 屏上都偏艳。同一窗口里 GPUI 界面的颜色是对的。
- **修复后**（视频层标 Display P3，mpv 输出 `target-prim=display-p3`、`target-trc=srgb`）：sRGB 和无标签内容与预期值相差不超过 3 个色阶，例如红色测得 (231,50,35)，预期 (234,51,35)。1080p 测试画面改动前后并排比较，色条不再过饱和。
- **视频层修复后的遗留**：带 P3 / Adobe RGB 配置文件的照片被当成 sRGB，显示偏淡（P3 红测得 (231,50,35)，应为 (255,0,0)）。修复前这类照片在 P3 屏上碰巧是对的，因为两个错误互相抵消。
- **逐张覆盖原色后**（`crates/iris/src/photo.rs` + `icc.rs`）：21 个色块（PNG / JPEG 测试素材，另加 sips 生成的 P3 / Adobe RGB HEIC、P3 TIFF 和带 nclx 的 HEIC）与 ColorSync 预期值相差都不超过 3 个色阶，例如 P3 红 (252,0,0) 对 (255,0,0)。Adobe RGB 纯绿超出 P3：mpv 默认映射测得 (0,241,105)，改为 `gamut-mapping-mode=clip` 后 (0,249,45)，预期 (0,250,45)。EXIF 方向和翻页延迟（JPEG 中位数 89 ms、HEIC 296 ms）不变；照片之后打开视频，`vf` 已清空。
- **未实测**：WebP（本机 FFmpeg 没有 WebP 编码器，做不出样图）；放在网络盘上的照片（配置文件在界面线程读取）。

### 视频 CPU（色彩修复前后，同一脚本、同一窗口尺寸）

| 片源 | 修复前 | 修复后 | 掉帧 |
|---|---|---|---|
| 1080p60 H.264 | 30.7% | 31.6% | 0 |
| 4K HEVC 10bit | 22.3% | 22.3% | 0 |

两边都是 videotoolbox 硬解，差别在噪声范围内。README 里的 24% / 17% 是 2026-11-20 用另一种测法得出的，不能和这张表直接比较。

### 未验证 / 未复现

- macOS 本轮修改后缺少实机环境；既有历史记录保留，但不能当成本轮新代码的复验。
- 一次全新启动后先开图片黑屏的历史记录：Windows 本轮有独立新进程首图验证；不等于 macOS 已复现／解决。
- HDR、Windows ACM 与任意显示器 ICC 组合未做完整验证；不宣称 HDR 正确。

### Windows 基本操作与合成（2026-11-21）

环境：Windows 11 x64，Rust 1.100.0，MSVC Build Tools；独立测试进程，私有控制端点和设置文件。

| 场景 | 当前证据 |
|---|---|
| 原生视频层 | 实际 `current-vo=libmpv`，`hwdec-current=d3d11va`，1280×720 H.264 时间前进；D3D11 层在 GPUI 下方可见 |
| 启动 | 首轮发现缺少 Common Controls v6 导致 `TaskDialogIndirect` 在 main 前加载失败；嵌入应用 manifest 后正常 |
| 窗口 | resize、最大化、全屏／Esc、边框实际鼠标拖拽逐帧截图；未见明显错位和异常强调色底色，退出码 0。不保证所有硬件或每一帧严格同步 |
| 精确跳转 | 暂停在 2.000000，右键到 7.000000，左键回 2.000000；素材关键帧间隔 6 秒 |
| EOF 重播 | `eof-reached=yes` 后空格，位置 0.200000，`pause=no` |
| 音轨／字幕 | A 从音轨 1 切至 2；J/V 正常处理 |
| 字幕安全区 | 全屏工具栏显示时 `sub-margin-y-offset=77`，截图字幕位于工具栏上方；退出全屏恢复 0 |
| 照片交互 | 键盘／滚轮缩放、实际鼠标拖拽（pan-x=0.04、pan-y=0.025）、0 归零、空格切换 1:1；目录滚轮前后 zoom 均 0.915936 |
| 切回视频 | 照片左键翻到上一视频，`vf` 空、`video-zoom=0`，D3D11VA 恢复 |
| 控制流 | Windows 命名管道同用户连接与重连已实际使用；普通 release 的关闭验证另见分发验证记录 |

补充精确焦点验证：实际点击音量第 3 格后 `volume=30`，空格令 `pause=yes`，音量仍为 30；未重复触发音量点击。实际拖动自绘标题栏，窗口左上角从 (120,100) 移到 (180,130)，正常退出。原始日志在 `target/focus-proof/`。

当时截图与原始日志位于忽略目录 `target/interaction/`、`target/features/`、`target/composition/`。它们是本机证据，不随源码提交。早期 GPL 第三方开发 DLL 只用于首轮开发；发行包必须切换到自建 LGPL SDK。

### 自建 LGPL SDK 与照片回归（2026-11-21）

Windows 媒体库已在 WSL 中从固定源码实际编译：mpv `d11ffba253def00b5172c4b8b47e6b2eb765cae1`、FFmpeg 8.0.1、dav1d、zlib、libass、libplacebo、Little CMS 等。六个 FFmpeg DLL 的运行时许可 API 均返回 `LGPL version 2.1 or later`；配置关闭 GPL、version3、nonfree、Chromaprint，mpv 关闭 GPL/Lua，只动态链接媒体库。PE 依赖图没有 FFTW、libgcc 或 libstdc++ 动态依赖。

- 完整 SDK：`target/lgpl-media-sdk/`；对应源码 `compliance/corresponding-source.tar.gz` 为 335204190 字节，SHA-256 `61eaaa981810857cd0a71e8868c6214565e8ec9147b265ae3618f91c9aa39753`。
- **13 个单元／回归测试通过**，使用最终自建媒体库。新增几何测试覆盖光标锚点、平移边界、复位；字幕安全区测试等待实际观察属性变化，验证 106 → 0，避免把异步排队当成功。
- 实际启动自建库时发现无 Lua 构建不包含 `osc` 等选项；现在仅允许这组脚本选项返回“编译时不存在”，其他初始化错误仍失败。
- 最终自建库独立窗口照片回归通过：JPEG EXIF 方向 6、带 `irot` 的 HEIC 方向 6（输出 rotate=90）、1000×750 HEIC、8064×6048 HEIC、20000×3000 PNG、4000×40000 PNG、12000² JPEG、sRGB PNG／WebP。都有真实截图；巨图经过 `scale`，无蓝屏，进程正常退出。
- Windows 实际读取系统显示器 ICC：`sRGB Color Space Profile.icm`。目前该显示器为 sRGB 配置；不能由此宣称所有广色域／ACM 组合均已校准。
- HEIC 硬解在本机不支持对应 still-picture 形态，mpv 尝试后使用真实软件 HEVC 解码（`hwdec=no`），并非伪造画面；日志保留硬解拒绝信息。
- 输出位于 `target/final-photos/`。记录中的 elapsed 包含等待与截图，不用作翻页性能数字。早期长时间 harness 被外部窗口操作干扰的结果不计入通过证据。
- 补充 Windows 真实色彩表面验证：固定源码 Little CMS 生成 P3／Adobe RGB 配置文件，PNG／WebP／TIFF 各一张；窗口像素与 Pillow/Little CMS 转到本机 sRGB 的预期值最大误差为 0／1／0、1／2／1 色阶。使用最终自建 DLL，截图和结果在 `target/icc-proof/`；不是 ColorSync，也不推广为任意 ICC 全覆盖。
- AVIF 单独首图启动验证：800×600，`video-codec=Alliance for Open Media AV1`、本机 `hwdec=d3d11va`，正常退出。SDK 的 dav1d 软件解码能力另由配置门禁要求，不把硬解结果当成软件性能数据。
- macOS arm64 / Intel 的构建配方和工作流已实现，但本轮没有 Mac，未执行；Developer ID、公证和 Authenticode 实签均未执行。用户确认当前只产未签名本地包。

### 未签名发行版验证（2026-11-21）

已用 `package.py --mode local` 的普通 release 可执行文件启动真实窗口：将 PATH 缩减为 Windows 系统目录，仍能加载 exe 同目录的自建媒体 DLL，显示视频，接收原生空格／F／Esc，关闭退出码 0。设置 `IRIS_CONTROL` 后命名管道仍不存在（Win32 error 2），运行日志没有监听记录；GUI 版通过 `IRIS_LOG` 写出 D3D11 和显示器 ICC 诊断。证据在 `target/package-smoke/`。

签名门禁实际执行：`--mode release` 在没有证书时以 `Signed release requires IRIS_WINDOWS_PFX` 失败，未伪装为已签名。当前 `.exe` 为 GUI subsystem，不依赖用户的 VCRUNTIME DLL（应用侧静态 CRT，媒体库仍为可替换 DLL）。

发行版截图另外暴露首次暂停全屏使用旧 canvas 高度、字幕上移过多；改用当前 `Window::viewport_size()` 计算全屏安全区。修复后首个暂停全屏立即为 77、退出为 0，精确跳转／重播／照片变换整条实测仍通过，不再依赖下一次 UI 重绘。

最终本地安装包 `target/dist-final/Iris-0.1.0-windows-x86_64-local-setup.exe` 已实际安装至独立测试目录，检查媒体／Rust／Iris 对应源码及双许可证存在；启动安装后的 `iris.exe`，仅系统 PATH 下视频与全屏正常、调试端点不存在、退出码 0；随后实际卸载成功。日志与结果在 `target/installer-evidence/`，测试安装已移除，未覆盖用户已有程序。

最终发行目录为 `target/dist-final/`，早期 `target/dist-local/` 只作增量编译与合规缓存，不作为最终交付。macOS 和正式签名不因本地安装成功而视为通过。

安装程序为 459494264 字节，SHA-256 `e8cd34e2373a83c621edc621ac70ebde3f111b77b3676a2ae79183c7fd7c389b`。`Get-AuthenticodeSignature` 实际结果 `NotSigned`，与 `unsigned-local` 清单一致，不使用自签冒充发行签名。


