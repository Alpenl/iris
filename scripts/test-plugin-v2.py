#!/usr/bin/env python3
"""Build real Rust/C guests and run the protocol-2 integration contracts."""
import argparse
import os
from pathlib import Path
import shutil
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
    parser.add_argument("--zig", default=shutil.which("zig"), help="Zig executable (used as a freestanding C compiler)")
    args = parser.parse_args()
    if not args.zig:
        parser.error("install Zig or pass --zig; both language fixtures are required")
    fixtures = ROOT / "crates/iris-plugin-sdk/tests/fixtures"
    target = ROOT / "target/v2-contracts"
    target.mkdir(parents=True, exist_ok=True)
    wasm_env = os.environ.copy()
    wasm_env.pop("RUSTFLAGS", None)
    wasm_env.pop("CARGO_ENCODED_RUSTFLAGS", None)
    subprocess.run(["cargo", "build", "--locked", "--release", "--target", "wasm32-unknown-unknown",
                    "--manifest-path", str(fixtures / "rust/Cargo.toml"),
                    "--target-dir", str(target / "rust")], cwd=ROOT, env=wasm_env, check=True)
    subprocess.run([args.zig, "cc", "-target", "wasm32-freestanding", "-O2", "-nostdlib",
                    "-Wl,--no-entry", "-Wl,--export-memory", "-Wl,-z,stack-size=65536",
                    str(fixtures / "contract.c"), "-o", str(target / "contract-c.wasm")], cwd=ROOT, check=True)
    env = configure_mpv_env(dict(os.environ))
    env["IRIS_V2_C_GUEST"] = str(target / "contract-c.wasm")
    env["IRIS_V2_RUST_GUEST"] = str(target / "rust/wasm32-unknown-unknown/release/iris_v2_contract_guest.wasm")
    subprocess.run(["cargo", "test", "--locked", "-p", "iris-plugin-host", "--lib",
                    "tests::guest_contracts::", "--", "--ignored", "--nocapture"], cwd=ROOT, env=env, check=True)
    subprocess.run(["cargo", "test", "--locked", "-p", "iris-plugin-host", "--test", "plugin_v2_contracts",
                    "--test", "plugin_v2_read_memory",
                    "--", "--include-ignored", "--test-threads=1", "--nocapture"], cwd=ROOT, env=env, check=True)


if __name__ == "__main__":
    main()
