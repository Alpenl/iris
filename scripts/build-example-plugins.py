#!/usr/bin/env python3
"""Build protocol 2 SDK examples; install resulting archives through the host review."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]


def cargo_target_dir(env: dict[str, str] | None = None) -> Path:
    source = os.environ if env is None else env
    raw = source.get("CARGO_TARGET_DIR", "").strip()
    if not raw:
        return ROOT / "target"
    path = Path(raw)
    return path if path.is_absolute() else (ROOT / path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/plugin-v2/examples")
    parser.add_argument("--no-build", action="store_true", help="Package already-built Wasm artifacts")
    args = parser.parse_args()
    examples = sorted((ROOT / "examples/plugins").glob("*/plugin.json"))
    crates = [(p, tomllib.loads((p.parent / "Cargo.toml").read_text(encoding="utf-8"))["package"]["name"])
              for p in examples if (p.parent / "Cargo.toml").is_file()]
    target_dir = cargo_target_dir()
    if not args.no_build:
        env = os.environ.copy()
        env["CARGO_TARGET_DIR"] = str(target_dir)
        env.pop("RUSTFLAGS", None)
        env.pop("CARGO_ENCODED_RUSTFLAGS", None)
        env["CARGO_PROFILE_RELEASE_DEBUG"] = "0"
        env["CARGO_PROFILE_RELEASE_STRIP"] = "debuginfo"
        subprocess.run(["cargo", "build", "--locked", "--release", "--target", "wasm32-unknown-unknown",
                        *[part for _, name in crates for part in ("-p", name)]], cwd=ROOT, env=env, check=True)
    args.output.mkdir(parents=True, exist_ok=True)
    for path in examples:
        manifest = json.loads(path.read_text(encoding="utf-8"))
        parts = {"plugin.json": (json.dumps(manifest, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")}
        if manifest.get("entry"):
            name = next(name for candidate, name in crates if candidate == path)
            parts["plugin.wasm"] = (target_dir / "wasm32-unknown-unknown/release" / (name.replace("-", "_") + ".wasm")).read_bytes()
        target = args.output / (manifest["id"] + ".irisplugin")
        with zipfile.ZipFile(target, "w", compression=zipfile.ZIP_STORED) as archive:
            for name, data in sorted(parts.items()):
                entry = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
                entry.create_system = 3
                entry.external_attr = 0o100644 << 16
                archive.writestr(entry, data)
        print(target)


if __name__ == "__main__":
    main()
