#!/usr/bin/env python3
"""Pinned CI fixture setup and real Rust/C/official/example Wasm execution."""
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tarfile
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[1]
# Official https://ziglang.org/download/index.json, version 0.14.1.
ZIG = {
    "x86_64-windows": ("zip", "554f5378228923ffd558eac35e21af020c73789d87afeabf4bfd16f2e6feed2c"),
    "x86_64-macos": ("tar.xz", "b0f8bdfb9035783db58dd6c19d7dea89892acc3814421853e5752fe4573e5f43"),
    "aarch64-macos": ("tar.xz", "39f3dc5e79c22088ce878edc821dedb4ca5a1cd9f5ef915e9b3cc3053e8faefa"),
}


def main():
    target = ("aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64") + ("-windows" if os.name == "nt" else "-macos")
    extension, digest = ZIG[target]
    directory = ROOT / "target/v2-ci"
    directory.mkdir(parents=True, exist_ok=True)
    name = f"zig-{target}-0.14.1"
    archive = directory / (name + "." + extension)
    if not archive.exists():
        with urllib.request.urlopen(f"https://ziglang.org/download/0.14.1/{archive.name}", timeout=60) as response, archive.open("wb") as output:
            size = 0
            while block := response.read(1 << 20):
                size += len(block)
                if size > 100 << 20:
                    raise RuntimeError("Zig archive exceeds pinned distribution bounds")
                output.write(block)
    with archive.open("rb") as stream:
        assert hashlib.file_digest(stream, "sha256").hexdigest() == digest
    if extension == "zip":
        with zipfile.ZipFile(archive) as source:
            source.extractall(directory)
    else:
        with tarfile.open(archive) as source:
            source.extractall(directory, filter="data")
    zig = directory / name / ("zig.exe" if os.name == "nt" else "zig")
    for script, args in [
        ("build-official-plugins.py", ["--verify-embedded"]),
        ("build-example-plugins.py", []),
        ("test-plugin-v2.py", ["--zig", str(zig)]),
        ("test-plugin-v2-official.py", []),
        ("test-plugin-v2-examples.py", []),
    ]:
        subprocess.run([sys.executable, "-B", str(ROOT / "scripts" / script), *args], cwd=ROOT, check=True)
    subprocess.run(["cargo", "build", "--locked", "-p", "iris"], cwd=ROOT, check=True)
    executable = ROOT / "target/debug" / ("iris.exe" if os.name == "nt" else "iris")
    result = subprocess.run([str(executable), "--check-plugin-runtime"], cwd=ROOT, check=True, capture_output=True, text=True, timeout=180)
    receipt = json.loads(result.stdout)
    assert receipt["status"] == "ok" and receipt["protocol"] == 2
    (directory / "jit-smoke.json").write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
