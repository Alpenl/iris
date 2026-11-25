#!/usr/bin/env python3
"""Build real Rust/C guests and run the protocol-2 integration contracts."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--zig", default=shutil.which("zig"), help="Zig executable (used as a freestanding C compiler)")
    args = parser.parse_args()
    if not args.zig:
        parser.error("install Zig or pass --zig; both language fixtures are required")
    fixtures = ROOT / "crates/iris-plugin-sdk/tests/fixtures"
    target = ROOT / "target/v2-contracts"
    target.mkdir(parents=True, exist_ok=True)
    subprocess.run(["cargo", "build", "--locked", "--release", "--target", "wasm32-unknown-unknown",
                    "--manifest-path", str(fixtures / "rust/Cargo.toml"),
                    "--target-dir", str(target / "rust")], cwd=ROOT, check=True)
    subprocess.run([args.zig, "cc", "-target", "wasm32-freestanding", "-O2", "-nostdlib",
                    "-Wl,--no-entry", "-Wl,--export-memory", "-Wl,-z,stack-size=65536",
                    str(fixtures / "contract.c"), "-o", str(target / "contract-c.wasm")], cwd=ROOT, check=True)
    env = dict(os.environ)
    env["IRIS_V2_C_GUEST"] = str(target / "contract-c.wasm")
    env["IRIS_V2_RUST_GUEST"] = str(target / "rust/wasm32-unknown-unknown/release/iris_v2_contract_guest.wasm")
    if "MPV_LIB_DIR" not in env:
        env["MPV_LIB_DIR"] = str(ROOT / "target/deps/mpv")
    env["PATH"] = env["MPV_LIB_DIR"] + os.pathsep + env["PATH"]
    subprocess.run(["cargo", "test", "--locked", "-p", "iris-core", "--test", "plugin_v2_contracts",
                    "--", "--include-ignored", "--nocapture"], cwd=ROOT, env=env, check=True)


if __name__ == "__main__":
    main()
