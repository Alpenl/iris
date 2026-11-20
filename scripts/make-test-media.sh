#!/bin/sh
# Generates synthetic test media for the spike into ./test-media (gitignored).
set -eu
cd "$(dirname "$0")/.."
OUT=test-media
mkdir -p "$OUT"
FF="ffmpeg -hide_banner -loglevel error -y"

# 1080p60 H.264 with a tone, 30 s.
$FF -f lavfi -i testsrc2=size=1920x1080:rate=60 -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 30 -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -b:a 128k "$OUT/01-testsrc-1080p60.mp4"

# 4K30 HEVC 10-bit, 20 s (exercises VideoToolbox hardware decoding).
$FF -f lavfi -i testsrc2=size=3840x2160:rate=30 -f lavfi -i sine=frequency=660:sample_rate=48000 \
  -t 20 -c:v libx265 -preset ultrafast -pix_fmt yuv420p10le -tag:v hvc1 -x265-params log-level=error \
  -c:a aac -b:a 128k "$OUT/02-testsrc-4k30-hevc10.mp4"

# 2.39:1 clip in MKV with an SRT subtitle track, 20 s.
cat > "$OUT/subs.srt" <<'SRT'
1
00:00:01,000 --> 00:00:06,000
这是一条测试字幕 · Iris spike

2
00:00:07,000 --> 00:00:14,000
GPUI 叠在 mpv 视频上方
SRT
$FF -f lavfi -i smptehdbars=size=1920x804:rate=24 -f lavfi -i sine=frequency=330:sample_rate=48000 -i "$OUT/subs.srt" \
  -t 20 -map 0 -map 1 -map 2 -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -c:s srt \
  -metadata:s:s:0 language=chi "$OUT/03-bars-scope-subs.mkv"
rm "$OUT/subs.srt"

# Photos.
$FF -f lavfi -i testsrc2=size=4032x3024 -frames:v 1 "$OUT/10-photo-4032x3024.jpg"
$FF -f lavfi -i "mandelbrot=size=2400x1600" -frames:v 1 "$OUT/11-photo-mandelbrot.png"
$FF -f lavfi -i "color=c=0x333a40:size=3000x2000,drawgrid=w=100:h=100:t=1:c=0xff5b1f" -frames:v 1 "$OUT/12-photo-grid.jpg"
ls -la "$OUT"
