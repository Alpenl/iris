#!/usr/bin/env python3
"""Generate reproducible H.264/AAC adaptive fixtures and test actual libmpv decoding."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]


def configure_mpv_env(env: dict[str, str], windows: bool | None = None) -> dict[str, str]:
    is_windows = (os.name == "nt") if windows is None else windows
    if is_windows:
        if "MPV_LIB_DIR" not in env or not env["MPV_LIB_DIR"].strip():
            env["MPV_LIB_DIR"] = str(ROOT / "target/deps/mpv")
        mpv_dir = Path(env["MPV_LIB_DIR"])
        path_entries = [str(mpv_dir)]
        sibling_bin = mpv_dir.parent / "bin"
        if mpv_dir.name.lower() == "lib" and sibling_bin.is_dir():
            path_entries.insert(0, str(sibling_bin))
        env["PATH"] = os.pathsep.join([*path_entries, env.get("PATH", "")])
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ffmpeg", required=True, type=Path)
    args = parser.parse_args()
    ffmpeg = args.ffmpeg.resolve(strict=True)
    directory = ROOT / "target/v2-contracts/adaptive"
    directory.mkdir(parents=True, exist_ok=True)
    prefix = [str(ffmpeg), "-hide_banner", "-loglevel", "error", "-nostdin", "-y"]
    commands = [
        ["-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25:duration=6", "-f", "lavfi", "-i",
         "sine=frequency=440:sample_rate=48000:duration=6", "-c:v", "libx264", "-threads", "1",
         "-preset", "fast", "-g", "25", "-sc_threshold", "0", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "96k", "reference.mp4"],
        ["-i", "reference.mp4", "-map", "0", "-c", "copy", "-f", "hls", "-hls_time", "1", "-hls_playlist_type", "vod",
         "-hls_segment_filename", "hls-%02d.ts", "hls.m3u8"],
    ]
    for command in commands:
        subprocess.run(prefix + command, cwd=directory, check=True, timeout=60)
    with ffmpeg.open("rb") as file:
        ffmpeg_digest = hashlib.file_digest(file, "sha256").hexdigest()
    receipt = {"platform": platform.platform(), "ffmpeg_sha256": ffmpeg_digest,
               "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
               "ffmpeg_version": subprocess.check_output([str(ffmpeg), "-version"], text=True), "commands": commands,
               "files": {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(directory.iterdir()) if p.is_file()},
               "scope": "160x90 H.264/AAC, six seconds, local HTTP, native null output; excludes GPU/display and 4K performance"}
    env = configure_mpv_env(dict(os.environ, IRIS_V2_ADAPTIVE_FIXTURE=str(directory)))
    result = subprocess.run(["cargo", "test", "--locked", "-p", "iris-plugin-host", "--test", "player_scoped_plugin",
                             "real_hls_decode_through_scoped_rewritten_manifest", "--", "--ignored", "--nocapture"],
                            cwd=ROOT, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=300)
    print(result.stdout)
    receipt.update(exit_code=result.returncode, output=result.stdout)
    (directory.parent / "adaptive-receipt.json").write_text(
        json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8", newline="\n"
    )
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
