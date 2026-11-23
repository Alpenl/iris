#!/usr/bin/env python3
"""Compare ICC swatch snapshots with a Pillow/LittleCMS color transform."""
import argparse
import io
import json
from pathlib import Path

from PIL import Image, ImageCms, ImageStat


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("outdir", type=Path)
    parser.add_argument("--window-width", type=float, default=1440.0)
    parser.add_argument("--fixture-dir", type=Path,
                        default=Path(__file__).resolve().parents[1] / "test-media/photo/icc")
    output = parser.add_mutually_exclusive_group()
    output.add_argument("--display-profile", type=Path, help="ICC profile of the monitor used for untagged snapshots")
    output.add_argument("--assume-srgb", action="store_true", help="Treat untagged snapshots as sRGB")
    parser.add_argument("--max-error", type=int, default=3)
    args = parser.parse_args()
    results = {}
    with (args.outdir / "results.json").open(encoding="utf-8") as file:
        for line in file:
            icc = json.loads(line).get("icc")
            if icc:
                results = icc
    if not results:
        parser.error("No ICC results. Run photo-harness.py OUTDIR icc first.")

    srgb = ImageCms.createProfile("sRGB")
    print(f"{'file':30} {'shown':16} {'expected':16} max err")
    failed = False
    for name, result in sorted(results.items()):
        snapshot = args.outdir / f"icc-{name}.png"
        source = args.fixture_dir / name
        if not source.is_file() and result.get("source"):
            source = Path(result["source"])
        if not result.get("rect"):
            raise RuntimeError(f"Missing video rectangle for {name}")
        with Image.open(source) as fixture, Image.open(snapshot) as image:
            source_profile = fixture.info.get("icc_profile")
            if source_profile:
                source_profile = ImageCms.ImageCmsProfile(io.BytesIO(source_profile))
            elif "-untagged." in name:
                source_profile = srgb
            else:
                raise RuntimeError(f"Tagged fixture has no embedded profile: {source}")
            embedded = image.info.get("icc_profile")
            if embedded:
                display = ImageCms.ImageCmsProfile(io.BytesIO(embedded))
            elif args.display_profile:
                display = ImageCms.getOpenProfile(str(args.display_profile))
            elif args.assume_srgb:
                display = srgb
            else:
                parser.error(f"{snapshot.name} is untagged; specify --display-profile or --assume-srgb")
            pixel = fixture.convert("RGB").getpixel((fixture.width // 2, fixture.height // 2))
            transform = ImageCms.buildTransform(source_profile, display, "RGB", "RGB",
                                                ImageCms.Intent.RELATIVE_COLORIMETRIC)
            expected = ImageCms.applyTransform(Image.new("RGB", (1, 1), pixel), transform).getpixel((0, 0))
            x, y, width, height = result["rect"]
            scale = image.width / args.window_width
            cx, cy = round((x + width / 2) * scale), round((y + height / 2) * scale)
            radius = min(50, int(min(width, height) * scale / 2), cx, cy, image.width - cx, image.height - cy)
            if radius <= 0:
                raise RuntimeError(f"Invalid video rectangle for {snapshot}")
            region = image.crop((cx - radius, cy - radius, cx + radius, cy + radius)).convert("RGB")
            shown = tuple(round(channel) for channel in ImageStat.Stat(region).mean)
        error = max(abs(a - b) for a, b in zip(shown, expected))
        print(f"{name:30} {str(shown):16} {str(expected):16} {error}")
        failed |= error > args.max_error
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
