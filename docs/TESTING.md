# 测试与实测记录

## 构建

```bash
brew install mpv                 # 开发用的 libmpv（GPL 构建，不能拿去分发）
cargo build --release            # 产物：target/release/iris
cargo test -p iris               # 单元测试（目前是 library.rs 的自然排序）
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

`scripts/photo-harness.py` 启动一个**独立的** Iris 实例（`IRIS_CONTROL` 指向自己建的 FIFO，`IRIS_SETTINGS` 指向输出目录里的设置文件），不会碰你正在用的窗口和真实设置。它会把窗口调成 1440×900，然后用 `open` / `get` / `state` / `snap` / `key` 跑测试。

```bash
cargo build --release
scripts/make-photo-test-media.sh
python3 scripts/photo-harness.py out/ exif heic icc huge flip
python3 scripts/icc-report.py out/
```

- 测试项：`probe`（冒烟）、`exif`、`heic`、`icc`、`huge`、`flip`，可任选几个。
- 输出：`out/results.json`（每次运行追加一行 JSON）、`out/<测试>-<文件>.png` 截图、`out/iris-stderr.log`。
- `--bin PATH` 可以换一个二进制，用来对比改动前后（例如先把旧版复制出来）。
- 翻页延迟的计时截止到 mpv 报出新文件的 `video-params` 为止，不含大纹理上传和上屏，真实延迟只会更长。
- `scripts/icc-report.py out/` 读取 `icc` 测试的截图，取视频区中心 100×100 像素的平均值，和 ColorSync 从源配置文件转换到截图内嵌的显示器配置文件的预期值比较。误差在 3 个色阶左右算一致。需要 numpy 和 Pillow（`pip3 install --user numpy pillow`）；Pillow 装在用户目录时不要用 `python3 -I`。

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

- 拖动窗口边缘实时缩放时视频和界面是否错位：程序模拟不出，需要人手动测。
- 一次全新启动后先开图片，视频区是黑的；之后多次重启没再出现。
- Windows：全部未验证。
