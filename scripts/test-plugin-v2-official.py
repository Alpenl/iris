#!/usr/bin/env python3
"""Exercise freshly built official Wasm packages through the real host runtime."""
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def main():
    subprocess.run([sys.executable, "-B", str(ROOT / "scripts/build-official-plugins.py")], cwd=ROOT, check=True)
    env = os.environ.copy()
    env["IRIS_V2_GUIDES_PACKAGE"] = str(ROOT / "target/plugin-v2/official/iris.guides.irisplugin")
    env["IRIS_V2_WEBDAV_PACKAGE"] = str(ROOT / "target/plugin-v2/official/iris.webdav.irisplugin")
    subprocess.run(["cargo", "test", "--locked", "-p", "iris-core", "--test", "plugin_v2_official",
                    "--", "--ignored", "--test-threads=1", "--nocapture"], cwd=ROOT, env=env, check=True)


if __name__ == "__main__":
    main()
