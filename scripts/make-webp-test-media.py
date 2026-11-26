#!/usr/bin/env python3
"""Generate lossless WebP ICC fixtures, optionally with PNG/JPEG controls."""
import argparse
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MAC_PROFILES = Path("/System/Library/ColorSync/Profiles")


def main(argv=None):
    from PIL import Image, ImageCms, features

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "test-media/photo/icc")
    parser.add_argument("--display-p3", type=Path)
    parser.add_argument("--adobe-rgb", type=Path)
    parser.add_argument("--all-formats", action="store_true")
    args = parser.parse_args(argv)
    if not features.check("webp"):
        parser.error("Pillow was built without WebP")
    profiles = {"srgb": ImageCms.ImageCmsProfile(ImageCms.createProfile("sRGB")).tobytes()}
    for tag, override, local_name, mac_name in (
        ("p3", args.display_p3, "p3.icc", "Display P3.icc"),
        ("adobergb", args.adobe_rgb, "adobergb.icc", "AdobeRGB1998.icc"),
    ):
        flag = "--display-p3" if tag == "p3" else "--adobe-rgb"
        if override is not None:
            if not override.is_file():
                parser.error(f"{flag} ICC profile not found: {override}")
            profiles[tag] = override.read_bytes()
            continue
        candidates = (args.output / local_name, MAC_PROFILES / mac_name)
        found = next((p for p in candidates if p.is_file()), None)
        if found:
            profiles[tag] = found.read_bytes()
        else:
            parser.error(f"Supply {flag} with an ICC profile")
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


if __name__ == "__main__":
    main()
