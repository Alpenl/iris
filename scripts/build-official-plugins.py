#!/usr/bin/env python3
"""Build public-SDK official Wasm plugins into deterministic .irisplugin archives."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import zipfile

ROOT = Path(__file__).resolve().parents[1]


def source_hash() -> str:
    paths = [ROOT / name for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "scripts/build-official-plugins.py")]
    for name in ("iris-api", "iris-plugin-sdk", "iris-plugin-guides", "iris-plugin-webdav"):
        crate = ROOT / "crates" / name
        paths += [crate / "Cargo.toml", *sorted((crate / "src").rglob("*.rs"))]
        if (crate / "plugin.json").is_file():
            paths.append(crate / "plugin.json")
    digest = hashlib.sha256()
    for path in sorted(paths):
        name = path.relative_to(ROOT).as_posix().encode("utf-8")
        # Git checkouts may use CRLF. The digest describes source content.
        data = path.read_bytes().replace(b"\r\n", b"\n")
        digest.update(len(name).to_bytes(8, "little") + name)
        digest.update(len(data).to_bytes(8, "little") + data)
    return digest.hexdigest()


def check(output: Path):
    index = json.loads((output / "index.json").read_text(encoding="utf-8"))
    assert index["schema"] == 1 and index["source_sha256"] == source_hash(), "Official Wasm source stamp is stale; rebuild the packages"
    assert {p["id"] for p in index["packages"]} == {"iris.guides", "iris.webdav"}
    assert len(index["packages"]) == 2
    for item in index["packages"]:
        assert item["file"] == item["id"] + ".irisplugin"
        archive = output / item["file"]
        assert hashlib.sha256(archive.read_bytes()).hexdigest() == item["sha256"]
        digest = hashlib.sha256()
        with zipfile.ZipFile(archive) as package_file:
            assert sorted(package_file.namelist()) == ["plugin.json", "plugin.wasm"]
            for name in sorted(package_file.namelist()):
                encoded, data = name.encode("ascii"), package_file.read(name)
                digest.update(len(encoded).to_bytes(8, "little") + encoded)
                digest.update(len(data).to_bytes(8, "little") + data)
            manifest = json.loads(package_file.read("plugin.json"))
            assert manifest["id"] == item["id"] and manifest["version"] == item["version"]
            assert manifest["protocol"] == manifest["abi"] == 2
        assert digest.hexdigest() == item["content_sha256"]
    return index


def package(crate: Path, output: Path) -> dict:
    manifest = json.loads((crate / "plugin.json").read_text(encoding="utf-8"))
    assert manifest["protocol"] == manifest["abi"] == 2
    assert manifest["id"].startswith("iris.") and manifest["entry"] == "plugin.wasm"
    wasm = ROOT / "target/wasm32-unknown-unknown/release" / (crate.name.replace("-", "_") + ".wasm")
    data = wasm.read_bytes()
    assert data.startswith(b"\x00asm\x01\x00\x00\x00")
    output.mkdir(parents=True, exist_ok=True)
    target = output / (manifest["id"] + ".irisplugin")
    parts = {"plugin.json": (json.dumps(manifest, ensure_ascii=False, sort_keys=True,
                                     separators=(",", ":")) + "\n").encode("utf-8"), "plugin.wasm": data}
    with zipfile.ZipFile(target, "w", compression=zipfile.ZIP_STORED) as archive:
        for name, content in sorted(parts.items()):
            entry = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
            entry.create_system = 3
            entry.external_attr = 0o100644 << 16
            archive.writestr(entry, content)
    content_hash = hashlib.sha256()
    for name, content in sorted(parts.items()):
        encoded = name.encode("ascii")
        content_hash.update(len(encoded).to_bytes(8, "little"))
        content_hash.update(encoded)
        content_hash.update(len(content).to_bytes(8, "little"))
        content_hash.update(content)
    return {"id": manifest["id"], "version": manifest["version"], "file": target.name,
            "sha256": hashlib.sha256(target.read_bytes()).hexdigest(), "content_sha256": content_hash.hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/plugin-v2/official")
    parser.add_argument("--no-build", action="store_true", help="Package already-built artifacts")
    parser.add_argument("--check", action="store_true", help="Check hashes and source freshness without building")
    parser.add_argument("--verify-embedded", action="store_true", help="Compare fresh build with checked-in distribution bytes")
    args = parser.parse_args()
    if args.check:
        print(json.dumps(check(args.output), indent=2))
        return
    crates = sorted(p.parent for p in (ROOT / "crates").glob("iris-plugin-*/plugin.json"))
    if not crates:
        raise SystemExit("No official plugin sources found")
    if not args.no_build:
        env = os.environ.copy()
        env["CARGO_TARGET_DIR"] = str(ROOT / "target")
        env["CARGO_PROFILE_RELEASE_DEBUG"] = "0"
        env["CARGO_PROFILE_RELEASE_STRIP"] = "debuginfo"
        env["CARGO_ENCODED_RUSTFLAGS"] = f"--remap-path-prefix={ROOT}=/iris"
        subprocess.run(["cargo", "build", "--locked", "--release", "--target", "wasm32-unknown-unknown",
                        *[part for crate in crates for part in ("-p", crate.name)]], cwd=ROOT, env=env, check=True)
    index = {"schema": 1, "source_sha256": source_hash(), "packages": [package(crate, args.output) for crate in crates]}
    (args.output / "index.json").write_text(json.dumps(index, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(index, indent=2))
    if args.verify_embedded:
        embedded = check(ROOT / "Resources/Plugins")
        assert index == embedded, "Fresh Wasm build differs from embedded distribution"


if __name__ == "__main__":
    main()
