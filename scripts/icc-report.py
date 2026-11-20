#!/usr/bin/env python3
"""Compares Iris's ICC swatch snapshots with the expected display colours.

usage: python3 scripts/icc-report.py OUTDIR [--window-width 1440]

OUTDIR is a directory written by `scripts/photo-harness.py OUTDIR icc`. For
each snapshot icc-<colour>-<tag>.<ext>.png it averages the centre 100x100
pixels of the video area and compares them with what ColorSync would show:
the swatch's raw RGB converted from its source profile (untagged counts as
sRGB) to the display profile embedded in the snapshot. "max err" is the
largest per-channel difference in 8-bit levels; about 3 or less is a match.

The video area comes from the harness's results.json (window points); the
pixel scale is the snapshot width divided by --window-width (the harness
resizes the window to 1440x900).

Needs numpy and Pillow with ImageCms (pip3 install --user numpy pillow).
"""
import argparse, glob, io, json, os, sys

import numpy as np
from PIL import Image, ImageCms

PROFILES = "/System/Library/ColorSync/Profiles/"
SOURCE = {
    "untagged": "sRGB Profile.icc",
    "srgb": "sRGB Profile.icc",
    "p3": "Display P3.icc",
    "adobergb": "AdobeRGB1998.icc",
    "p3-jpeg": "Display P3.icc",
}
RAW = {"red": (255, 0, 0), "green": (0, 255, 0), "blue": (0, 0, 255), "orange": (200, 120, 60)}

ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
ap.add_argument("outdir")
ap.add_argument("--window-width", type=float, default=1440.0)
args = ap.parse_args()

snaps = sorted(glob.glob(os.path.join(args.outdir, "icc-*.png")))
if not snaps:
    sys.exit(f"no icc-*.png in {args.outdir}; run photo-harness.py {args.outdir} icc first")

# Last run that has ICC results wins.
rects = {}
try:
    with open(os.path.join(args.outdir, "results.json")) as fh:
        for line in fh:
            icc = json.loads(line).get("icc")
            if icc:
                rects = {name: r.get("rect") for name, r in icc.items()}
except FileNotFoundError:
    pass

first = Image.open(snaps[0])
if "icc_profile" not in first.info:
    sys.exit("snapshot has no embedded display profile")
display = ImageCms.ImageCmsProfile(io.BytesIO(first.info["icc_profile"]))
scale = first.width / args.window_width


def expected(rgb, source):
    t = ImageCms.buildTransform(ImageCms.ImageCmsProfile(PROFILES + source), display,
                                "RGB", "RGB", ImageCms.Intent.RELATIVE_COLORIMETRIC)
    return ImageCms.applyTransform(Image.new("RGB", (1, 1), rgb), t).getpixel((0, 0))


print("%-22s %-16s %-16s %s" % ("file", "shown", "expected", "max err"))
for path in snaps:
    name = os.path.basename(path)[len("icc-"):-len(".png")]  # e.g. red-p3.png
    colour, rest = name.split("-", 1)
    tag = rest.rsplit(".", 1)[0]
    if colour not in RAW or tag not in SOURCE:
        continue
    rect = rects.get(name)
    if not rect:
        print("%-22s no video rect in results.json, skipped" % name)
        continue
    x, y, w, h = rect
    cx, cy = int((x + w / 2) * scale), int((y + h / 2) * scale)
    px = np.asarray(Image.open(path).convert("RGB")).astype(float)
    shown = tuple(int(round(v)) for v in px[cy - 50:cy + 50, cx - 50:cx + 50].reshape(-1, 3).mean(0))
    exp = expected(RAW[colour], SOURCE[tag])
    print("%-22s %-16s %-16s %d" % (name, shown, exp, max(abs(a - b) for a, b in zip(shown, exp))))
