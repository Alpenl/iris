# 测试与实测记录

## 构建

```bash
brew install mpv                 # 开发用的 libmpv（GPL 构建，不能拿去分发）
cargo build --release --features devtools  # 性能/控制测试专用，不能作为发行包
cargo test --locked --workspace            # 72 项单元/端到端测试：协议与多源权限、插件隔离/异步校验与外部 stdio 握手、内置 iris.webdav、稀疏预设与基线保护、!TextInput 打字防火墙、硬解镜像自动回拷切换、照片/ICC 单遍解析、Windows WIC 缩略图方向/通道、UI 契约与 ThemeTokens 安全兜底
python3 scripts/check-project.py           # 仓库约束：递归 UI/mpv 隔离、iris-core 与 iris-api 依赖门禁、devtools 默认特性门禁、单文件 650/850 行预算、SVG 图标与插件清单校验
cargo deny check licenses        # 依赖许可证检查，需要先 cargo install cargo-deny
```

libmpv 不在 `/opt/homebrew/lib` 时用 `MPV_LIB_DIR` 指定。

## 测试素材

脚本都把素材写到 `test-media/`（已忽略，不进仓库）。

| 脚本 | 内容 | 依赖 |
|---|---|---|
| `scripts/make-test-media.sh` | 1080p60 H.264、4K30 HEVC 10bit、带中文字幕的 2.39:1 MKV、几张大照片 | ffmpeg |
| `scripts/make-photo-test-media.sh` | `test-media/photo/` 下的照片边界素材：EXIF 方向 1/3/6/8 的 JPEG 和方向 6 的 HEIC；48 MP 分块 HEIC；无标签 / sRGB / P3 / Adobe RGB 的纯色块；20000×3000、4000×40000、12000×12000 的巨图；JPEG 和 HEIC 各 20 张约 48 MP 的翻页图（每张宽度不同，用来识别是哪一张）| ffmpeg、exiftool、sips、swift（Command Line Tools）|
| `scripts/make-webp-test-media.py` | `test-media/photo/icc/` 下 sRGB / P3 / Adobe RGB 的无损 WebP 色块（本机 FFmpeg 没有 WebP 编码器，改用 Pillow）| Pillow（带 WebP）、macOS ColorSync 配置文件 |

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
- WebP：`make-webp-test-media.py` 生成的色块已进单元测试（`photo::tests::reads_colour_from_test_media`，sRGB / P3 / Adobe RGB 都识别正确）。
- **未实测**：放在网络盘上的照片。文件头读取已移到后台线程（`player::prepare`），不再卡界面，但慢盘上的打开延迟没有测过。

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

### 合并 Windows 验证分支后（2026-11-21，macOS）

远端 4176e96（Windows 实机验证版）为底，移入本地的收藏、续播、目录过滤、后台打开准备、单实例和 `iris-core` 拆分。

- `cargo test --workspace` 17 项通过；`cargo clippy --workspace --all-targets -D warnings` 在 macOS（默认、`devtools`、release）和 Windows 目标上都无警告；`scripts/check-project.py` 通过。
- Windows 目标在 Mac 上交叉检查：`x86_64-pc-windows-msvc` 的 debug `cargo clippy` 通过（`ring` 的 C 构建脚本用占位编译器跳过，不链接）。release 检查要编译 GPUI 的 HLSL 着色器，只能在 Windows 上跑，由 `.github/workflows/check.yml` 承担。
- 独立实例端到端（debug 构建，私有 socket 和 `IRIS_SETTINGS`）：视频跳到 8 秒按 S 收藏、`filter favorites`、`filter photo` 下按 → 只翻到照片；退出时在 12 秒，重启后同一视频从 12.0 秒继续，收藏仍在，音量 94 恢复。顺带发现并修复：音量经 ↑ 超过 100 时，保存的值会被下次读取整个丢弃，现在保存时限制在 0–100。
- **未验证**：Windows 单实例转发（只过了类型检查）；macOS 从 Finder 打开文件（`on_open_urls`）；这一轮界面在 Windows 上的实机表现。

### 界面重做（2026-11-22，macOS）

工具栏压成一行、时间轴并进工具栏上沿、SVG 图标、悬停提示、目录重排，见 README“界面”一节和 DECISIONS 1.8。

- `cargo test --workspace` 18 项通过（新增 `pixel_scale_reads_one_at_actual_size`）；`scripts/check-project.py` 通过；`cargo clippy --workspace --all-targets -- -D warnings` 在 macOS（默认、`--features devtools`、`--release`）和 Windows 目标（debug）上都无警告。
- Windows 目标的交叉检查这次把 `CC` / `AR` 换成一个只按 `-o`、`-Fo`、`-out:` 和 `ar` 参数创建空输出文件的占位脚本：`ring`、`psm`、`stacker` 的构建脚本要求输出文件存在，单用 `/usr/bin/true` 会失败。产物不链接，只用于 clippy。
- 独立实例实测（release + `devtools`，私有 socket 和 `IRIS_SETTINGS`，窗口 1280×800，截图用 `snap`）：空状态；视频工具栏；Tab 收起目录及提示文字；指针移到时间轴上变粗并显示该处时间；工具栏上的悬停提示不压住工具栏和时间轴（窗口左右边缘都试过）；音量格为灰色；收藏后 `filter favorites` 只列收藏，工具栏星标为橙色；目录放右侧时的视频和照片；照片适应窗口显示 48%，按空格到 1:1 显示 100%。
- 点击行为：点时间轴从 1.67 秒跳到 8.33 秒；点第 3 格音量把音量设为 30；点完按钮再按空格只切换暂停，不会再触发刚点过的按钮。
- 全屏实测（同一套独立实例，MacBook 内建屏；全屏窗口 1470×923 逻辑像素，不含刘海区）：
  - 工具栏悬浮在画面下方，离屏幕左、右、下边缘各 16px；文件名和媒体信息浮在顶部、目录的另一侧。
  - 工具栏显示时 `sub-margin-y-offset=53`，字幕落在工具栏上方；工具栏隐藏后和退出全屏后为 0。
  - 指针停在画面上 2 秒后工具栏和文件名一起隐藏，移动指针即恢复；指针停在工具栏上、目录弹窗打开时都不隐藏。
  - Tab 在左侧弹出目录；弹窗右上角的按钮把它换到右侧；点画面关闭弹窗。
  - 工具栏右端“退出全屏 Esc”的提示显示在工具栏上方，不压住工具栏；时间轴悬停变粗并显示该处时间。
  - 照片全屏适应屏幕显示 61%，悬浮工具栏补上边线；Esc 回到窗口，目录停在右侧，照片 48%。
- 全屏实测时发现并修复了过期的悬停提示：在提示出现之前点按钮，GPUI 过一会儿仍按点击前的文字和位置弹出提示。点“播放”后，暂停图标上方冒出“播放 Space”；在目录里换边后，按钮原来的位置上冒出“目录移到右侧”。原因是 GPUI 0.2.2 在指针移动时就按当时的文字排好提示，按下鼠标只清掉已经显示的提示。现在会变的按钮和工具栏那一行的元素 id 带上状态或布局，变化后旧提示随旧元素丢弃。复测（窗口和全屏各一遍）：点播放后不再出现旧提示，移动指针后显示“暂停 Space”；换边、点目录按钮、Tab 收起目录后都没有残留提示。修复后重跑 `cargo test`、上面四种 clippy 和 `check-project.py`，都通过。
- **未验证**：按住时间轴拖动（只测了点击）；全屏只在内建屏上测过，没测外接屏和多屏；Windows 实机上的新界面。

### 成熟播放器体验完善（2026-11-22，GitHub Actions）

范围：可调宽虚拟化目录（`uniform_list`）、搜索与多维排序、子目录导航、设置面板、60fps 跟手进度条与章节刻度、连续音量条、音频播放卡片、字幕模糊自动加载与偏移、画面比例/旋转/翻转/截图、防休眠、窗口置顶、独立轻量 OSD、右键上下文菜单、媒体详情与 EXIF、照片内存缩略图底条、macOS 原生捏合手势与媒体键。

- 提交：`8c15a5c`（基础体验与设置面板，CI `Checks` run `44930580831` 双端通过）、`bce0319` + Clippy 告警修复 `871385b`。
- GitHub Actions `Checks` run `44952797730`（Rust `1.100.0`）：
  - `macos`（`macos-15`）：`scripts/check-project.py`、`cargo test --locked --workspace`（23 项通过，新增音频/字幕分类、多维排序、倍速档位、设置校验与记录测试）、`cargo clippy --locked --workspace --all-targets -- -D warnings`（默认与 `--features iris/devtools`）全部通过。
  - `windows`（`windows-2025`）：`scripts/check-project.py` 与 debug / `--release` 两轮 `cargo clippy -- -D warnings` 全部通过。

### 控制面与矢量叠加面插件系统（2026-11-22，GitHub Actions）

范围：新增无头协议 crate `crates/iris-api`、`crates/iris-core/src/plugin.rs`（`PluginManager`、外部 JSON-lines stdio v1 运行时、权限门控、内置 `iris.guides` 与 `iris.ab-loop`）、命令面板（`⌘P` / `Ctrl+P`）、4 类宿主槽位渲染、设置内插件管理与示例插件 `examples/plugins/watch-stats`。

- 提交：`f732354` + 告警修复 `9037a2c`（移除多余 `mut` 并将 `iris-api` 的 `serde_json` 移至 `[dev-dependencies]`）。
- GitHub Actions `Checks` run `44971302313`（`9037a2c`）：
  - `macos`（`macos-15`，4m14s）：`scripts/check-project.py`（含 `iris-api` 无 GPUI / 无 `iris-mpv` 依赖约束）通过；`cargo test --locked --workspace` **29 项全部通过**（`iris-api` 2 项协议往返与路径穿越/协议版本校验 + `iris-core` 27 项，包含权限拦截、内置构图线/A-B 循环测试，以及拉起真实 Python 3 进程 `examples/plugins/watch-stats/main.py` 的外部 stdio 握手与命令集成测试）；默认与 `devtools` 两轮 `cargo clippy -- -D warnings` 零告警通过。
  - `windows`（`windows-2025`，4m22s）：`scripts/check-project.py` 与 debug / `--release` 两轮 `cargo clippy -- -D warnings` 零告警通过。
- GitHub Actions `Release recipe policy` run `44953868554`（`f732354`）：`cargo deny check licenses` 与发行策略检查通过。
- **未实机人工验证**：上述 2026-11-22 两轮新增交互（触控板双指捏合、macOS 媒体键与 Now Playing、底部缩略图条、右键菜单、命令面板、外部插件 GUI 交互、窗口置顶以及 Windows 实机界面与单实例转发）仅完成 CI 编译、Clippy、权限与 stdio 单元测试，尚未在实体 macOS / Windows 机器上逐项人工点验。

### 工程化、稳定性与性能优化打磨（2026-11-22，macOS 本机实测 + Windows 交叉检查）

范围：不增新功能，全面修复底层 FFI / 原生视频层 / 插件隔离隐患，优化热路径与重绘频率，补齐中文 IME / 剪贴板 / 键盘滚动等 UX 细节，拆分超大文件并收敛工具链/CI/打包脚本。

- **静态检查与格式**：
  - `python3 scripts/check-project.py`（扩展校验 `examples/**/*.py` 语法）通过；`scripts/release/*.py` 策略校验通过。
  - `cargo fmt --all -- --check` 通过（已在 `.github/workflows/check.yml` 与 `rust-toolchain.toml` 纳入校验）。
  - `cargo clippy --locked --workspace --all-targets -- -D warnings` 四套目标全部零告警通过：macOS debug 默认、macOS debug `--features iris/devtools`、macOS `--release`（Thin LTO）、Windows `x86_64-pc-windows-msvc` debug（验证 `platform/windows.rs` 原生 WIC 缩略图解码与 `video/windows.rs` 后台 ICC / `FLIP_DISCARD` / D3D11 设备重置恢复）。
- **单元与集成测试**：`cargo test --locked --workspace` **44 项全部通过**（`iris` 2 项 IME UTF-16 与缩略图 LRU 并发测试 + `iris-api` 2 项 + `iris-core` 40 项，新增 `input.rs` 策略全覆盖、畸形 PNG/JPEG/HEIF/ICC 防护、5000 项自然排序基准、脏标记与完成条目清理、真实 libmpv `track-list` JSON 解析、A-B 循环小时格式与防抖、有界读行器与外部插件崩溃/满载写队列非阻塞集成测试）。
- **macOS 独立隔离实例实测**（`target/release/iris`，带 `devtools` 与 Thin LTO，私有 Unix socket、`IRIS_SETTINGS` 与 `IRIS_PLUGINS`，挂载 `examples/plugins/watch-stats`）：
  - 空状态空闲 2.0s：`renders=3 -> 3`（**0.00 Hz**，彻底消除空转重绘）。
  - 30 秒 1080p60 视频播放（`videotoolbox` 硬解 + `watch-stats` 外部插件每秒更新状态）：3.01s 内 `delta=15`（**4.99 Hz**，含 4 Hz 短视频时间轴上限 + 1 Hz 外部插件状态刷新）。
  - 打开多轨字幕 MKV 并切换 `I` 详情面板、打开照片切换底部缩略图条 `G` / 1:1 `Space` / 适应 `0` / 翻页 `→` 均正常。
  - 搜索栏（`⌘F`）与命令面板（`⌘P`）中输入 `f` / `t` 保持 `fullscreen=false`（`Workspace && !TextInput` 作用域与 `EntityInputHandler` 生效），方向键滚动正常。
  - `quit` 退出码 0，外部 `watch-stats` Python 子进程零残留，`stderr` 无 panic、无 ERROR、无 mpv 废弃选项告警（已将 `video-aspect-override=-1` 修正为 `no`）。
- **照片边界与 ColorSync ICC 回归**（`scripts/photo-harness.py --bin target/release/iris ... probe exif heic icc` + `scripts/icc-report.py`）：
  - EXIF 1/3/6/8、HEIC 方向 6 与 48 MP 分块 HEIC 全部通过。
  - 17 个 PNG/JPEG 色块与 macOS ColorSync 预期值最大误差均在 **1–3 个色阶**内（`green-adobergb.png` 误差 1、`red-p3-jpeg.jpg` 误差 1、`red-srgb.png` 误差 3），色彩管线零回归。
- **未在 Windows 实体机复验**：Windows WIC 缩略图、D3D11 设备重置恢复与后台显示器 ICC 切换已通过 `x86_64-pc-windows-msvc` 交叉 Clippy 零告警验证，尚未在实体 Windows 机器上实测（后续已于 2026-11-23 完成实体机双屏核销，见下节）。

### 中期架构收敛后的 Windows 实体机回归与 Issue #1 核销（2026-11-23，Windows 11 双屏实测）

环境：Windows 11 x64，双显示器（主屏 `\\.\DISPLAY2` 2194×1234 @ 175% 缩放 + 副屏 `\\.\DISPLAY1` 1707×1067），Rust `1.100.0`，MSVC Build Tools 2022，自建 LGPL `libmpv` SDK（`target/lgpl-media-sdk`）。

- **实测与深度走查捕获并修复的 11 项问题**：
  1. **普通 `--release` 构建条件导入（`crates/iris/src/platform/windows.rs`）**：无 `devtools` 特性的 `--release` 构建下，顶层 `Gdi::*` 未随 `snapshot_window` 条件门控触发 `-D unused-imports` 报错，现已收拢至 `snapshot_window` 内部。
  2. **硬解视频水平/垂直镜像滤镜失效（`crates/iris-core/src/player/{mod,backend}.rs`）**：`hwdec=auto-safe`（零拷贝 `d3d11va` 硬件帧）下直接挂载 FFmpeg `hflip` / `vflip` 会报像素格式无法转换并自动禁用滤镜；现改为视频开启镜像期间自动切换至 `auto-copy`（`d3d11va-copy` 硬解回拷），关闭镜像或切集时立即恢复零拷贝 `auto-safe`（含回归单测）。
  3. **看图广色域/竖拍 HEIF 镜像滤镜顺序与色域保护（`crates/iris-core/src/photo/mod.rs`、`player/backend.rs`）**：`Photo::vf_chain_with_transforms` 将用户水平/垂直镜像按正立图像坐标换算并置于 `format=primaries=…:gamma=…[:rotate=…]` 之前，确保 Display P3 / Adobe RGB 色彩元数据与 HEIF 旋转标记不被 `lavfi` 镜像滤镜冲掉（含单测与实机验证）。
  4. **D3D11 设备丢失（TDR / 驱动更新）完整自愈闭环（`crates/iris/src/video/windows.rs`、`crates/iris-mpv/src/render.rs`、`player/backend.rs`、`ui/mod.rs`）**：除检测 DXGI 返回码（含 `REMOVED / RESET / HUNG / DRIVER_INTERNAL_ERROR`）外，新增 `ID3D11Device::GetDeviceRemovedReason()` 检查以捕获 `mpv_render_context_render` 侧先行触发的设备丢失；每次绘制成功后清零连续重试计数；修复表面重建期间 `mpv_render_context_free` 导致视频 `vid` 置 `no`（黑屏仅剩声音）及静帧图片自动卸载的问题——现重建后立即恢复显示器 ICC、重绑视频 `vid` 与 `d3d11va` 硬解，并对静帧照片/无音轨流自动原子重载且保留镜像、旋转与缩放平移状态。
  5. **点击窗口 `X` 按钮或 `Alt+F4`（`WM_CLOSE`）退出漏落盘（`crates/iris/src/ui/mod.rs`）**：GPUI 在窗口销毁时先释放 `IrisWindow` 实体再触发 `on_app_quit`，导致 `WeakEntity` 回调拿不到实体；现补充 `impl Drop for IrisWindow` 幂等调用 `shutdown()`，保证点击 `win-close` 与 `WM_CLOSE` 均完整保存续播进度、音量、倍速与窗口尺寸。
  6. **Windows 自绘标题栏最大化无法还原、双击空实现与单击模态卡死（`crates/iris/src/platform/windows.rs`、`ui/titlebar.rs`）**：绕开 GPUI 0.2.2 Windows 后端 `zoom_window()` 写死 `SW_MAXIMIZE` 及 `titlebar_double_click()` 空实现，基于 `IsZoomed(hwnd)` 原生切换 `SW_MAXIMIZE` / `SW_RESTORE`；标题栏拖拽接入 5px 位移阈值与 `GetAsyncKeyState(VK_LBUTTON)` 按下校验后调用 `WM_SYSCOMMAND(SC_DRAGMOVE)`，既支持最大化窗口拖出还原，又杜绝单击/双击误入 Win32 键盘移动模态循环。
  7. **Winsock 小请求体分包触发 TCP `RST`（`crates/iris-core/src/vfs/http_client.rs`）**：`TCP_NODELAY` 下请求头与小请求体分两次 `write_all` 发送，服务端提早关闭连接时易触发 `WSAECONNRESET`；现对 `<= 16 KiB` 请求体（如 WebDAV `PROPFIND` XML）与请求头合并单次发送。
  8. **Windows 本地路径冗余 `\\?\` 前缀统一（`crates/iris-core/src/{library,media_uri}.rs`）**：对常规长度本地磁盘路径自动剥离 `canonicalize()` 产生的 `\\?\` 逐字前缀，确保 `MediaUri::Local` 与 `file://` URI 往返判等、侧栏高亮、收藏续播及资源管理器定位一致。
  9. **快捷键作用域徽标同步（`crates/iris/src/ui/settings/shortcuts.rs`）**：设置 → 快捷键中的作用域标签改为由 `gpui_contexts` 推导（`R / Shift+R / H / Shift+H` 准确显示「视频/看图」）。
  10. **外部 Python 插件 Windows UTF-8 管道编码兜底（`crates/iris-core/src/plugin/process.rs`）**：外部 `.py` 插件子进程注入 `PYTHONUTF8=1` 与 `PYTHONIOENCODING=utf-8`，防止默认 GBK 代码页损坏非 ASCII JSON。
  11. **无 Lua 构建单测与照片脚本兼容（`crates/iris-mpv/src/lib.rs`、`scripts/photo-harness.py`）**：自建无 Lua `libmpv` 下兼容 `osc` 编译期不存在（错误码 `-5`）；`photo-harness.py` 跳过原始 `.icc` 配置文件。
- **静态门禁与全量测试**：
  - `python scripts/check-project.py`、`cargo fmt --all -- --check`、`cargo deny --locked check licenses` 全部通过。
  - Windows 原生三套 `cargo clippy --locked --workspace --all-targets -- -D warnings`（默认、`--features iris/devtools`、`--release` 含 HLSL 着色器编译）全部零告警通过。
  - `cargo test --locked --workspace` **72 项全部通过**（新增 Windows 原生 WIC 缩略图 EXIF 1/3/6/8 旋转宽高与直通 BGRA 像素通道单测、硬解镜像 `auto-copy` 切换回归单测及广色域/竖拍 HEIF 镜像滤镜单测）。
- **Issue #1、窗口控制、D3D11 重置自愈与新架构 Windows 实体机逐项点验（独立隔离实例，零污染用户 `%APPDATA%\Iris`）**：
  - **空转与播放渲染门控**：空窗口静置 2.0s `renders=2 -> 2`（**0.00 Hz**）；`01-smoke.mp4` 启用 `current-vo=libmpv`、`hwdec-current=d3d11va` 正常推进播放并可由 `mpv` 导出精确解码帧。
  - **D3D11 设备丢失连续三次故障注入自愈（视频 + 广色域照片）**：在 `01-smoke.mp4` 硬解播放中连续 3 次注入 `DXGI_ERROR_DEVICE_REMOVED`（`sim_device_reset`），渲染工作线程每次均以 `attempt 1` 完成旧表面销毁、新 `ID3D11Device` / DXGI 交换链 / DirectComposition 视觉树重建、显示器 ICC 重绑及 `vid=1` / `hwdec=d3d11va` 恢复，时间轴持续前进并成功导出 198 KB 真实解码帧；在水平镜像后的 Display P3 PNG 上注入设备重置，同样自动重载并恢复 `current-vo=libmpv` 与 `vf=hflip,format=primaries=display-p3:gamma=srgb`。
  - **Windows 窗口按钮、标题栏双击与退出落盘**：点击右上角 `win-max` 按钮在正常窗口与最大化（`IsZoomed == true`）间双向切换，双击 `#drag` 标题栏同样双向切换最大化/还原且不卡焦点；分别通过点击右上角 `win-close`（`remove_window`）与投递原生 `WM_CLOSE`（`Alt+F4`）关闭窗口，进程均以退出码 `0` 结束，准确落盘 `volume = 42`、`speed = 1.5`、`window_width = 1080`、`window_height = 680` 与 `records.tsv` 续播进度（`7.5s`）。
  - **设置「显示停靠侧栏」单真源与混合同步**：在 **设置 → 外观与常规** 点击切换开关，主窗口侧栏立即收起；退出后 `settings.conf` 写入 `directory_open = false`，冷启动重开保持 `dir=false`；混合 `Tab`、侧栏宽度调节、`Ctrl+F` 搜索、命令面板（`Ctrl+P` 切换至「数据源」视图）状态始终一致。
  - **变换、时间格式、全屏与双屏 ICC**：`R / Shift+R`（`video-rotate` `0° <-> 90°`）、视频 `H / Shift+H`（`d3d11va <-> d3d11va-copy` 无滤镜报错）、广色域 P3 PNG 与 `heic-o6.heic` 镜像滤镜（保留尾部 `format=primaries=display-p3:gamma=srgb` 与 `rotate=90`）、跳转 OSD（`+5s · 00:06`）与超过 1 分钟 WAV 纯音频卡片正常；全屏工具栏显示时 `sub-margin-y-offset > 0`，静置 2.3s 自动隐藏顶底栏且字幕边距归零，`Esc` 退出正常；将主窗口在 `\\.\DISPLAY2` 与 `\\.\DISPLAY1` 之间跨屏移动，后台线程均按 500ms 轮询触发 `display ICC` 更新且播放无中断。
  - **WIC 缩略图、中文/Emoji 直输与防火墙、右键菜单、置顶与单实例转发**：`photo-harness.py`（`probe / exif / heic / icc / huge`）全量通过，按 `G` 开启照片缩略图底条经 WIC 成功解码缓存；`Ctrl+F` 搜索框直接接收原生 `WM_CHAR` UTF-16 中文与代理对 Emoji（`测试🎬`）、两段式 `Escape`（先清空文本再关搜索框）、剪贴板中文+Emoji 粘贴及 `!TextInput` 快捷键防火墙均正常；右键弹出上下文菜单、`T` 置顶（Win32 `WS_EX_TOPMOST` 实测生效）正常；Windows 单实例转发含空格与中文路径（`中文目录 单实例测试\测试照片 10.jpg`）在 `0.098s` 内以退出码 `0` 准确投递至主实例打开。
  - **`1B+` 稀疏预设、`iris.webdav` + Windows 凭据管理器与外部插件**：`seek_step = 12` 个人基线下切换 `精听与学习 (3.0) -> 小窗置顶 (自动回退 12.0 + 置顶) -> 默认工作区`，磁盘基线零污染；`iris.webdav` 实连 Windows Credential Manager（`CredWriteW` / `CredReadW` / 测试后 `CredDeleteW` 清理）完成本地 WebDAV `PROPFIND`、带认证视频流与同名 `.srt` 字幕自动挂载、远程 Display P3 PNG `bytes=0-262143` 头探测与色彩滤镜注入；同时挂载 `watch-stats`、`cloud-media-demo`、`cinema-ui` 三个外部 Python 插件，`EnumWindows` 实测 **0 个控制台弹窗**（`CREATE_NO_WINDOW`），`cinema-ui` 无标题栏 `hide_titlebar` 顶缘悬浮唤醒零画面抖动、「恢复默认外观」一键逃生生效，退出后子进程 **0 残留**。
- **仍待后续环境验证的边界**：本轮中期重构后的 macOS 实体机人工点验（含触控板双指捏合与 macOS 媒体键）待在 Mac 设备上补齐。



