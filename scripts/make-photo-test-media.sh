#!/bin/sh
# Generates photo edge-case media into ./test-media/photo (gitignored):
#   exif/   JPEG with EXIF Orientation 1/3/6/8, HEIC with orientation 6
#           (stored image: gray, red block in the top-left quadrant)
#   heic/   48 MP HEIC (tiled grid) and a small HEIC control
#   icc/    solid-colour PNGs untagged / sRGB / Display P3 / Adobe RGB, plus a P3 JPEG
#   huge/   20000x3000 PNG, 4000x40000 PNG, 12000x12000 JPEG
#           (red marker in the bottom-right corner)
#   flip-jpg/, flip-heic/   20 noisy ~48 MP photos, each with a unique width
# Needs ffmpeg, exiftool, sips and swift (Command Line Tools).
set -eu
cd "$(dirname "$0")/.."
OUT=test-media/photo
FF="ffmpeg -hide_banner -loglevel error -y"
PROFILES=/System/Library/ColorSync/Profiles
mkdir -p "$OUT/exif" "$OUT/heic" "$OUT/icc" "$OUT/huge" "$OUT/flip-jpg" "$OUT/flip-heic"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# Small ImageIO helper: re-encode as HEIC with an orientation tag.
cat > "$TMP/heic_orient.swift" <<'SWIFT'
import Foundation
import ImageIO
import UniformTypeIdentifiers
let a = CommandLine.arguments
let src = CGImageSourceCreateWithURL(URL(fileURLWithPath: a[1]) as CFURL, nil)!
let img = CGImageSourceCreateImageAtIndex(src, 0, nil)!
let dst = CGImageDestinationCreateWithURL(URL(fileURLWithPath: a[2]) as CFURL,
                                          UTType.heic.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(dst, img, [kCGImagePropertyOrientation: Int(a[3])!] as CFDictionary)
guard CGImageDestinationFinalize(dst) else { exit(1) }
SWIFT

echo "exif"
$FF -f lavfi -i "color=c=gray:size=3000x2000,drawbox=x=0:y=0:w=1500:h=1000:color=red:t=fill" \
  -frames:v 1 -q:v 2 "$TMP/exif-base.jpg"
for o in 1 3 6 8; do
  cp "$TMP/exif-base.jpg" "$OUT/exif/exif-o$o.jpg"
  exiftool -q -overwrite_original -n "-Orientation=$o" "$OUT/exif/exif-o$o.jpg"
done
swift "$TMP/heic_orient.swift" "$TMP/exif-base.jpg" "$OUT/exif/heic-o6.heic" 6

echo "heic"
$FF -f lavfi -i "testsrc2=size=8064x6048,noise=alls=25:allf=u" -frames:v 1 -q:v 2 "$TMP/48mp.jpg"
sips -s format heic "$TMP/48mp.jpg" --out "$OUT/heic/heic-48mp-8064x6048.heic" >/dev/null
$FF -f lavfi -i "testsrc2=size=1000x750" -frames:v 1 -q:v 2 "$TMP/small.jpg"
sips -s format heic "$TMP/small.jpg" --out "$OUT/heic/heic-small-1000x750.heic" >/dev/null

echo "icc"
for c in "red 0xff0000" "green 0x00ff00" "blue 0x0000ff" "orange 0xc8783c"; do
  set -- $c
  $FF -f lavfi -i "color=c=$2:size=600x400" -frames:v 1 "$OUT/icc/$1-untagged.png"
  for p in "srgb:sRGB Profile.icc" "p3:Display P3.icc" "adobergb:AdobeRGB1998.icc"; do
    tag=${p%%:*}; file=${p#*:}
    cp "$OUT/icc/$1-untagged.png" "$OUT/icc/$1-$tag.png"
    exiftool -q -overwrite_original "-icc_profile<=$PROFILES/$file" "$OUT/icc/$1-$tag.png"
  done
done
$FF -f lavfi -i "color=c=0xff0000:size=600x400" -frames:v 1 -q:v 1 "$OUT/icc/red-p3-jpeg.jpg"
exiftool -q -overwrite_original "-icc_profile<=$PROFILES/Display P3.icc" "$OUT/icc/red-p3-jpeg.jpg"

echo "huge"
huge() { # name w h ext
  $FF -f lavfi -i "color=c=0x333a40:size=$2x$3,drawgrid=w=500:h=500:t=4:c=0xff5b1f,drawbox=x=iw-1000:y=ih-1000:w=1000:h=1000:color=red:t=fill" \
    -frames:v 1 -q:v 3 "$OUT/huge/$1.$4"
}
huge "huge-20000x3000" 20000 3000 png
huge "huge-4000x40000" 4000 40000 png
huge "huge-12000x12000" 12000 12000 jpg

echo "flip"
i=0
while [ $i -lt 20 ]; do
  w=$((8064 - 2 * i))
  n=$(printf "%02d" $i)
  $FF -f lavfi -i "testsrc2=size=${w}x6048,noise=alls=25:allf=u" -frames:v 1 -q:v 3 \
    "$OUT/flip-jpg/flip-$n-w$w.jpg"
  sips -s format heic "$OUT/flip-jpg/flip-$n-w$w.jpg" --out "$OUT/flip-heic/flip-$n-w$w.heic" >/dev/null
  i=$((i + 1))
done
du -sh "$OUT"/*
