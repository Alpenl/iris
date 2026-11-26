#!/usr/bin/env python3
"""Build and test the real protocol 2 authoring examples."""
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
    parser.add_argument("--no-build", action="store_true", help="Reuse already-built example packages in target/plugin-v2/examples")
    profile = parser.add_mutually_exclusive_group()
    profile.add_argument("--release", action="store_true", help="Run integration tests in release mode (default, avoids 60s debug Cranelift timeout on virtual-audio)")
    profile.add_argument("--debug", action="store_true", help="Run integration tests in debug mode instead of release")
    args = parser.parse_args()
    if not args.no_build:
        subprocess.run([sys.executable, "-B", str(ROOT / "scripts/build-example-plugins.py")], cwd=ROOT, check=True)
    examples_dir = ROOT / "target/plugin-v2/examples"
    if not examples_dir.is_dir() or not any(examples_dir.glob("*.irisplugin")):
        raise SystemExit("Example packages missing; run scripts/build-example-plugins.py first or omit --no-build")
    env = configure_mpv_env(dict(os.environ, IRIS_V2_EXAMPLES=str(examples_dir)))
    cmd = ["cargo", "test", "--locked"]
    if not args.debug:
        cmd.append("--release")
    cmd += ["-p", "iris-core", "--test", "plugin_v2_examples", "--", "--ignored", "--nocapture"]
    subprocess.run(cmd, cwd=ROOT, env=env, check=True)


if __name__ == "__main__":
    main()
