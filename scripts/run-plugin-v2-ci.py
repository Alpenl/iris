#!/usr/bin/env python3
"""Pinned CI fixture setup and real Rust/C/official/example Wasm execution."""
import hashlib
import http.client
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.request
import zipfile

sys.dont_write_bytecode = True
if not __debug__:
    raise RuntimeError("Optimization (-O / PYTHONOPTIMIZE) disables plugin v2 CI checks")

ROOT = Path(__file__).resolve().parents[1]
ZIG_VERSION = "0.14.1"
# Official https://ziglang.org/download/index.json, version 0.14.1.
ZIG = {
    "x86_64-windows": ("zip", "554f5378228923ffd558eac35e21af020c73789d87afeabf4bfd16f2e6feed2c"),
    "x86_64-macos": ("tar.xz", "b0f8bdfb9035783db58dd6c19d7dea89892acc3814421853e5752fe4573e5f43"),
    "aarch64-macos": ("tar.xz", "39f3dc5e79c22088ce878edc821dedb4ca5a1cd9f5ef915e9b3cc3053e8faefa"),
}


def cargo_target_dir(env: dict[str, str] | None = None) -> Path:
    source = os.environ if env is None else env
    raw = source.get("CARGO_TARGET_DIR", "").strip()
    if not raw:
        return ROOT / "target"
    path = Path(raw)
    return path if path.is_absolute() else (ROOT / path)


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


def _file_sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def _zig_binary_valid(zig: Path) -> bool:
    if not zig.is_file():
        return False
    try:
        completed = subprocess.run(
            [str(zig), "version"],
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
    except OSError:
        return False
    return completed.returncode == 0 and completed.stdout.strip() == ZIG_VERSION


def _archive_matches(archive: Path, extension: str, digest: str) -> bool:
    if not archive.is_file():
        return False
    try:
        if _file_sha256(archive) != digest:
            return False
        if extension == "zip":
            with zipfile.ZipFile(archive) as source:
                return source.testzip() is None
        with tarfile.open(archive) as source:
            source.getmembers()
        return True
    except (zipfile.BadZipFile, tarfile.TarError, EOFError, OSError):
        return False


def _zig_matches_verified_archive(zig: Path, archive: Path, extension: str, member_name: str) -> bool:
    """Verify on-disk `zig` bytes match the exact member inside an already SHA-256-verified archive."""
    if not zig.is_file():
        return False
    try:
        actual = _file_sha256(zig)
        if extension == "zip":
            with zipfile.ZipFile(archive) as source, source.open(member_name) as member:
                expected = hashlib.file_digest(member, "sha256").hexdigest()
            return actual == expected
        with tarfile.open(archive) as source:
            member = source.extractfile(member_name)
            if member is None:
                return False
            with member:
                expected = hashlib.file_digest(member, "sha256").hexdigest()
            return actual == expected
    except (KeyError, zipfile.BadZipFile, tarfile.TarError, EOFError, OSError):
        return False


ZIG_DOWNLOAD_ATTEMPTS = 3
ZIG_DOWNLOAD_RETRY_SECONDS = 10


def _is_transient_download_error(error: BaseException) -> bool:
    """DNS, connection, timeout and server-side failures. Never 4xx, size or digest errors."""
    if isinstance(error, urllib.error.HTTPError):
        return error.code in (408, 429) or error.code >= 500
    return isinstance(error, (urllib.error.URLError, http.client.HTTPException, ConnectionError, TimeoutError))


def _download_zig(archive: Path, digest: str):
    url = f"https://ziglang.org/download/{ZIG_VERSION}/{archive.name}"
    for attempt in range(1, ZIG_DOWNLOAD_ATTEMPTS + 1):
        try:
            _fetch_zig_archive(url, archive, digest)
            return
        except Exception as error:
            if attempt == ZIG_DOWNLOAD_ATTEMPTS or not _is_transient_download_error(error):
                raise
            delay = ZIG_DOWNLOAD_RETRY_SECONDS * attempt
            print(f"Zig download attempt {attempt}/{ZIG_DOWNLOAD_ATTEMPTS} failed: {error}; retrying in {delay}s",
                  file=sys.stderr, flush=True)
            time.sleep(delay)


def _fetch_zig_archive(url: str, archive: Path, digest: str):
    partial = archive.with_suffix(archive.suffix + ".part")
    partial.unlink(missing_ok=True)
    try:
        with urllib.request.urlopen(url, timeout=60) as response, partial.open("wb") as output:
            size = 0
            while block := response.read(1 << 20):
                size += len(block)
                if size > 100 << 20:
                    raise RuntimeError("Zig archive exceeds pinned distribution bounds")
                output.write(block)
        actual = _file_sha256(partial)
        if actual != digest:
            raise RuntimeError(f"Zig archive SHA-256 mismatch: {actual} != {digest}")
        partial.replace(archive)
    except BaseException:
        partial.unlink(missing_ok=True)
        raise


def ensure_zig(directory: Path, target: str) -> Path:
    extension, digest = ZIG[target]
    directory.mkdir(parents=True, exist_ok=True)
    name = f"zig-{target}-{ZIG_VERSION}"
    extracted = directory / name
    zig_name = "zig.exe" if target.endswith("-windows") else "zig"
    zig = extracted / zig_name
    archive = directory / f"{name}.{extension}"

    # Verify the pinned archive SHA-256 and cached zig binary bytes BEFORE executing zig.
    if (
        archive.is_file()
        and _file_sha256(archive) == digest
        and _zig_matches_verified_archive(zig, archive, extension, f"{name}/{zig_name}")
        and _zig_binary_valid(zig)
    ):
        return zig

    if not _archive_matches(archive, extension, digest):
        archive.unlink(missing_ok=True)
        _download_zig(archive, digest)

    if extracted.exists():
        shutil.rmtree(extracted)
    try:
        if extension == "zip":
            with zipfile.ZipFile(archive) as source:
                source.extractall(directory)
        else:
            with tarfile.open(archive) as source:
                source.extractall(directory, filter="data")
    except (zipfile.BadZipFile, tarfile.TarError, EOFError, OSError):
        archive.unlink(missing_ok=True)
        if extracted.exists():
            shutil.rmtree(extracted)
        _download_zig(archive, digest)
        if extension == "zip":
            with zipfile.ZipFile(archive) as source:
                source.extractall(directory)
        else:
            with tarfile.open(archive) as source:
                source.extractall(directory, filter="data")

    if not _zig_binary_valid(zig):
        raise RuntimeError(f"Extracted Zig executable failed validation: {zig}")
    return zig


def verify_runtime_receipt(receipt: dict):
    identities = [row.get("plugin") for row in receipt.get("plugins", [])]
    if (
        receipt.get("status") != "ok"
        or receipt.get("protocol") != 2
        or receipt.get("abi") != 2
        or sorted(identities) != ["iris.guides", "iris.webdav"]
    ):
        raise RuntimeError(f"Plugin runtime smoke test failed: {receipt!r}")


def main():
    target = ("aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64") + ("-windows" if os.name == "nt" else "-macos")
    directory = ROOT / "target/v2-ci"
    zig = ensure_zig(directory, target)
    for script, args in [
        ("build-official-plugins.py", ["--verify-embedded"]),
        ("build-example-plugins.py", []),
        ("test-plugin-v2.py", ["--zig", str(zig)]),
        ("test-plugin-v2-official.py", ["--no-build"]),
        ("test-plugin-v2-examples.py", ["--no-build", "--release"]),
    ]:
        subprocess.run([sys.executable, "-B", str(ROOT / "scripts" / script), *args], cwd=ROOT, check=True)
    env = configure_mpv_env(os.environ.copy())
    subprocess.run(["cargo", "build", "--locked", "-p", "iris"], cwd=ROOT, env=env, check=True)
    executable = cargo_target_dir(env) / "debug" / ("iris.exe" if os.name == "nt" else "iris")
    result = subprocess.run([str(executable), "--check-plugin-runtime"], cwd=ROOT, env=env, check=True, capture_output=True, text=True, timeout=180)
    receipt = json.loads(result.stdout)
    verify_runtime_receipt(receipt)
    (directory / "jit-smoke.json").write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
