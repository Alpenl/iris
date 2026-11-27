#!/usr/bin/env python3
"""Build public-SDK official Wasm plugins into deterministic .irisplugin archives."""
import argparse
import functools
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tomllib
import zipfile

sys.dont_write_bytecode = True
if not __debug__:
    raise RuntimeError("Optimization (-O / PYTHONOPTIMIZE) disables official plugin verification gates")

ROOT = Path(__file__).resolve().parents[1]

PINNED_TOOLCHAIN = "1.100.0"
PINNED_RUSTC_COMMIT = "0ef1bc386059596dada8fb65b50425aab2ea637a"
# Official Docker Hub linux/amd64 manifest digest for rust:1.100.0-slim-bookworm.
PINNED_LINUX_IMAGE = (
    "rust:1.100.0-slim-bookworm@"
    "sha256:5d3170f8660a07782d5ff42abc55601d2df51d49e5397a8f22247341a658e128"
)
CRATES_IO_INDEX_DIRS = (
    "index.crates.io-1949cf8c6b5b557f",
    "index.crates.io-6f17d22bba15001f",
)

DEFAULT_NETWORK = {
    "net_public": False,
    "net_hosts": [],
    "public_ports": [],
    "tcp_targets": [],
}

# Reviewed capability/network baseline for bundled official plugins (R1-M07).
# Expanding permissions or static network rules requires updating this baseline
# and verifying the host upgrade review path in crates/iris-plugin-host/src/official.rs.
FROZEN_OFFICIAL_CAPABILITIES = {
    "iris.guides": (
        frozenset({"ui_overlay", "ui_status", "ui_sidebar", "notifications"}),
        DEFAULT_NETWORK,
    ),
    "iris.webdav": (
        frozenset({"media_source", "net_http", "secrets", "ui_modal", "notifications"}),
        DEFAULT_NETWORK,
    ),
}

_WINDOWS_USERS_RE = re.compile(rb"(?i)[a-z]:[\\/]+users[\\/]+")


def _require(condition: bool, message: str):
    if not condition:
        raise AssertionError(message)


def _is_windows_host() -> bool:
    return os.name == "nt"


def _is_linux_x86_64() -> bool:
    return sys.platform == "linux" and platform.machine().lower() in ("x86_64", "amd64")


def cargo_target_dir(env: dict[str, str] | None = None) -> Path:
    source = os.environ if env is None else env
    raw = source.get("CARGO_TARGET_DIR", "").strip()
    if not raw:
        return ROOT / "target"
    path = Path(raw)
    return path if path.is_absolute() else (ROOT / path)


def _user_home(source: dict[str, str]) -> Path | None:
    keys = ("USERPROFILE", "HOME") if _is_windows_host() else ("HOME", "USERPROFILE")
    for key in keys:
        raw = source.get(key, "").strip()
        if raw:
            return Path(raw).resolve()
    try:
        return Path.home().resolve()
    except RuntimeError:
        return None


def _cargo_home(env: dict[str, str] | None = None) -> Path | None:
    source = os.environ if env is None else env
    raw = source.get("CARGO_HOME", "").strip()
    if raw:
        return Path(raw).resolve()
    home = _user_home(source)
    return (home / ".cargo").resolve() if home is not None else None


def _rustup_home(env: dict[str, str] | None = None) -> Path | None:
    source = os.environ if env is None else env
    raw = source.get("RUSTUP_HOME", "").strip()
    if raw:
        return Path(raw).resolve()
    home = _user_home(source)
    return (home / ".rustup").resolve() if home is not None else None


@functools.lru_cache(maxsize=8)
def _cached_rustc_sysroot(rustup_home: str, toolchain: str, path_env: str) -> Path | None:
    query_env = os.environ.copy()
    if rustup_home:
        query_env["RUSTUP_HOME"] = rustup_home
    if toolchain:
        query_env["RUSTUP_TOOLCHAIN"] = toolchain
    if path_env:
        query_env["PATH"] = path_env
    try:
        proc = subprocess.Popen(
            ["rustc", "--print", "sysroot"],
            cwd=ROOT,
            env=query_env,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        out, _ = proc.communicate(timeout=15)
        if proc.returncode == 0 and out.strip():
            return Path(out.strip()).resolve()
    except (OSError, subprocess.SubprocessError):
        pass
    return None


@functools.lru_cache(maxsize=8)
def _cached_rustc_commit_hash(rustup_home: str, toolchain: str, path_env: str) -> str:
    query_env = os.environ.copy()
    if rustup_home:
        query_env["RUSTUP_HOME"] = rustup_home
    if toolchain:
        query_env["RUSTUP_TOOLCHAIN"] = toolchain
    if path_env:
        query_env["PATH"] = path_env
    try:
        proc = subprocess.Popen(
            ["rustc", "-vV"],
            cwd=ROOT,
            env=query_env,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        vv, _ = proc.communicate(timeout=15)
        if proc.returncode == 0:
            for line in vv.splitlines():
                if line.startswith("commit-hash:"):
                    candidate = line.partition(":")[2].strip()
                    if re.fullmatch(r"[0-9a-f]{40}", candidate):
                        return candidate
                    break
    except (OSError, subprocess.SubprocessError):
        pass
    return PINNED_RUSTC_COMMIT


def _rustc_sysroot(env: dict[str, str] | None = None) -> Path | None:
    source = os.environ if env is None else env
    return _cached_rustc_sysroot(
        source.get("RUSTUP_HOME", ""),
        source.get("RUSTUP_TOOLCHAIN", ""),
        source.get("PATH", ""),
    )


def _rustc_commit_hash(env: dict[str, str] | None = None) -> str:
    source = os.environ if env is None else env
    return _cached_rustc_commit_hash(
        source.get("RUSTUP_HOME", ""),
        source.get("RUSTUP_TOOLCHAIN", ""),
        source.get("PATH", ""),
    )


def build_remap_rustflags(env: dict[str, str] | None = None) -> str:
    """Build \\x1f-separated rustc --remap-path-prefix flags (later entries take precedence in rustc)."""
    mappings: list[tuple[Path, str]] = [(ROOT, "/iris")]
    cargo_home = _cargo_home(env)
    if cargo_home is not None:
        mappings.append((cargo_home, "/cargo"))
        reg_src = cargo_home / "registry/src"
        index_names = list(CRATES_IO_INDEX_DIRS)
        if reg_src.is_dir():
            for child in sorted(reg_src.iterdir()):
                if child.is_dir() and child.name not in index_names:
                    index_names.append(child.name)
        for index_name in index_names:
            mappings.append((reg_src / index_name, "/cargo/deps"))
    rustup_home = _rustup_home(env)
    if rustup_home is not None:
        mappings.append((rustup_home, "/rustup"))
    sysroot = _rustc_sysroot(env)
    if sysroot is not None:
        mappings.append((sysroot, "/rustup-sysroot"))
        commit_hash = (_rustc_commit_hash(env) if (sysroot / "lib/rustlib/src/rust").is_dir() else "") or PINNED_RUSTC_COMMIT
        mappings.append((sysroot / "lib/rustlib/src/rust", f"/rustc/{commit_hash}"))
    flags: list[str] = []
    seen: set[str] = set()
    for prefix, replacement in mappings:
        for form in (str(prefix), prefix.as_posix()):
            flag = f"--remap-path-prefix={form}={replacement}"
            if flag not in seen:
                seen.add(flag)
                flags.append(flag)
    return "\x1f".join(flags)


def write_remap_args_file(target_dir: Path, env: dict[str, str] | None = None) -> Path:
    """Write per-.rs POSIX remaps on Windows hosts so native scratch builds never leak backslashes."""
    lines: list[str] = []
    seen: set[str] = set()

    def add(src: Path, posix_dest: str):
        for form in (str(src), src.as_posix()):
            flag = f"--remap-path-prefix={form}={posix_dest}"
            if flag not in seen:
                seen.add(flag)
                lines.append(flag)

    sysroot = _rustc_sysroot(env)
    if sysroot is not None:
        rust_src_root = sysroot / "lib/rustlib/src/rust"
        if rust_src_root.is_dir():
            commit_hash = _rustc_commit_hash(env) or PINNED_RUSTC_COMMIT
            add(rust_src_root, f"/rustc/{commit_hash}")
            lib_dir = rust_src_root / "library"
            if lib_dir.is_dir():
                for rs in sorted(lib_dir.rglob("*.rs")):
                    add(rs, f"/rustc/{commit_hash}/library/{rs.relative_to(lib_dir).as_posix()}")

    cargo_home = _cargo_home(env)
    lock_path = ROOT / "Cargo.lock"
    if cargo_home is not None and lock_path.is_file():
        closure = json.loads(lock_closure(lock_path.read_text(encoding="utf-8"), OFFICIAL_SOURCE_CRATES))
        reg_roots = sorted((cargo_home / "registry/src").glob("*")) if (cargo_home / "registry/src").is_dir() else []
        for pkg in closure.get("package", []):
            if not pkg.get("source"):
                continue
            dir_name = f"{pkg['name']}-{pkg['version']}"
            for reg in reg_roots:
                crate_dir = reg / dir_name
                if crate_dir.is_dir():
                    add(crate_dir, f"/cargo/deps/{dir_name}")
                    for rs in sorted(crate_dir.rglob("*.rs")):
                        add(rs, f"/cargo/deps/{dir_name}/{rs.relative_to(crate_dir).as_posix()}")

    for crate_name in OFFICIAL_SOURCE_CRATES:
        crate_dir = ROOT / "crates" / crate_name
        if crate_dir.is_dir():
            for rs in sorted(crate_dir.rglob("*.rs")):
                rel = rs.relative_to(ROOT)
                rel_posix = rel.as_posix()
                add(rs, rel_posix)
                win_rel = rel_posix.replace("/", "\\")
                flag = f"--remap-path-prefix={win_rel}={rel_posix}"
                if flag not in seen:
                    seen.add(flag)
                    lines.append(flag)

    target_dir.mkdir(parents=True, exist_ok=True)
    content = ("\n".join(lines) + ("\n" if lines else "")).encode("utf-8")
    stamp = hashlib.sha256(content).hexdigest()[:16]
    args_path = target_dir / f"official-plugin-remap-{stamp}.args"
    args_path.write_bytes(content)
    return args_path


def _find_container_runtime() -> str | None:
    for candidate in ("docker", "podman"):
        binary = shutil.which(candidate)
        if not binary:
            continue
        try:
            proc = subprocess.Popen(
                [binary, "info", "--format", "{{.OSType}}"],
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
            )
            out, _ = proc.communicate(timeout=20)
            if proc.returncode == 0 and "linux" in out.strip().lower():
                return binary
        except (OSError, subprocess.SubprocessError):
            continue
    return None


def _wsl_linux_x86_64_available() -> bool:
    if not _is_windows_host() or not shutil.which("wsl"):
        return False
    try:
        proc = subprocess.Popen(
            ["wsl", "-e", "uname", "-m"],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        out, _ = proc.communicate(timeout=8)
        return proc.returncode == 0 and out.strip().lower() in ("x86_64", "amd64")
    except (OSError, subprocess.SubprocessError):
        return False


def _windows_to_wsl_path(path: Path) -> str:
    resolved = path.resolve().as_posix()
    match = re.match(r"^([A-Za-z]):/(.*)$", resolved)
    if not match:
        raise RuntimeError(f"Cannot translate Windows path to WSL mount: {path}")
    return f"/mnt/{match.group(1).lower()}/{match.group(2)}"


def _container_remap_rustflags() -> str:
    sysroot = f"/usr/local/rustup/toolchains/{PINNED_TOOLCHAIN}-x86_64-unknown-linux-gnu"
    flags = [
        "--remap-path-prefix=/iris=/iris",
        "--remap-path-prefix=/usr/local/cargo=/cargo",
        *[
            f"--remap-path-prefix=/usr/local/cargo/registry/src/{idx}=/cargo/deps"
            for idx in CRATES_IO_INDEX_DIRS
        ],
        "--remap-path-prefix=/usr/local/rustup=/rustup",
        f"--remap-path-prefix={sysroot}=/rustup-sysroot",
        f"--remap-path-prefix={sysroot}/lib/rustlib/src/rust=/rustc/{PINNED_RUSTC_COMMIT}",
    ]
    return "\x1f".join(flags)


def _build_in_container(runtime: str, crates: list[Path], target_dir: Path):
    resolved_root = ROOT.resolve()
    resolved_target = target_dir.resolve()
    registry_cache = resolved_target / "container-cargo-registry"
    resolved_target.mkdir(parents=True, exist_ok=True)
    registry_cache.mkdir(parents=True, exist_ok=True)
    crate_args = " ".join(f"-p {crate.name}" for crate in crates)
    shell_cmd = (
        "rustup target add wasm32-unknown-unknown && "
        f"cargo build --locked --release --target wasm32-unknown-unknown {crate_args} && "
        "chmod -R a+rwX /target/wasm32-unknown-unknown/release"
    )
    subprocess.run(
        [
            runtime,
            "run",
            "--rm",
            "--platform",
            "linux/amd64",
            "-v",
            f"{resolved_root}:/iris:ro",
            "-v",
            f"{resolved_target}:/target",
            "-v",
            f"{registry_cache}:/usr/local/cargo/registry",
            "-w",
            "/iris",
            "-e",
            "CARGO_TARGET_DIR=/target",
            "-e",
            "CARGO_PROFILE_RELEASE_DEBUG=0",
            "-e",
            "CARGO_PROFILE_RELEASE_STRIP=symbols",
            "-e",
            f"CARGO_ENCODED_RUSTFLAGS={_container_remap_rustflags()}",
            PINNED_LINUX_IMAGE,
            "sh",
            "-eu",
            "-c",
            shell_cmd,
        ],
        cwd=ROOT,
        check=True,
    )


def _build_in_wsl(crates: list[Path], target_dir: Path):
    wsl_root = _windows_to_wsl_path(ROOT)
    out_release = target_dir.resolve() / "wasm32-unknown-unknown/release"
    out_release.mkdir(parents=True, exist_ok=True)
    wsl_out = _windows_to_wsl_path(out_release)
    crate_flags = " ".join(f"-p {crate.name}" for crate in crates)
    copy_cmds = " && ".join(
        f'cp "$STAGE/target/wasm32-unknown-unknown/release/{crate.name.replace("-", "_")}.wasm" "{wsl_out}/"'
        for crate in crates
    )
    script = f"""set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
rustup toolchain install {PINNED_TOOLCHAIN} --profile minimal >/dev/null 2>&1 || true
rustup target add --toolchain {PINNED_TOOLCHAIN} wasm32-unknown-unknown
STAGE="$HOME/.cache/iris-official-wasm-stage"
rm -rf "$STAGE/src"
mkdir -p "$STAGE/src" "$STAGE/target"
tar -C "{wsl_root}" --exclude='./target' --exclude='./.git' -cf - . | tar -C "$STAGE/src" -xf -
cd "$STAGE/src"
CARGO_HOME_DIR="${{CARGO_HOME:-$HOME/.cargo}}"
RUSTUP_HOME_DIR="${{RUSTUP_HOME:-$HOME/.rustup}}"
SYSROOT_DIR="$(RUSTUP_TOOLCHAIN={PINNED_TOOLCHAIN} rustc --print sysroot)"
SEP=$(printf '\\x1f')
FLAGS="--remap-path-prefix=$STAGE/src=/iris"
FLAGS="${{FLAGS}}${{SEP}}--remap-path-prefix=$CARGO_HOME_DIR=/cargo"
for IDX in {' '.join(CRATES_IO_INDEX_DIRS)}; do
  FLAGS="${{FLAGS}}${{SEP}}--remap-path-prefix=$CARGO_HOME_DIR/registry/src/$IDX=/cargo/deps"
done
FLAGS="${{FLAGS}}${{SEP}}--remap-path-prefix=$RUSTUP_HOME_DIR=/rustup"
FLAGS="${{FLAGS}}${{SEP}}--remap-path-prefix=$SYSROOT_DIR=/rustup-sysroot"
FLAGS="${{FLAGS}}${{SEP}}--remap-path-prefix=$SYSROOT_DIR/lib/rustlib/src/rust=/rustc/{PINNED_RUSTC_COMMIT}"
env -u RUSTFLAGS \\
  RUSTUP_TOOLCHAIN={PINNED_TOOLCHAIN} \\
  CARGO_TARGET_DIR="$STAGE/target" \\
  CARGO_PROFILE_RELEASE_DEBUG=0 \\
  CARGO_PROFILE_RELEASE_STRIP=symbols \\
  CARGO_ENCODED_RUSTFLAGS="$FLAGS" \\
  cargo build --locked --release --target wasm32-unknown-unknown {crate_flags}
{copy_cmds}
"""
    subprocess.run(["wsl", "-e", "bash", "-c", script], cwd=ROOT, check=True)


def _build_native(crates: list[Path], target_dir: Path, base_env: dict[str, str]) -> dict[str, str]:
    build_env = base_env.copy()
    build_env["CARGO_TARGET_DIR"] = str(target_dir)
    build_env.pop("RUSTFLAGS", None)
    build_env["CARGO_PROFILE_RELEASE_DEBUG"] = "0"
    build_env["CARGO_PROFILE_RELEASE_STRIP"] = "symbols"
    flags = build_remap_rustflags(build_env)
    if _is_windows_host():
        remap_args = write_remap_args_file(target_dir, build_env)
        flags = f"{flags}\x1f@{remap_args}"
    build_env["CARGO_ENCODED_RUSTFLAGS"] = flags
    subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            *[part for crate in crates for part in ("-p", crate.name)],
        ],
        cwd=ROOT,
        env=build_env,
        check=True,
    )
    return build_env


def verify_wasm_no_host_paths(
    wasm_bytes: bytes,
    env: dict[str, str] | None = None,
    extra_prefixes: tuple[Path, ...] = (),
):
    if _WINDOWS_USERS_RE.search(wasm_bytes):
        raise RuntimeError("plugin.wasm leaked a Windows Users directory path")
    for marker in (
        b"/Users/",
        b"/home/",
        b"/rustup-sysroot",
        b"/iris\\",
        b"/cargo\\",
        b"/rustup\\",
        b"crates\\",
        b"\\src\\",
        b"\\library\\",
    ):
        if marker in wasm_bytes:
            raise RuntimeError(f"plugin.wasm leaked host or non-POSIX path prefix {marker.decode('ascii')!r}")
    candidates: list[Path] = [ROOT]
    for home_dir in (_cargo_home(env), _rustup_home(env), _rustc_sysroot(env)):
        if home_dir is not None:
            candidates.append(home_dir)
    candidates.extend(extra_prefixes)
    lowered = wasm_bytes.lower() if _is_windows_host() else None
    for candidate in candidates:
        for form in {str(candidate), candidate.as_posix()}:
            if len(form) < 4:
                continue
            needle = form.encode("utf-8")
            if needle in wasm_bytes or (lowered is not None and needle.lower() in lowered):
                raise RuntimeError(f"plugin.wasm leaked host directory prefix: {form}")


def _canonical_manifest_bytes(manifest: dict) -> bytes:
    return (
        json.dumps(manifest, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n"
    ).encode("utf-8")


def _normalize_network(raw) -> dict:
    _require(isinstance(raw, dict), f"network declaration must be an object, got {type(raw).__name__}")
    unknown = set(raw) - set(DEFAULT_NETWORK)
    _require(not unknown, f"unknown network declaration fields: {sorted(unknown)}")
    return {
        "net_public": bool(raw.get("net_public", False)),
        "net_hosts": list(raw.get("net_hosts", [])),
        "public_ports": list(raw.get("public_ports", [])),
        "tcp_targets": list(raw.get("tcp_targets", [])),
    }


def validate_official_manifest(manifest: dict):
    plugin_id = manifest.get("id")
    _require(plugin_id in FROZEN_OFFICIAL_CAPABILITIES, f"Unexpected official plugin id: {plugin_id!r}")
    allowed_permissions, allowed_network = FROZEN_OFFICIAL_CAPABILITIES[plugin_id]
    permissions = manifest.get("permissions", [])
    _require(
        isinstance(permissions, list) and all(isinstance(p, str) for p in permissions),
        f"{plugin_id}: permissions must be a list of strings",
    )
    extra_permissions = sorted(set(permissions) - allowed_permissions)
    _require(
        not extra_permissions,
        f"{plugin_id}: official plugin expanded permissions beyond reviewed baseline: {extra_permissions}",
    )
    network = _normalize_network(manifest.get("network", {}))
    _require(
        network == allowed_network,
        f"{plugin_id}: official plugin changed network declaration beyond reviewed baseline: {network!r}",
    )
    sensitive_keys = manifest.get("sensitive_keys", [])
    _require(
        sensitive_keys == [],
        f"{plugin_id}: official plugin declared unreviewed sensitive_keys: {sensitive_keys!r}",
    )


OFFICIAL_SOURCE_CRATES = ("iris-api", "iris-plugin-sdk", "iris-plugin-guides", "iris-plugin-webdav")


def lock_closure(lock_text: str, roots) -> bytes:
    """Canonical bytes of the Cargo.lock packages reachable from `roots`.

    Lock entries list every dependency kind for every target, so the closure is a
    superset of what the Wasm build compiles. Host-only packages stay out of it.
    """
    lock = tomllib.loads(lock_text)
    by_name: dict[str, list[dict]] = {}
    for item in lock.get("package", []):
        by_name.setdefault(item["name"], []).append(item)

    def resolve(spec: str) -> dict:
        # Dependency entries are "name", "name version" or "name version (source)".
        name, _, rest = spec.partition(" ")
        version, _, source = rest.partition(" ")
        candidates = [
            item for item in by_name.get(name, [])
            if (not version or item["version"] == version)
            and (not source or f"({item.get('source', '')})" == source)
        ]
        _require(len(candidates) == 1, f"Cargo.lock entry {spec!r} does not name exactly one package")
        return candidates[0]

    seen: dict[tuple[str, str, str], dict] = {}
    pending = [resolve(name) for name in roots]
    while pending:
        item = pending.pop()
        key = (item["name"], item["version"], item.get("source", ""))
        if key not in seen:
            seen[key] = item
            pending.extend(resolve(spec) for spec in item.get("dependencies", []))
    closure = {"version": lock.get("version"), "package": [seen[key] for key in sorted(seen)]}
    return json.dumps(closure, sort_keys=True, separators=(",", ":")).encode("utf-8")


def _collect_inherited_workspace_deps(manifest: dict) -> set[str]:
    inherited: set[str] = set()
    tables = [
        manifest.get("dependencies", {}),
        manifest.get("build-dependencies", {}),
    ]
    target_table = manifest.get("target", {})
    if isinstance(target_table, dict):
        for cfg_table in target_table.values():
            if isinstance(cfg_table, dict):
                tables.append(cfg_table.get("dependencies", {}))
                tables.append(cfg_table.get("build-dependencies", {}))
    for table in tables:
        if isinstance(table, dict):
            for dep_key, spec in table.items():
                if isinstance(spec, dict) and spec.get("workspace") is True:
                    inherited.add(dep_key)
    return inherited


def root_cargo_closure(root_cargo_text: str, crate_cargo_texts) -> bytes:
    """Canonical bytes of root Cargo.toml sections that affect official Wasm plugin builds.

    Ignores host-only workspace members, default-members, [workspace.lints], [profile.dev],
    and [workspace.dependencies] entries not inherited (`workspace = true`) by any official
    plugin source crate.
    """
    root_manifest = tomllib.loads(root_cargo_text)
    workspace = root_manifest.get("workspace", {})
    ws_deps = workspace.get("dependencies", {})

    inherited_keys: set[str] = set()
    for text in crate_cargo_texts:
        inherited_keys |= _collect_inherited_workspace_deps(tomllib.loads(text))

    missing = sorted(inherited_keys - set(ws_deps))
    _require(not missing, f"Official plugin crates inherit missing workspace.dependencies: {missing}")

    filtered: dict[str, object] = {
        "workspace": {
            "resolver": workspace.get("resolver"),
            "package": workspace.get("package"),
            "dependencies": {key: ws_deps[key] for key in sorted(inherited_keys)},
        },
        "profile_release": root_manifest.get("profile", {}).get("release"),
    }
    if root_manifest.get("patch"):
        filtered["patch"] = root_manifest["patch"]
    if root_manifest.get("replace"):
        filtered["replace"] = root_manifest["replace"]
    return json.dumps(filtered, sort_keys=True, separators=(",", ":")).encode("utf-8")


def source_hash() -> str:
    paths = [ROOT / name for name in ("rust-toolchain.toml", "scripts/build-official-plugins.py")]
    crate_cargo_texts: list[str] = []
    for name in OFFICIAL_SOURCE_CRATES:
        crate = ROOT / "crates" / name
        crate_manifest = crate / "Cargo.toml"
        crate_cargo_texts.append(crate_manifest.read_text(encoding="utf-8"))
        paths += [crate_manifest, *sorted((crate / "src").rglob("*.rs"))]
        if (crate / "plugin.json").is_file():
            paths.append(crate / "plugin.json")
    for cargo_cfg_name in (".cargo/config.toml", ".cargo/config"):
        cargo_cfg = ROOT / cargo_cfg_name
        if cargo_cfg.is_file():
            paths.append(cargo_cfg)
    # Git checkouts may use CRLF. The digest describes source content.
    entries = [(path.relative_to(ROOT).as_posix(), path.read_bytes().replace(b"\r\n", b"\n")) for path in paths]
    root_cargo_text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    entries.append(("Cargo.toml#official-plugins", root_cargo_closure(root_cargo_text, crate_cargo_texts)))
    # Only the plugins' own dependency closure is stamped, so a host-only
    # dependency bump does not force rebuilding the committed archives.
    lock_text = (ROOT / "Cargo.lock").read_text(encoding="utf-8")
    entries.append(("Cargo.lock#official-plugins", lock_closure(lock_text, OFFICIAL_SOURCE_CRATES)))
    digest = hashlib.sha256()
    for name, data in sorted(entries):
        encoded = name.encode("utf-8")
        digest.update(len(encoded).to_bytes(8, "little") + encoded)
        digest.update(len(data).to_bytes(8, "little") + data)
    return digest.hexdigest()


def check(output: Path):
    raw_index = (output / "index.json").read_bytes()
    _require(b"\r" not in raw_index, "Official plugin index.json must use LF line endings only")
    index = json.loads(raw_index.decode("utf-8"))
    _require(
        index["schema"] == 1 and index["source_sha256"] == source_hash(),
        "Official Wasm source stamp is stale; rebuild the packages",
    )
    _require({p["id"] for p in index["packages"]} == {"iris.guides", "iris.webdav"}, "Unexpected official package set")
    _require(len(index["packages"]) == 2, "Expected exactly two official packages")
    for item in index["packages"]:
        _require(item["file"] == item["id"] + ".irisplugin", f"Unexpected package filename: {item['file']!r}")
        archive = output / item["file"]
        _require(hashlib.sha256(archive.read_bytes()).hexdigest() == item["sha256"], f"Archive sha256 mismatch: {archive}")
        digest = hashlib.sha256()
        with zipfile.ZipFile(archive) as package_file:
            _require(sorted(package_file.namelist()) == ["plugin.json", "plugin.wasm"], f"Unexpected entries in {archive}")
            for name in sorted(package_file.namelist()):
                encoded, data = name.encode("ascii"), package_file.read(name)
                digest.update(len(encoded).to_bytes(8, "little") + encoded)
                digest.update(len(data).to_bytes(8, "little") + data)
            wasm_bytes = package_file.read("plugin.wasm")
            _require(wasm_bytes.startswith(b"\x00asm\x01\x00\x00\x00"), f"Invalid Wasm header in {archive}")
            verify_wasm_no_host_paths(wasm_bytes)
            manifest = json.loads(package_file.read("plugin.json").decode("utf-8"))
            _require(manifest["id"] == item["id"] and manifest["version"] == item["version"], f"Manifest identity mismatch in {archive}")
            _require(manifest["protocol"] == manifest["abi"] == 2, f"Unsupported protocol/abi in {archive}")
            validate_official_manifest(manifest)
        _require(digest.hexdigest() == item["content_sha256"], f"Archive content_sha256 mismatch: {archive}")
    return index


def verify_embedded(fresh_index: dict, crates: list[Path], embedded_dir: Path, fresh_dir: Path | None = None):
    embedded = check(embedded_dir)
    _require(fresh_index["schema"] == embedded["schema"], "Official plugin index schema mismatch")
    _require(
        fresh_index["source_sha256"] == embedded["source_sha256"],
        "Official Wasm source stamp differs from embedded distribution",
    )
    fresh_meta = [(p["id"], p["version"], p["file"]) for p in fresh_index["packages"]]
    embedded_meta = [(p["id"], p["version"], p["file"]) for p in embedded["packages"]]
    _require(fresh_meta == embedded_meta, "Official plugin package metadata differs from embedded distribution")
    for crate in crates:
        source_manifest = json.loads((crate / "plugin.json").read_text(encoding="utf-8"))
        expected_manifest_bytes = _canonical_manifest_bytes(source_manifest)
        embedded_archive = embedded_dir / f"{source_manifest['id']}.irisplugin"
        with zipfile.ZipFile(embedded_archive) as package_file:
            _require(
                package_file.read("plugin.json") == expected_manifest_bytes,
                f"Embedded plugin.json differs from source manifest for {source_manifest['id']}",
            )
    _require(fresh_index == embedded, "Fresh Wasm build differs from embedded distribution")
    if fresh_dir is not None:
        _require(
            (fresh_dir / "index.json").read_bytes() == (embedded_dir / "index.json").read_bytes(),
            "Fresh index.json bytes differ from embedded distribution",
        )
        for item in embedded["packages"]:
            _require(
                (fresh_dir / item["file"]).read_bytes() == (embedded_dir / item["file"]).read_bytes(),
                f"Fresh {item['file']} archive bytes differ from embedded distribution",
            )
    return embedded


def package(crate: Path, output: Path, target_dir: Path | None = None, env: dict[str, str] | None = None) -> dict:
    manifest = json.loads((crate / "plugin.json").read_text(encoding="utf-8"))
    _require(manifest["protocol"] == manifest["abi"] == 2, f"{crate}: protocol/abi must be 2")
    _require(manifest["id"].startswith("iris.") and manifest["entry"] == "plugin.wasm", f"{crate}: invalid official entry")
    validate_official_manifest(manifest)
    resolved_target = cargo_target_dir(env) if target_dir is None else target_dir
    wasm = resolved_target / "wasm32-unknown-unknown/release" / (crate.name.replace("-", "_") + ".wasm")
    data = wasm.read_bytes()
    _require(data.startswith(b"\x00asm\x01\x00\x00\x00"), f"{wasm}: invalid Wasm magic")
    verify_wasm_no_host_paths(data, env=env)
    output.mkdir(parents=True, exist_ok=True)
    target = output / (manifest["id"] + ".irisplugin")
    parts = {"plugin.json": _canonical_manifest_bytes(manifest), "plugin.wasm": data}
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


def _select_backend(requested: str, needs_canonical: bool) -> tuple[str, str | None]:
    if requested == "native":
        return ("native", None)
    if requested == "container":
        runtime = _find_container_runtime()
        if runtime is None:
            raise RuntimeError("Container backend requested, but neither docker nor podman Linux runtime is available")
        return ("container", runtime)
    if requested == "wsl":
        if not _wsl_linux_x86_64_available():
            raise RuntimeError("WSL backend requested, but WSL2 Linux x86_64 is unavailable")
        return ("wsl", None)
    if _is_linux_x86_64():
        return ("native", None)
    runtime = _find_container_runtime()
    if runtime is not None:
        return ("container", runtime)
    if _wsl_linux_x86_64_available():
        return ("wsl", None)
    if needs_canonical:
        raise RuntimeError(
            "Canonical official plugin builds target x86_64-unknown-linux-gnu; install Docker/Podman "
            "(linux/amd64) or WSL2 x86_64, pass --verified-dir / IRIS_VERIFIED_OFFICIAL_PLUGINS, "
            "or pass --native-host for non-distribution scratch builds"
        )
    return ("native", None)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/plugin-v2/official")
    parser.add_argument("--no-build", action="store_true", help="Package already-built artifacts")
    parser.add_argument("--check", action="store_true", help="Check hashes and source freshness without building")
    parser.add_argument("--verify-embedded", action="store_true", help="Compare fresh build with checked-in distribution bytes")
    parser.add_argument("--backend", choices=("auto", "container", "wsl", "native"), default="auto")
    parser.add_argument("--container", action="store_true", help="Build inside pinned linux/amd64 Rust container")
    parser.add_argument("--native-host", action="store_true", help="Build with the current host OS rustc")
    parser.add_argument("--verified-dir", type=Path, default=None, help="Pre-built canonical output directory to compare byte-for-byte")
    args = parser.parse_args()
    if args.container and args.native_host:
        parser.error("--container and --native-host are mutually exclusive")
    if args.check:
        print(json.dumps(check(args.output), indent=2))
        return
    embedded_dir = (ROOT / "Resources/Plugins").resolve()
    output_resolved = args.output.resolve()
    if args.verify_embedded and output_resolved == embedded_dir:
        parser.error("--verify-embedded requires an output directory separate from Resources/Plugins")
    crates = sorted(p.parent for p in (ROOT / "crates").glob("iris-plugin-*/plugin.json"))
    if not crates:
        raise SystemExit("No official plugin sources found")

    requested_backend = "container" if args.container else ("native" if args.native_host else args.backend)
    env_verified = os.environ.get("IRIS_VERIFIED_OFFICIAL_PLUGINS", "").strip()
    verified_dir = args.verified_dir or (Path(env_verified) if env_verified else None)

    target_dir = cargo_target_dir()
    build_env = os.environ.copy()

    # When a CI job on macOS/Windows receives the canonical Linux container output via
    # IRIS_VERIFIED_OFFICIAL_PLUGINS, verify those fresh archives byte-for-byte against
    # Resources/Plugins AND still compile native Wasm on this host to guard path/ABI hygiene.
    if (
        args.verify_embedded
        and verified_dir is not None
        and requested_backend == "auto"
        and not _is_linux_x86_64()
    ):
        verified_resolved = verified_dir.resolve()
        _require(verified_resolved != embedded_dir, "verified-dir must be separate from Resources/Plugins")
        verified_index = check(verified_resolved)
        verify_embedded(verified_index, crates, ROOT / "Resources/Plugins", fresh_dir=verified_resolved)
        if not args.no_build:
            build_env = _build_native(crates, target_dir, build_env)
            native_index = {
                "schema": 1,
                "source_sha256": source_hash(),
                "packages": [package(crate, args.output, target_dir, build_env) for crate in crates],
            }
            with (args.output / "index.json").open("w", encoding="utf-8", newline="\n") as fh:
                fh.write(json.dumps(native_index, indent=2) + "\n")
        print(json.dumps(verified_index, indent=2))
        return

    if not args.no_build:
        needs_canonical = args.verify_embedded or output_resolved == embedded_dir
        backend, runtime = _select_backend(requested_backend, needs_canonical)
        if backend == "container":
            assert runtime is not None
            _build_in_container(runtime, crates, target_dir)
        elif backend == "wsl":
            _build_in_wsl(crates, target_dir)
        else:
            build_env = _build_native(crates, target_dir, build_env)

    index = {"schema": 1, "source_sha256": source_hash(), "packages": [package(crate, args.output, target_dir, build_env) for crate in crates]}
    with (args.output / "index.json").open("w", encoding="utf-8", newline="\n") as fh:
        fh.write(json.dumps(index, indent=2) + "\n")
    print(json.dumps(index, indent=2))
    if args.verify_embedded:
        verify_embedded(index, crates, ROOT / "Resources/Plugins", fresh_dir=args.output)


if __name__ == "__main__":
    main()
