#!/usr/bin/env python3
"""Build and test the real protocol 2 authoring examples."""
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]

if __name__ == "__main__":
    subprocess.run([sys.executable, "-B", str(ROOT / "scripts/build-example-plugins.py")], cwd=ROOT, check=True)
    env = dict(os.environ, IRIS_V2_EXAMPLES=str(ROOT / "target/plugin-v2/examples"))
    subprocess.run(["cargo", "test", "--locked", "-p", "iris-core", "--test", "plugin_v2_examples", "--", "--ignored", "--nocapture"], cwd=ROOT, env=env, check=True)
