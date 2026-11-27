#!/usr/bin/env python3
"""Fast, zero-network architectural & engineering invariant gates for Iris."""
import ast
import json
import re
import subprocess
from pathlib import Path
import sys
import tomllib

sys.dont_write_bytecode = True
if not __debug__:
    raise RuntimeError("Optimization (-O / PYTHONOPTIMIZE) disables project invariant gates")

ROOT = Path(__file__).resolve().parents[1]

PROD_WARN_LINES = 650
PROD_HARD_MAX_LINES = 850
SRC_TOTAL_HARD_MAX_LINES = 1000
TEST_FILE_HARD_MAX_LINES = 1000


def _require(condition: bool, message: str):
    if not condition:
        raise AssertionError(message)


# Dependencies with no released fix, and the exact (code, message) warnings
# each one may report. A new package or a new warning still fails the gate.
KNOWN_FUTURE_INCOMPATIBILITIES = {
    # GPUI 0.2.2 pulls in stacksafe-macro 0.1.4. Its proc-macro-error2
    # dependency has no released fix for rust-lang/rust#127909.
    "proc-macro-error2@2.0.1": [
        ("E0365", "extern crate `proc_macro` is private and cannot be re-exported"),
    ],
    # macOS only: GPUI 0.2.2 pulls in block 0.1.6 directly and through cocoa,
    # core-video and metal. It is the crate's last release (superseded by
    # block2), and its `_NSConcreteStackBlock` static has an uninhabited type,
    # rust-lang/rust#74840.
    "block@0.1.6": [
        ("", "static of uninhabited type"),
    ],
}
FUTURE_INCOMPAT_WARNING_RE = re.compile(r"(?m)^> warning(?:\[([^]]+)\])?: ([^\r\n]+)")


def check_future_incompatibilities(report):
    if not report.is_file():
        return
    data = json.loads(report.read_text(encoding="utf-8"))
    if data["version"] != 0:
        raise RuntimeError(f"Unknown Cargo future incompatibility report format: {report}")
    for entry in data["reports"]:
        for package, warning in entry["per_package"].items():
            warning = re.sub(r"\x1b\[[0-9;]*m", "", warning)
            if FUTURE_INCOMPAT_WARNING_RE.findall(warning) != KNOWN_FUTURE_INCOMPATIBILITIES.get(package):
                raise RuntimeError(f"Unexpected Cargo future incompatibility in {package}:\n{warning}")


TEST_MOD_RE = re.compile(
    r"(?m)^[ \t]*#\[cfg\(test\)\]\s*(?:#\[[^\]]+\]\s*)*mod\s+\w+\s*(?:\{|;)"
)


def strip_comments(rust_src: str) -> str:
    """Strip Rust // and nested /* ... */ comments while preserving string/char literals and newlines."""
    out = []
    i = 0
    n = len(rust_src)
    while i < n:
        ch = rust_src[i]
        nxt = rust_src[i + 1] if i + 1 < n else ""
        # Line comment
        if ch == "/" and nxt == "/":
            i += 2
            while i < n and rust_src[i] != "\n":
                i += 1
            continue
        # Nested block comment
        if ch == "/" and nxt == "*":
            depth = 1
            i += 2
            while i < n and depth > 0:
                if rust_src[i] == "/" and i + 1 < n and rust_src[i + 1] == "*":
                    depth += 1
                    i += 2
                elif rust_src[i] == "*" and i + 1 < n and rust_src[i + 1] == "/":
                    depth -= 1
                    i += 2
                else:
                    if rust_src[i] == "\n":
                        out.append("\n")
                    i += 1
            continue
        # Raw string literal: r#"..."#, br#"..."#, cr#"..."#
        if ch in ("r", "b", "c") and (i == 0 or not (rust_src[i - 1].isalnum() or rust_src[i - 1] == "_")):
            j = i
            if ch in ("b", "c") and j + 1 < n and rust_src[j + 1] == "r":
                j += 1
            if rust_src[j] == "r":
                k = j + 1
                while k < n and rust_src[k] == "#":
                    k += 1
                if k < n and rust_src[k] == '"':
                    hashes = k - (j + 1)
                    end_marker = '"' + ("#" * hashes)
                    end = rust_src.find(end_marker, k + 1)
                    if end == -1:
                        out.append(rust_src[i:])
                        break
                    out.append(rust_src[i : end + len(end_marker)])
                    i = end + len(end_marker)
                    continue
        # Normal/byte/c string literal
        if ch == '"':
            start = i
            i += 1
            while i < n:
                if rust_src[i] == "\\":
                    i += 2
                elif rust_src[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
            out.append(rust_src[start:i])
            continue
        # Char literal (distinguished from Rust lifetimes like 'a or 'static)
        if ch == "'":
            if i + 3 < n and rust_src[i + 1] == "\\":
                end = rust_src.find("'", i + 3)
                if end != -1 and "\n" not in rust_src[i:end]:
                    out.append(rust_src[i : end + 1])
                    i = end + 1
                    continue
            elif i + 2 < n and rust_src[i + 1] not in ("'", "\n") and rust_src[i + 2] == "'":
                out.append(rust_src[i : i + 3])
                i += 3
                continue
        out.append(ch)
        i += 1
    return "".join(out)


def find_matching_brace(rust_src: str, open_idx: int) -> int:
    """Find matching `}` for `{` at `open_idx`, skipping comments and string/char literals."""
    n = len(rust_src)
    _require(0 <= open_idx < n and rust_src[open_idx] == "{", "expected `{` at open_idx")
    depth = 0
    i = open_idx
    while i < n:
        ch = rust_src[i]
        nxt = rust_src[i + 1] if i + 1 < n else ""
        if ch == "/" and nxt == "/":
            i += 2
            while i < n and rust_src[i] != "\n":
                i += 1
            continue
        if ch == "/" and nxt == "*":
            blk = 1
            i += 2
            while i < n and blk > 0:
                if rust_src[i] == "/" and i + 1 < n and rust_src[i + 1] == "*":
                    blk += 1
                    i += 2
                elif rust_src[i] == "*" and i + 1 < n and rust_src[i + 1] == "/":
                    blk -= 1
                    i += 2
                else:
                    i += 1
            continue
        if ch in ("r", "b", "c") and (i == 0 or not (rust_src[i - 1].isalnum() or rust_src[i - 1] == "_")):
            j = i
            if ch in ("b", "c") and j + 1 < n and rust_src[j + 1] == "r":
                j += 1
            if rust_src[j] == "r":
                k = j + 1
                while k < n and rust_src[k] == "#":
                    k += 1
                if k < n and rust_src[k] == '"':
                    end_marker = '"' + ("#" * (k - (j + 1)))
                    end = rust_src.find(end_marker, k + 1)
                    if end == -1:
                        break
                    i = end + len(end_marker)
                    continue
        if ch == '"':
            i += 1
            while i < n:
                if rust_src[i] == "\\":
                    i += 2
                elif rust_src[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
            continue
        if ch == "'":
            if i + 3 < n and rust_src[i + 1] == "\\":
                end = rust_src.find("'", i + 3)
                if end != -1 and "\n" not in rust_src[i:end]:
                    i = end + 1
                    continue
            elif i + 2 < n and rust_src[i + 1] not in ("'", "\n") and rust_src[i + 2] == "'":
                i += 3
                continue
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    raise AssertionError("Unclosed `{` in `#[cfg(test)] mod` block")


def prod_prefix(text: str) -> str:
    stripped = strip_comments(text)
    test_mod = TEST_MOD_RE.search(stripped)
    return stripped[: test_mod.start()] if test_mod else stripped


def is_standalone_test_file(path: Path, rel: Path) -> bool:
    return (
        "/tests/" in rel.as_posix()
        or "tests" in rel.parts
        or path.name in ("tests.rs", "test_util.rs", "fixtures.rs")
    )


def is_under(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


POISON_UNWRAP_RE = re.compile(r"\.(?:lock|read|write)\(\)\s*\.(?:unwrap|expect)\(")
RAW_THREAD_SPAWN_RE = re.compile(
    r"\bthread::(?:spawn|Builder)\b"
    r"|\buse\s+std::thread::(?:\{[^}]*\b(?:spawn|Builder)\b|(?:spawn|Builder)\b|\*)"
)
THREAD_ROLE_COUNT_RE = re.compile(r"\bpub\s+const\s+COUNT\s*:\s*usize\s*=\s*([0-9]+)\s*;")
THREAD_ROLE_NAME_RE = re.compile(r'Self::([A-Za-z0-9_]+)\s*=>\s*"([^"]+)"')


def check_lock_poisoning(rel: Path, text: str):
    """Production code recovers a poisoned lock instead of cascading one panic into every later caller."""
    match = POISON_UNWRAP_RE.search(prod_prefix(text))
    _require(
        match is None,
        f"{rel} unwraps a lock result in production code; use `.unwrap_or_else(PoisonError::into_inner)`",
    )


def check_thread_spawns(rel: Path, text: str):
    """Production code must spawn OS threads only via `iris_net::concurrency::{spawn_thread, spawn_thread_named}`."""
    if rel.as_posix() == "crates/iris-net/src/concurrency.rs":
        return
    match = RAW_THREAD_SPAWN_RE.search(prod_prefix(text))
    _require(
        match is None,
        f"{rel} spawns an unregistered OS thread via `{match.group(0) if match else ''}`; "
        "use `iris_net::concurrency::{spawn_thread, spawn_thread_named}`",
    )


def check_thread_inventory_docs(concurrency_src: str, arch_doc: str):
    """Every `ThreadRole::base_name` in `concurrency.rs` must be documented in `docs/ARCHITECTURE.md`."""
    stripped = prod_prefix(concurrency_src)
    count_match = THREAD_ROLE_COUNT_RE.search(stripped)
    _require(count_match is not None, "concurrency.rs missing `ThreadRole::COUNT`")
    expected_count = int(count_match.group(1))
    entries = THREAD_ROLE_NAME_RE.findall(stripped)
    _require(
        len(entries) == expected_count,
        f"ThreadRole::base_name arms ({len(entries)}) must match ThreadRole::COUNT ({expected_count})",
    )
    missing = sorted({name for _, name in entries if f"`{name}" not in arch_doc})
    _require(
        not missing,
        f"docs/ARCHITECTURE.md missing documented ThreadRole names: {', '.join(missing)}",
    )


def check_file_line_budget(rel: Path, text: str) -> int | None:
    """Validate file line budgets and trailing `#[cfg(test)] mod` placement; return prod lines."""
    total_lines = len(text.splitlines())
    if "/tests/" in rel.as_posix() or rel.name == "tests.rs":
        _require(
            total_lines <= TEST_FILE_HARD_MAX_LINES,
            f"{rel} has {total_lines} lines (exceeds test file limit {TEST_FILE_HARD_MAX_LINES})",
        )
        return None
    stripped = strip_comments(text)
    test_mod = TEST_MOD_RE.search(stripped)
    prod_part = stripped[: test_mod.start()] if test_mod else stripped
    _require(
        "#[cfg(test)]" not in prod_part,
        f"{rel} places `#[cfg(test)]` outside trailing `mod tests`",
    )
    if test_mod:
        tail = stripped[test_mod.end() :]
        _require("#[cfg(test)]" not in tail, f"{rel} has multiple `#[cfg(test)]` blocks")
        if stripped[test_mod.end() - 1] == ";":
            _require(
                not tail.strip().strip("}").strip(),
                f"{rel} has production code after `mod tests;`",
            )
        else:
            close_idx = find_matching_brace(stripped, test_mod.end() - 1)
            after_mod = stripped[close_idx + 1 :]
            _require(
                not after_mod.strip().strip("}").strip(),
                f"{rel} has production code after inline `mod tests` block",
            )
        prod_lines = len(prod_part.splitlines())
    else:
        prod_lines = total_lines
    _require(
        prod_lines <= PROD_HARD_MAX_LINES,
        f"{rel} has {prod_lines} production lines (hard limit {PROD_HARD_MAX_LINES})",
    )
    _require(
        total_lines <= SRC_TOTAL_HARD_MAX_LINES,
        f"{rel} has {total_lines} total lines (hard limit {SRC_TOTAL_HARD_MAX_LINES})",
    )
    return prod_lines


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--future-incompat-report":
        check_future_incompatibilities(Path(sys.argv[2]))
        return

    # 1. Python syntax across scripts/ and examples/ + packaging unit tests
    for directory in ("scripts", "examples"):
        base = ROOT / directory
        if base.is_dir():
            for script in sorted(base.rglob("*.py")):
                ast.parse(script.read_text(encoding="utf-8"), filename=str(script))
    subprocess.run(
        [sys.executable, "-B", "-m", "unittest", "scripts/test_package.py"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )

    # 2. Recursive UI & app layer isolation (`clippy.toml` `disallowed_*`, no wildcard super, no UI target_os)
    iris_src = ROOT / "crates/iris/src"
    video_dir = iris_src / "video"
    ui_dir = iris_src / "ui"

    root_cargo = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    clippy_lints = root_cargo.get("workspace", {}).get("lints", {}).get("clippy", {})
    for lint in ("disallowed_methods", "disallowed_types"):
        _require(
            clippy_lints.get(lint) == "deny",
            f"Workspace Cargo.toml must deny `clippy::{lint}`",
        )
    iris_clippy = (ROOT / "crates/iris/clippy.toml").read_text(encoding="utf-8")
    for required_rule in (
        "iris_mpv::Mpv",
        "iris_mpv::RenderHandle",
        "iris_mpv::RenderHandle::new",
        "iris_core::player::Session::render_handle",
    ):
        _require(required_rule in iris_clippy, f"crates/iris/clippy.toml must disallow `{required_rule}`")
    core_clippy = (ROOT / "crates/iris-core/clippy.toml").read_text(encoding="utf-8")
    _require(
        "iris_mpv::RenderHandle::new" in core_clippy,
        "crates/iris-core/clippy.toml must disallow `iris_mpv::RenderHandle::new`",
    )

    for path in sorted(iris_src.rglob("*.rs")):
        rel = path.relative_to(ROOT)
        raw = path.read_text(encoding="utf-8")
        code = strip_comments(raw)
        if not is_under(path, video_dir):
            _require(
                not re.search(r"\b(?:allow|expect)\s*\([^)]*disallowed_(?:methods|types)", code),
                f"{rel} suppresses `clippy::disallowed_*` outside `crates/iris/src/video/`",
            )
        if not is_standalone_test_file(path, rel):
            prod_code = prod_prefix(raw)
            _require("use super::*;" not in prod_code, f"{rel} uses forbidden wildcard `use super::*;`")
        if is_under(path, ui_dir):
            _require(not re.search(r"\btarget_os\b", code), f"{rel} scatters `target_os` cfg in UI layer")

    # 3. Cargo metadata: devtools security gates, layer dependency boundaries & workspace lints
    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--offline", "--locked", "--no-deps", "--format-version", "1"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )
    pkgs = {p["name"]: p for p in metadata["packages"]}
    version = pkgs["iris"]["version"]
    _require(bool(re.fullmatch(r"0\.0\.(0|[1-9][0-9]*)", version)), "Only 0.0.x Iris builds are authorized")
    _require(all(p["version"] == version for p in pkgs.values()), "Workspace package versions differ")

    for crate_name in ("iris", "iris-core"):
        default_feats = pkgs[crate_name]["features"].get("default", [])
        _require("devtools" not in default_feats, f"{crate_name} enables devtools by default: {default_feats}")

    for gated_file in (
        ROOT / "crates/iris/src/debug.rs",
        ROOT / "crates/iris-core/src/player/mod.rs",
    ):
        _require(
            'feature = "devtools"' in gated_file.read_text(encoding="utf-8"),
            f"{gated_file.relative_to(ROOT)} missing devtools cfg gate",
        )

    vault = ROOT / "crates/iris-core/src/vault.rs"
    guard = r'#\[cfg\(any\(debug_assertions, feature = "devtools"\)\)\]'
    _require(
        bool(re.search(guard + r'\s+if let Some\(p\) = std::env::var_os\("IRIS_TEST_VAULT"\)', vault.read_text(encoding="utf-8"))),
        "IRIS_TEST_VAULT must be gated out of production builds",
    )

    for name, pkg in pkgs.items():
        manifest_text = Path(pkg["manifest_path"]).read_text(encoding="utf-8")
        _require(
            bool(re.search(r"(?m)^\[lints\]\s+workspace\s*=\s*true\b", manifest_text)),
            f"{name} must inherit `[lints] workspace = true`",
        )
        all_deps = {d["name"] for d in pkg["dependencies"]}
        _require("bindgen" not in all_deps, f"{name} must not depend on bindgen")
        _require("tokio" not in all_deps, f"{name} must not depend directly on tokio")

    mpv_normal = [d["name"] for d in pkgs["iris-mpv"]["dependencies"] if d["kind"] is None]
    _require(mpv_normal == [], f"iris-mpv must stay zero-dependency, got {mpv_normal}")

    api_normal = [d["name"] for d in pkgs["iris-api"]["dependencies"] if d["kind"] is None]
    _require(api_normal == ["serde"], f"iris-api normal deps must be ['serde'] only, got {api_normal}")

    forbidden_headless = {
        "gpui",
        "cocoa",
        "objc",
        "objc2",
        "raw-window-handle",
        "winit",
        "windows",
        "image",
        "iris-plugin-sdk",
    }
    net_deps = {d["name"] for d in pkgs["iris-net"]["dependencies"]}
    forbidden_net = forbidden_headless | {"iris-core", "iris-plugin-host", "iris-api", "iris-mpv"}
    _require(not (net_deps & forbidden_net), f"iris-net must stay domain-free, found {net_deps & forbidden_net}")

    core_deps = {d["name"] for d in pkgs["iris-core"]["dependencies"]}
    forbidden_core = forbidden_headless | {"iris-plugin-host", "iris-api", "wasmtime"}
    _require(not (core_deps & forbidden_core), f"iris-core must stay headless and plugin-free, found {core_deps & forbidden_core}")

    host_deps = {d["name"] for d in pkgs["iris-plugin-host"]["dependencies"]}
    forbidden_host = forbidden_headless | {"iris-mpv"}
    _require(not (host_deps & forbidden_host), f"iris-plugin-host must stay headless and mpv-free, found {host_deps & forbidden_host}")

    # 3b. Wildcard-super ban across all headless & guest crates
    for crate_dir in (
        ROOT / "crates/iris-net/src",
        ROOT / "crates/iris-core/src",
        ROOT / "crates/iris-plugin-host/src",
        ROOT / "crates/iris-api/src",
        ROOT / "crates/iris-mpv/src",
        ROOT / "crates/iris-plugin-sdk/src",
        ROOT / "crates/iris-plugin-guides/src",
        ROOT / "crates/iris-plugin-webdav/src",
    ):
        for path in sorted(crate_dir.rglob("*.rs")):
            rel = path.relative_to(ROOT)
            if not is_standalone_test_file(path, rel):
                prod_code = prod_prefix(path.read_text(encoding="utf-8"))
                _require("use super::*;" not in prod_code, f"{rel} uses forbidden wildcard `use super::*;`")

    # 4. File line-count budgets, lock poisoning & registered OS thread spawns
    soft_warnings = []
    for path in sorted([*(ROOT / "crates").rglob("*.rs"), *(ROOT / "examples/plugins").rglob("*.rs")]):
        rel = path.relative_to(ROOT)
        text = path.read_text(encoding="utf-8")
        prod_lines = check_file_line_budget(rel, text)
        if prod_lines is not None and prod_lines > PROD_WARN_LINES:
            soft_warnings.append(f"{rel} ({prod_lines} prod lines)")
        if not is_standalone_test_file(path, rel):
            check_lock_poisoning(rel, text)
            if is_under(path, ROOT / "crates"):
                check_thread_spawns(rel, text)

    check_thread_inventory_docs(
        (ROOT / "crates/iris-net/src/concurrency.rs").read_text(encoding="utf-8"),
        (ROOT / "docs/ARCHITECTURE.md").read_text(encoding="utf-8"),
    )

    # 5. Bidirectional SVG icon registry & zero-reference check
    icons_on_disk = {p.name for p in (ROOT / "Resources/Icons").glob("*.svg")}
    assets_path = ROOT / "crates/iris/src/assets.rs"
    assets_text = assets_path.read_text(encoding="utf-8")
    icons_block = re.search(r"icons!\s*\{([^}]*)\}", assets_text)
    _require(icons_block is not None, "assets.rs missing `icons! { ... }` invocation")
    icon_entries = re.findall(r'([A-Za-z0-9_]+)\s*=>\s*"([A-Za-z0-9_.-]+)"', icons_block.group(1))
    icons_in_code = {f"{filename}.svg" for _, filename in icon_entries}
    _require(
        icons_on_disk == icons_in_code,
        f"SVG icon mismatch: disk-only={icons_on_disk - icons_in_code}, code-only={icons_in_code - icons_on_disk}",
    )
    iris_non_assets_code = "\n".join(
        strip_comments(p.read_text(encoding="utf-8"))
        for p in sorted(iris_src.rglob("*.rs"))
        if p != assets_path
    )
    unused_icons = sorted(
        f"Icon::{variant} ({filename}.svg)"
        for variant, filename in icon_entries
        if not re.search(rf"\bIcon::{variant}\b", iris_non_assets_code)
    )
    _require(not unused_icons, f"Unreferenced SVG icons in crates/iris/src: {', '.join(unused_icons)}")

    # 6. Example plugin manifest schema & entry verification
    for manifest_path in sorted((ROOT / "examples/plugins").glob("*/plugin.json")):
        plugin_data = json.loads(manifest_path.read_text(encoding="utf-8"))
        pid = plugin_data.get("id", "")
        _require(bool(pid) and not pid.startswith("iris."), f"{manifest_path}: invalid or reserved id `{pid}`")
        _require(bool(plugin_data.get("name", "").strip()), f"{manifest_path}: missing plugin name")
        _require(plugin_data.get("protocol") == plugin_data.get("abi") == 2, f"{manifest_path}: protocol/abi must be 2")
        entry = plugin_data.get("entry")
        if entry is not None:
            _require(entry == "plugin.wasm", f"{manifest_path}: only Wasm entries are accepted")
            _require(
                (manifest_path.parent / "src/lib.rs").is_file() and (manifest_path.parent / "Cargo.toml").is_file(),
                f"{manifest_path}: missing SDK source",
            )
        else:
            _require(bool(plugin_data.get("theme") or plugin_data.get("chrome")), f"{manifest_path}: missing declarative theme")
    _require(
        "pub const PROTOCOL_VERSION: u32 = 2;" in (ROOT / "crates/iris-api/src/lib.rs").read_text(encoding="utf-8"),
        "iris-api PROTOCOL_VERSION must be 2",
    )
    _require(not (ROOT / "crates/iris-core/src/plugin").exists(), "Legacy plugin runtime must not return")
    _require(not list((ROOT / "examples/plugins").rglob("*.py")), "Python plugin examples must not return")
    for path in (ROOT / "crates").rglob("*.rs"):
        code = strip_comments(path.read_text(encoding="utf-8"))
        _require("BuiltinPlugin" not in code, f"{path}: native builtin plugin bypass")
        _require(not re.search(r"Module::deserialize|\.deserialize_file\(", code), f"{path}: forbidden machine-code cache")
    subprocess.run([sys.executable, "-B", str(ROOT / "scripts/build-official-plugins.py"), "--check", "--output", str(ROOT / "Resources/Plugins")], cwd=ROOT, check=True, stdout=subprocess.PIPE)

    # 7. Rust toolchain version sync across CI workflows & standalone fixtures
    tc_text = (ROOT / "rust-toolchain.toml").read_text(encoding="utf-8")
    tc_match = re.search(r'(?m)^channel\s*=\s*"([^"]+)"', tc_text)
    _require(tc_match is not None, "rust-toolchain.toml missing channel")
    expected_channel = tc_match.group(1)
    allowed_msrv = (expected_channel, expected_channel.removesuffix(".0"))
    _require(
        all(p["rust_version"] in allowed_msrv for p in pkgs.values()),
        "Workspace package rust-version must match the pinned toolchain",
    )
    fixture_manifest = ROOT / "crates/iris-plugin-sdk/tests/fixtures/rust/Cargo.toml"
    if fixture_manifest.is_file():
        with fixture_manifest.open("rb") as fh:
            fixture_pkg = tomllib.load(fh).get("package", {})
        _require(
            fixture_pkg.get("rust-version") in allowed_msrv,
            f"{fixture_manifest.relative_to(ROOT)} rust-version must match {allowed_msrv}",
        )
    for wf in sorted((ROOT / ".github/workflows").glob("*.yml")):
        wf_text = wf.read_text(encoding="utf-8")
        for match in re.findall(r"rustup toolchain install ([0-9.]+)", wf_text):
            _require(
                match == expected_channel,
                f"{wf.relative_to(ROOT)} installs Rust {match}, expected {expected_channel}",
            )

    if soft_warnings:
        print("Note (files > 650 prod lines):", ", ".join(soft_warnings))
    print(
        "All checks passed: Python syntax, recursive UI/mpv boundary, devtools gate, "
        "layer dependencies, file size budgets (<850 prod / <1000 total), lock poisoning, SVG icons, "
        "plugin manifests, and toolchain sync: OK"
    )


if __name__ == "__main__":
    main()
