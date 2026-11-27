#!/usr/bin/env python3
"""Exercise freshly built official Wasm packages through the real host runtime."""
import argparse
import os
from pathlib import Path
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
    parser.add_argument("--no-build", action="store_true", help="Reuse already-built official packages in target/plugin-v2/official")
    args = parser.parse_args()
    if not args.no_build:
        subprocess.run([sys.executable, "-B", str(ROOT / "scripts/build-official-plugins.py")], cwd=ROOT, check=True)
    guides = ROOT / "target/plugin-v2/official/iris.guides.irisplugin"
    webdav = ROOT / "target/plugin-v2/official/iris.webdav.irisplugin"
    if not guides.is_file() or not webdav.is_file():
        raise SystemExit("Official packages missing; run scripts/build-official-plugins.py first or omit --no-build")
    env = configure_mpv_env(os.environ.copy())
    env["IRIS_V2_GUIDES_PACKAGE"] = str(guides)
    env["IRIS_V2_WEBDAV_PACKAGE"] = str(webdav)
    subprocess.run(["cargo", "test", "--locked", "-p", "iris-plugin-host", "--test", "plugin_v2_official",
                    "--", "--ignored", "--test-threads=1", "--nocapture"], cwd=ROOT, env=env, check=True)


if __name__ == "__main__":
    main()
