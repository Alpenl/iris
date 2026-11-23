#!/usr/bin/env python3
"""Fast, zero-network architectural & engineering invariant gates for Iris."""
import ast
import json
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

PROD_WARN_LINES = 650
PROD_HARD_MAX_LINES = 850
SRC_TOTAL_HARD_MAX_LINES = 1000
TEST_FILE_HARD_MAX_LINES = 1000


def strip_line_comments(rust_src: str) -> str:
    out = []
    for line in rust_src.splitlines():
        stripped = line.lstrip()
        if stripped.startswith("//"):
            out.append("")
        else:
            out.append(line.split("//", 1)[0])
    return "\n".join(out)


def is_under(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


# 1. Python syntax across scripts/ and examples/
for directory in ("scripts", "examples"):
    base = ROOT / directory
    if base.is_dir():
        for script in sorted(base.rglob("*.py")):
            ast.parse(script.read_text(encoding="utf-8"), filename=str(script))

# 2. Recursive UI & app layer isolation (no raw mpv outside src/video/, no wildcard super, no UI target_os)
iris_src = ROOT / "crates/iris/src"
video_dir = iris_src / "video"
ui_dir = iris_src / "ui"

mpv_needles = (
    "iris_mpv",
    "command_async(",
    "loadfile_with_options(",
    "get_property_string(",
    "set_property",
    "self.mpv",
    "this.mpv",
)

for path in sorted(iris_src.rglob("*.rs")):
    rel = path.relative_to(ROOT)
    raw = path.read_text(encoding="utf-8")
    code = strip_line_comments(raw)
    if not is_under(path, video_dir):
        for needle in mpv_needles:
            assert needle not in code, f"{rel} breaks playback boundary via `{needle}`"
    if is_under(path, ui_dir):
        assert "use super::*;" not in code, f"{rel} uses forbidden wildcard `use super::*;`"
        assert "cfg!(target_os" not in code, f"{rel} scatters `cfg!(target_os...)` in UI layer"

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

for crate_name in ("iris", "iris-core"):
    default_feats = pkgs[crate_name]["features"].get("default", [])
    assert "devtools" not in default_feats, f"{crate_name} enables devtools by default: {default_feats}"

for gated_file in (
    ROOT / "crates/iris/src/debug.rs",
    ROOT / "crates/iris-core/src/player/mod.rs",
):
    assert 'feature = "devtools"' in gated_file.read_text(encoding="utf-8"), (
        f"{gated_file.relative_to(ROOT)} missing devtools cfg gate"
    )

for name, pkg in pkgs.items():
    manifest_text = Path(pkg["manifest_path"]).read_text(encoding="utf-8")
    assert re.search(r"(?m)^\[lints\]\s+workspace\s*=\s*true\b", manifest_text), (
        f"{name} must inherit `[lints] workspace = true`"
    )
    all_deps = {d["name"] for d in pkg["dependencies"]}
    assert "bindgen" not in all_deps, f"{name} must not depend on bindgen"
    assert "tokio" not in all_deps, f"{name} must not depend directly on tokio"

mpv_normal = [d["name"] for d in pkgs["iris-mpv"]["dependencies"] if d["kind"] is None]
assert mpv_normal == [], f"iris-mpv must stay zero-dependency, got {mpv_normal}"

api_normal = [d["name"] for d in pkgs["iris-api"]["dependencies"] if d["kind"] is None]
assert api_normal == ["serde"], f"iris-api normal deps must be ['serde'] only, got {api_normal}"

forbidden_core = {"gpui", "cocoa", "objc", "objc2", "raw-window-handle", "winit", "image"}
core_deps = {d["name"] for d in pkgs["iris-core"]["dependencies"]}
assert not (core_deps & forbidden_core), f"iris-core must stay headless, found {core_deps & forbidden_core}"

# 4. File line-count budgets (prevent monolith regressions)
soft_warnings = []
for path in sorted((ROOT / "crates").rglob("*.rs")):
    rel = path.relative_to(ROOT)
    text = path.read_text(encoding="utf-8")
    total_lines = len(text.splitlines())
    if "/tests/" in rel.as_posix():
        assert total_lines <= TEST_FILE_HARD_MAX_LINES, (
            f"{rel} has {total_lines} lines (exceeds test file limit {TEST_FILE_HARD_MAX_LINES})"
        )
        continue
    prod_part = re.split(r"(?m)^#\[cfg\(test\)\]", text, maxsplit=1)[0]
    prod_lines = len(prod_part.splitlines())
    assert prod_lines <= PROD_HARD_MAX_LINES, (
        f"{rel} has {prod_lines} production lines (hard limit {PROD_HARD_MAX_LINES})"
    )
    assert total_lines <= SRC_TOTAL_HARD_MAX_LINES, (
        f"{rel} has {total_lines} total lines (hard limit {SRC_TOTAL_HARD_MAX_LINES})"
    )
    if prod_lines > PROD_WARN_LINES:
        soft_warnings.append(f"{rel} ({prod_lines} prod lines)")

# 5. Bidirectional SVG icon registry check
icons_on_disk = {p.name for p in (ROOT / "Resources/Icons").glob("*.svg")}
assets_text = (ROOT / "crates/iris/src/assets.rs").read_text(encoding="utf-8")
icons_block = re.search(r"icons!\s*\{([^}]*)\}", assets_text)
assert icons_block, "assets.rs missing `icons! { ... }` invocation"
icons_in_code = {f"{m}.svg" for m in re.findall(r'=>\s*"([A-Za-z0-9_.-]+)"', icons_block.group(1))}
assert icons_on_disk == icons_in_code, (
    f"SVG icon mismatch: disk-only={icons_on_disk - icons_in_code}, code-only={icons_in_code - icons_on_disk}"
)

# 6. Example plugin manifest schema & entry verification
for manifest_path in sorted((ROOT / "examples/plugins").glob("*/plugin.json")):
    plugin_data = json.loads(manifest_path.read_text(encoding="utf-8"))
    pid = plugin_data.get("id", "")
    assert pid and not pid.startswith("iris."), f"{manifest_path}: invalid or reserved id `{pid}`"
    assert plugin_data.get("name", "").strip(), f"{manifest_path}: missing plugin name"
    assert plugin_data.get("protocol") == 1, f"{manifest_path}: protocol must be 1"
    entry = plugin_data.get("entry")
    if entry is not None:
        assert (manifest_path.parent / entry).is_file(), f"{manifest_path}: missing entry `{entry}`"

# 7. Rust toolchain version sync across CI workflows
tc_text = (ROOT / "rust-toolchain.toml").read_text(encoding="utf-8")
tc_match = re.search(r'(?m)^channel\s*=\s*"([^"]+)"', tc_text)
assert tc_match, "rust-toolchain.toml missing channel"
expected_channel = tc_match.group(1)
for wf in sorted((ROOT / ".github/workflows").glob("*.yml")):
    wf_text = wf.read_text(encoding="utf-8")
    for match in re.findall(r"rustup toolchain install ([0-9.]+)", wf_text):
        assert match == expected_channel, (
            f"{wf.relative_to(ROOT)} installs Rust {match}, expected {expected_channel}"
        )

if soft_warnings:
    print("Note (files > 650 prod lines):", ", ".join(soft_warnings))
print(
    "All checks passed: Python syntax, recursive UI/mpv boundary, devtools gate, "
    "layer dependencies, file size budgets (<850 prod / <1000 total), SVG icons, "
    "plugin manifests, and toolchain sync: OK"
)
