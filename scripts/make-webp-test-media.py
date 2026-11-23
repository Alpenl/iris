#!/usr/bin/env python3
"""Generate lossless WebP ICC fixtures, optionally with PNG/JPEG controls."""
import argparse
from pathlib import Path
from PIL import Image, ImageCms, features

ROOT = Path(__file__).resolve().parents[1]
MAC_PROFILES = Path("/System/Library/ColorSync/Profiles")
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--output", type=Path, default=ROOT / "test-media/photo/icc")
parser.add_argument("--display-p3", type=Path)
parser.add_argument("--adobe-rgb", type=Path)
parser.add_argument("--all-formats", action="store_true")
args = parser.parse_args()
if not features.check("webp"):
    parser.error("Pillow was built without WebP")
profiles = {"srgb": ImageCms.ImageCmsProfile(ImageCms.createProfile("sRGB")).tobytes()}
for tag, override, path in (("p3", args.display_p3, MAC_PROFILES / "Display P3.icc"),
                            ("adobergb", args.adobe_rgb, MAC_PROFILES / "AdobeRGB1998.icc")):
    if override or path.is_file():
        profiles[tag] = (override or path).read_bytes()
    else:
        parser.error(f"Supply --{'display-p3' if tag == 'p3' else 'adobe-rgb'} with an ICC profile")
args.output.mkdir(parents=True, exist_ok=True)
swatches = {"red": (255, 0, 0), "green": (0, 255, 0), "blue": (0, 0, 255), "orange": (200, 120, 60)}
for name, rgb in swatches.items():
    image = Image.new("RGB", (600, 400), rgb)
    for tag, profile in profiles.items():
        if name != "blue":
            image.save(args.output / f"{name}-{tag}-webp.webp", lossless=True, icc_profile=profile)
        if args.all_formats:
            image.save(args.output / f"{name}-{tag}.png", icc_profile=profile)
    if args.all_formats:
        image.save(args.output / f"{name}-untagged.png")
        if name == "red":
            image.save(args.output / "red-p3-jpeg.jpg", quality=100, icc_profile=profiles["p3"])
print(f"Created ICC fixtures in {args.output}")
