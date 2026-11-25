#!/usr/bin/env python3
"""Run native v2 adapters against explicitly supplied, reviewed BYOB binaries."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def file_record(path):
    path = Path(path).resolve(strict=True)
    return {"path": str(path), "sha256": hashlib.file_digest(path.open("rb"), "sha256").hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for tool in ("ffmpeg", "ffprobe", "whisper", "ytdlp", "model", "speech", "video"):
        parser.add_argument("--" + tool, required=True)
    parser.add_argument("--metadata-url", default="https://raw.githubusercontent.com/ggml-org/whisper.cpp/v1.8.2/samples/jfk.wav")
    args = parser.parse_args()
    receipt = {}
    for name in ("ffmpeg", "ffprobe", "whisper", "ytdlp", "model"):
        entry = file_record(getattr(args, name))
        if name != "model":
            directory = Path(entry["path"]).parent
            components = [p for p in directory.iterdir() if p.is_file() and p.suffix.lower() in (".dll", ".dylib", ".so", ".pyd")]
            if name == "ytdlp":
                components.extend(p for p in (directory / "_internal").rglob("*") if p.is_file())
            entry["components"] = [file_record(p) for p in sorted(set(components))]
        receipt[name] = entry
    receipt.update(speech=str(Path(args.speech).resolve(strict=True)), video=str(Path(args.video).resolve(strict=True)), metadata_url=args.metadata_url)
    target = ROOT / "target/v2-contracts/tools-receipt.json"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(receipt, ensure_ascii=False, indent=2), encoding="utf-8")
    env = dict(os.environ, IRIS_V2_TOOLS_RECEIPT=str(target))
    env.setdefault("MPV_LIB_DIR", str(ROOT / "target/deps/mpv"))
    env["PATH"] = env["MPV_LIB_DIR"] + os.pathsep + env["PATH"]
    subprocess.run(["cargo", "test", "--locked", "-p", "iris-core", "plugin_v2::tools::tests::native", "--lib", "--", "--include-ignored", "--test-threads=1", "--nocapture"], cwd=ROOT, env=env, check=True)
    print("Native tool receipts:", target)


if __name__ == "__main__":
    main()
