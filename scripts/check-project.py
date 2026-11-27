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
INLINE_TEST_HARD_MAX_LINES = 400
TEST_FILE_HARD_MAX_LINES = 1500


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

# One-way ratchet for top-level module dependency cycles in non-guest crates under `crates/*/src`.
# Bundled Wasm guest plugin packages (`plugin.json` present) are frozen archives outside host
# refactoring scope. Adding any new cycle edge fails; resolving an existing edge requires removing
# it here so the baseline only shrinks (R-05 clears `iris-core`; R-13 clears `iris-plugin-host`).
KNOWN_MODULE_CYCLE_EDGES: dict[str, frozenset[tuple[str, str]]] = {
    "iris-core": frozenset(),
    "iris-plugin-host": frozenset(
        {
            ("actor", "engine"),
            ("actor", "instance"),
            ("actor", "validation"),
            ("auth", "actor"),
            ("auth", "instance"),
            ("auth", "network"),
            ("engine", "imports"),
            ("engine", "instance"),
            ("engine", "package"),
            ("enhancement", "actor"),
            ("enhancement", "files"),
            ("enhancement", "instance"),
            ("enhancement", "runtime"),
            ("enhancement", "stream"),
            ("enhancement", "validation"),
            ("files", "actor"),
            ("files", "installation"),
            ("files", "instance"),
            ("files", "network"),
            ("imports", "instance"),
            ("installation", "engine"),
            ("installation", "instance"),
            ("installation", "package"),
            ("installation", "runtime"),
            ("instance", "actor"),
            ("instance", "engine"),
            ("network", "actor"),
            ("network", "installation"),
            ("network", "instance"),
            ("package", "validation"),
            ("runtime", "actor"),
            ("runtime", "auth"),
            ("runtime", "engine"),
            ("runtime", "enhancement"),
            ("runtime", "files"),
            ("runtime", "installation"),
            ("runtime", "instance"),
            ("runtime", "network"),
            ("runtime", "services"),
            ("runtime", "storage"),
            ("runtime", "stream"),
            ("services", "actor"),
            ("services", "auth"),
            ("services", "files"),
            ("services", "instance"),
            ("services", "network"),
            ("services", "storage"),
            ("storage", "actor"),
            ("storage", "installation"),
            ("storage", "instance"),
            ("storage", "network"),
            ("stream", "actor"),
            ("stream", "engine"),
            ("stream", "instance"),
            ("stream", "network"),
            ("validation", "network"),
        }
    ),
}

# Exact file/directory exemptions for `allow/expect(clippy::disallowed_*)` across `crates/*/{src,tests}`.
DISALLOWED_LINT_EXEMPT_PREFIXES = ("crates/iris/src/video/",)
# Temporary R-08 cross-layer integration test exemption for `disallowed_types` (`Session`) only,
# until R-09 (#35) migrates `player_scoped_plugin` into `crates/iris-app/tests/`.
HOST_SCOPED_TEST_DISALLOWED_TYPES_EXEMPT_FILES = frozenset(
    {"crates/iris-plugin-host/tests/player_scoped_plugin.rs"}
)
HOST_SCOPED_TEST_DISALLOWED_TYPES_EXEMPT_PREFIXES = (
    "crates/iris-plugin-host/tests/player_scoped_plugin/",
)
HOST_SCOPED_TEST_TYPES_ONLY_ALLOW_RE = re.compile(
    r"#\s*!?\s*\[\s*(?:allow|expect)\s*\(\s*clippy::disallowed_types\s*\)\s*\]"
)
IRIS_FORBIDDEN_HOST_INTERNAL_RE = re.compile(
    r"\biris_plugin_host::(?:ledger|actor)\b|\bstream::prepare\b"
)
DISALLOWED_LINT_SUPPRESSION_RE = re.compile(
    r"#\s*!?\s*\[\s*(?:allow|expect)\s*\([^]]*\b(?:clippy::)?disallowed_[A-Za-z0-9_]+[^]]*\)\s*\]"
    r"|\b(?:allow|expect)\s*\([^)]*\b(?:clippy::)?disallowed_[A-Za-z0-9_]+"
)
CORE_PLAYER_EXEMPT_ALLOW_RE = re.compile(
    r"#\s*\[\s*(?:allow|expect)\s*\(\s*clippy::disallowed_methods\s*\)\s*\]\s*"
    r"pub\s+fn\s+(render_handle|video_recovery)\s*\("
)


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


def strip_comments_and_literals(rust_src: str) -> str:
    """Strip comments and blank out string/char literals while preserving newlines and Rust punctuation."""
    out = []
    i = 0
    n = len(rust_src)
    while i < n:
        ch = rust_src[i]
        nxt = rust_src[i + 1] if i + 1 < n else ""
        if ch == "/" and nxt == "/":
            i += 2
            while i < n and rust_src[i] != "\n":
                i += 1
            continue
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
                        out.append('""')
                        break
                    newlines = rust_src[i : end + len(end_marker)].count("\n")
                    out.append('""' + ("\n" * newlines))
                    i = end + len(end_marker)
                    continue
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
            newlines = rust_src[start:i].count("\n")
            out.append('""' + ("\n" * newlines))
            continue
        if ch == "'":
            if i + 3 < n and rust_src[i + 1] == "\\":
                end = rust_src.find("'", i + 3)
                if end != -1 and "\n" not in rust_src[i:end]:
                    out.append("'x'")
                    i = end + 1
                    continue
            elif i + 2 < n and rust_src[i + 1] not in ("'", "\n") and rust_src[i + 2] == "'":
                out.append("'x'")
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


def count_sloc(stripped_src: str) -> int:
    """Count non-blank source lines of code after comments have been stripped."""
    return sum(1 for line in stripped_src.splitlines() if line.strip())


def prod_prefix(text: str) -> str:
    stripped = strip_comments(text)
    test_mod = TEST_MOD_RE.search(stripped)
    return stripped[: test_mod.start()] if test_mod else stripped


CFG_TEST_ATTR_RE = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
OUTER_ATTR_RE = re.compile(r"#\s*\[[^\]]*\]")
SEMI_TERMINATED_ITEM_RE = re.compile(
    r"(?:pub(?:\s*\([^)]*\))?\s+)?(?:use|type|const|static|extern\s+crate)\b"
)


def strip_cfg_test_items(sanitized_src: str) -> str:
    """Remove all `#[cfg(test)]`-annotated items/blocks from comment-and-literal-stripped Rust code."""
    src = sanitized_src
    while True:
        match = CFG_TEST_ATTR_RE.search(src)
        if match is None:
            return src
        start = match.start()
        pos = match.end()
        n = len(src)
        while pos < n:
            while pos < n and src[pos].isspace():
                pos += 1
            attr_match = OUTER_ATTR_RE.match(src, pos)
            if attr_match is None:
                break
            pos = attr_match.end()
        semi_only = SEMI_TERMINATED_ITEM_RE.match(src, pos) is not None
        depth = 0
        end = n
        i = pos
        while i < n:
            ch = src[i]
            if ch == "{":
                if not semi_only and depth == 0:
                    close = find_matching_brace(src, i)
                    end = close + 1
                    break
                depth += 1
            elif ch == "}":
                depth = max(0, depth - 1)
            elif ch == ";" and depth == 0:
                end = i + 1
                break
            i += 1
        src = src[:start] + src[end:]


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


TOP_MOD_DECL_RE = re.compile(r"(?m)^\s*(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?:;|\{)")
USE_STMT_RE = re.compile(r"(?<![A-Za-z0-9_])(?:pub(?:\s*\([^)]*\))?\s+)?use\s+(?!<)([^;]+);")
QUALIFIED_REL_PATH_RE = re.compile(r"(?<![A-Za-z0-9_:])((?:crate|super|self)(?:::[A-Za-z_][A-Za-z0-9_]*)+)")


def expand_use_tree(tree: str) -> list[str]:
    """Expand a Rust `use` tree (including nested braces and `as` aliases) into flat `::` paths."""

    def parse_group(text: str, idx: int) -> tuple[list[str], int]:
        items: list[str] = []
        n = len(text)
        while idx < n:
            while idx < n and text[idx].isspace():
                idx += 1
            if idx >= n or text[idx] == "}":
                break
            if text[idx] == ",":
                idx += 1
                continue
            buf: list[str] = []
            while idx < n and text[idx] not in "{,}":
                buf.append(text[idx])
                idx += 1
            raw_prefix = "".join(buf).strip()
            if idx < n and text[idx] == "{":
                children, idx = parse_group(text, idx + 1)
                if idx < n and text[idx] == "}":
                    idx += 1
                compact_base = re.sub(r"\s+", "", raw_prefix).removesuffix("::")
                for child in children:
                    if child == "self":
                        if compact_base:
                            items.append(compact_base)
                    else:
                        items.append(f"{compact_base}::{child}" if compact_base else child)
            elif raw_prefix:
                before_alias = re.split(r"\s+as\s+", raw_prefix, maxsplit=1)[0].strip()
                compact = re.sub(r"\s+", "", before_alias)
                if compact:
                    items.append(compact)
        return items, idx

    expanded, _ = parse_group(tree, 0)
    return expanded


def _resolve_target_top_module(
    path_expr: str,
    mod_chain: list[str],
    top_mods: set[str],
) -> str | None:
    if path_expr.startswith("::"):
        return None
    segs = [seg for seg in path_expr.split("::") if seg]
    if not segs:
        return None
    if segs[0] == "crate":
        cur: list[str] = []
        rest = segs[1:]
    elif segs[0] in ("super", "self"):
        cur = list(mod_chain)
        rest = segs
    else:
        return None
    while rest and rest[0] in ("super", "self"):
        head = rest.pop(0)
        if head == "super":
            if not cur:
                return None
            cur.pop()
    resolved = cur + rest
    if resolved and resolved[0] in top_mods:
        return resolved[0]
    return None


def extract_crate_module_edges(src_dir: Path) -> tuple[set[str], set[tuple[str, str]]]:
    """Extract top-level modules and directed inter-module edges for a crate's `src/` directory."""
    top_mods: set[str] = set()
    for root_file in ("lib.rs", "main.rs"):
        rp = src_dir / root_file
        if rp.is_file():
            root_prod = strip_cfg_test_items(strip_comments_and_literals(rp.read_text(encoding="utf-8")))
            for match in TOP_MOD_DECL_RE.finditer(root_prod):
                top_mods.add(match.group(1))

    edges: set[tuple[str, str]] = set()
    if not top_mods:
        return top_mods, edges

    for rs in sorted(src_dir.rglob("*.rs")):
        rel_src = rs.relative_to(src_dir)
        if is_standalone_test_file(rs, Path("src") / rel_src):
            continue
        parts = rel_src.parts
        if len(parts) == 1 and parts[0] in ("lib.rs", "main.rs"):
            continue
        src_mod = parts[0].removesuffix(".rs")
        if src_mod not in top_mods:
            continue
        mod_chain = list(parts[:-1])
        if parts[-1] != "mod.rs":
            mod_chain.append(parts[-1].removesuffix(".rs"))

        prod_code = strip_cfg_test_items(strip_comments_and_literals(rs.read_text(encoding="utf-8")))
        for match in USE_STMT_RE.finditer(prod_code):
            for path_expr in expand_use_tree(match.group(1)):
                target = _resolve_target_top_module(path_expr, mod_chain, top_mods)
                if target is not None and target != src_mod:
                    edges.add((src_mod, target))
        for match in QUALIFIED_REL_PATH_RE.finditer(prod_code):
            target = _resolve_target_top_module(match.group(1), mod_chain, top_mods)
            if target is not None and target != src_mod:
                edges.add((src_mod, target))
    return top_mods, edges


def tarjan_sccs(nodes: set[str], edges: set[tuple[str, str]]) -> list[list[str]]:
    """Return non-trivial strongly connected components (each with >=2 modules) in deterministic order."""
    adj = {node: sorted(dst for src, dst in edges if src == node) for node in nodes}
    next_index = 0
    indices: dict[str, int] = {}
    lowlink: dict[str, int] = {}
    stack: list[str] = []
    on_stack: set[str] = set()
    sccs: list[list[str]] = []

    def strongconnect(v: str):
        nonlocal next_index
        indices[v] = next_index
        lowlink[v] = next_index
        next_index += 1
        stack.append(v)
        on_stack.add(v)
        for w in adj.get(v, ()):
            if w not in indices:
                strongconnect(w)
                lowlink[v] = min(lowlink[v], lowlink[w])
            elif w in on_stack:
                lowlink[v] = min(lowlink[v], indices[w])
        if lowlink[v] == indices[v]:
            comp: list[str] = []
            while True:
                w = stack.pop()
                on_stack.remove(w)
                comp.append(w)
                if w == v:
                    break
            if len(comp) > 1:
                sccs.append(sorted(comp))

    for node in sorted(nodes):
        if node not in indices:
            strongconnect(node)
    return sorted(sccs)


def _format_edges(edge_set) -> str:
    ordered = sorted(edge_set)
    return ", ".join(f"{u} -> {v}" for u, v in ordered) if ordered else "none"


def check_module_cycle_ratchet(
    crates_dir: Path,
    baseline: dict[str, frozenset[tuple[str, str]]] = KNOWN_MODULE_CYCLE_EDGES,
):
    """Enforce one-way ratchet on top-level module dependency cycles across `crates/*/src`."""
    seen_crates: set[str] = set()
    for crate_dir in sorted(p for p in crates_dir.iterdir() if p.is_dir()):
        if (crate_dir / "plugin.json").is_file():
            # Bundled official Wasm guest plugin crates are frozen byte-reproducible packages.
            continue
        src_dir = crate_dir / "src"
        if not src_dir.is_dir():
            continue
        crate_name = crate_dir.name
        seen_crates.add(crate_name)
        nodes, edges = extract_crate_module_edges(src_dir)
        sccs = tarjan_sccs(nodes, edges)
        cycle_edges = frozenset(
            (u, v)
            for scc in sccs
            for scc_set in (set(scc),)
            for (u, v) in edges
            if u in scc_set and v in scc_set
        )
        expected = baseline.get(crate_name, frozenset())
        if cycle_edges != expected:
            added = cycle_edges - expected
            resolved = expected - cycle_edges
            scc_desc = "; ".join("[" + ", ".join(scc) + "]" for scc in sccs) if sccs else "none"
            lines = [
                f"Module cycle ratchet mismatch in `{crate_name}`:",
                f"  SCCs ({len(sccs)}): {scc_desc}",
                f"  Current cycle edges ({len(cycle_edges)}): {_format_edges(cycle_edges)}",
            ]
            if added:
                lines.append(f"  New cycle edges (forbidden): {_format_edges(added)}")
            if resolved:
                lines.append(
                    f"  Resolved cycle edges (remove from KNOWN_MODULE_CYCLE_EDGES): {_format_edges(resolved)}"
                )
            raise AssertionError("\n".join(lines))

    stale_crates = sorted(name for name, edges in baseline.items() if edges and name not in seen_crates)
    _require(
        not stale_crates,
        f"KNOWN_MODULE_CYCLE_EDGES references missing crates: {', '.join(stale_crates)}",
    )


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
    """Production code must spawn OS threads only via `iris_base::rt::{spawn_thread, spawn_thread_named}`."""
    if rel.as_posix() == "crates/iris-base/src/rt/mod.rs":
        return
    match = RAW_THREAD_SPAWN_RE.search(prod_prefix(text))
    _require(
        match is None,
        f"{rel} spawns an unregistered OS thread via `{match.group(0) if match else ''}`; "
        "use `iris_base::rt::{spawn_thread, spawn_thread_named}`",
    )


def check_thread_inventory_docs(rt_src: str, arch_doc: str):
    """Every `ThreadRole::base_name` in `iris-base::rt` must be documented in `docs/ARCHITECTURE.md`."""
    stripped = prod_prefix(rt_src)
    count_match = THREAD_ROLE_COUNT_RE.search(stripped)
    _require(count_match is not None, "rt/mod.rs missing `ThreadRole::COUNT`")
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


ARCH_CRATES_BLOCK_RE = re.compile(r"```[a-zA-Z]*\ncrates/\n(.*?)```", re.DOTALL)
ARCH_CRATE_ENTRY_RE = re.compile(r"(?m)^\s+(?:[├└│─\s]*)?([a-z0-9-]+)\/")


def _markdown_h2_section(doc: str, heading: str) -> str:
    pattern = re.compile(rf"(?m)^##\s+{re.escape(heading)}\s*$(.*?)(?=^##\s+|\Z)", re.DOTALL)
    match = pattern.search(doc)
    _require(match is not None, f"Missing `## {heading}` section")
    return match.group(1)


def check_crate_docs_sync(
    expected_crates: set[str],
    arch_doc: str,
    readme_en: str,
    readme_zh: str,
):
    """Strict two-way crate check on `docs/ARCHITECTURE.md` + lightweight check on bilingual READMEs."""
    block_match = ARCH_CRATES_BLOCK_RE.search(arch_doc)
    _require(block_match is not None, "docs/ARCHITECTURE.md missing `crates/` layer block")
    arch_entries = ARCH_CRATE_ENTRY_RE.findall(block_match.group(1))
    _require(
        len(arch_entries) == len(set(arch_entries)),
        f"docs/ARCHITECTURE.md has duplicate crate entries: {arch_entries}",
    )
    arch_crates = set(arch_entries)
    _require(
        arch_crates == expected_crates,
        "docs/ARCHITECTURE.md `crates/` list out of sync with workspace crates: "
        f"missing={sorted(expected_crates - arch_crates)}, extra={sorted(arch_crates - expected_crates)}",
    )

    for doc_name, doc_text, heading in (
        ("README.md", readme_en, "Project layout"),
        ("README.zh-CN.md", readme_zh, "代码结构"),
    ):
        section = _markdown_h2_section(doc_text, heading)
        missing = sorted(
            crate
            for crate in expected_crates
            if not re.search(rf"(?<![A-Za-z0-9_-]){re.escape(crate)}(?![A-Za-z0-9_-])", section)
        )
        _require(
            not missing,
            f"{doc_name} (`## {heading}`) missing crate names: {', '.join(missing)}",
        )


def check_clippy_disallowed_configs(root: Path):
    """Validate workspace and per-crate `clippy.toml` disallowed rules."""
    root_cargo = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    clippy_lints = root_cargo.get("workspace", {}).get("lints", {}).get("clippy", {})
    for lint in ("disallowed_methods", "disallowed_types"):
        _require(
            clippy_lints.get(lint) == "deny",
            f"Workspace Cargo.toml must deny `clippy::{lint}`",
        )

    def configured_section_paths(rel_toml: str, key: str) -> set[str]:
        data = tomllib.loads((root / rel_toml).read_text(encoding="utf-8"))
        paths: set[str] = set()
        for entry in data.get(key, []):
            if isinstance(entry, str):
                paths.add(entry)
            elif isinstance(entry, dict) and isinstance(entry.get("path"), str):
                paths.add(entry["path"])
        return paths

    def configured_paths(rel_toml: str) -> set[str]:
        return configured_section_paths(rel_toml, "disallowed-types") | configured_section_paths(
            rel_toml, "disallowed-methods"
        )

    iris_paths = configured_paths("crates/iris/clippy.toml")
    for required_rule in (
        "iris_mpv::Mpv",
        "iris_mpv::RenderHandle",
        "iris_mpv::RenderHandle::new",
        "iris_core::player::Session::render_handle",
    ):
        _require(required_rule in iris_paths, f"crates/iris/clippy.toml must disallow `{required_rule}`")

    core_paths = configured_paths("crates/iris-core/clippy.toml")
    _require(
        "iris_mpv::RenderHandle::new" in core_paths,
        "crates/iris-core/clippy.toml must disallow `iris_mpv::RenderHandle::new`",
    )

    host_types = configured_section_paths("crates/iris-plugin-host/clippy.toml", "disallowed-types")
    host_methods = configured_section_paths("crates/iris-plugin-host/clippy.toml", "disallowed-methods")
    _require(
        "iris_core::player::Session" in host_types,
        "crates/iris-plugin-host/clippy.toml must disallow type `iris_core::player::Session`",
    )
    _require(
        "iris_core::player::Session::render_handle" in host_methods,
        "crates/iris-plugin-host/clippy.toml must disallow `iris_core::player::Session::render_handle`",
    )


def check_disallowed_lint_suppressions(rel: Path, raw_text: str):
    """Reject `allow/expect(clippy::disallowed_*)` outside exact reviewed paths."""
    posix = rel.as_posix()
    if any(posix == prefix or posix.startswith(prefix) for prefix in DISALLOWED_LINT_EXEMPT_PREFIXES):
        return
    sanitized = strip_comments_and_literals(raw_text)
    if (
        posix in HOST_SCOPED_TEST_DISALLOWED_TYPES_EXEMPT_FILES
        or any(posix.startswith(p) for p in HOST_SCOPED_TEST_DISALLOWED_TYPES_EXEMPT_PREFIXES)
    ):
        sanitized = HOST_SCOPED_TEST_TYPES_ONLY_ALLOW_RE.sub("", sanitized)
    if posix == "crates/iris-core/src/player/mod.rs":
        seen_fns: list[str] = []

        def _strip_allowed(match: re.Match) -> str:
            fn_name = match.group(1)
            seen_fns.append(fn_name)
            return f"pub fn {fn_name}("

        sanitized = CORE_PLAYER_EXEMPT_ALLOW_RE.sub(_strip_allowed, sanitized)
        _require(
            len(seen_fns) == len(set(seen_fns)),
            f"{rel} has duplicate `clippy::disallowed_methods` exemptions: {seen_fns}",
        )
    offending = DISALLOWED_LINT_SUPPRESSION_RE.search(sanitized)
    _require(
        offending is None,
        f"{rel} suppresses `clippy::disallowed_*` without authorization: `{offending.group(0) if offending else ''}`",
    )


def check_iris_plugin_host_facade_imports(rel: Path, raw_text: str):
    """Reject direct references from `crates/iris/src` into `iris_plugin_host::{ledger, actor}` or `stream::prepare`."""
    sanitized = strip_comments_and_literals(raw_text)
    for match in USE_STMT_RE.finditer(sanitized):
        for expanded in expand_use_tree(match.group(1)):
            _require(
                not (
                    expanded.startswith("iris_plugin_host::ledger")
                    or expanded.startswith("iris_plugin_host::actor")
                    or expanded.startswith("iris_plugin_host::stream::prepare")
                    or "::stream::prepare" in expanded
                ),
                f"{rel} imports forbidden plugin-host internal `{expanded}`; route through `WindowSession` / `PluginHost`",
            )
    hit = IRIS_FORBIDDEN_HOST_INTERNAL_RE.search(sanitized)
    _require(
        hit is None,
        f"{rel} references forbidden plugin-host internal `{hit.group(0) if hit else ''}`; route through `WindowSession` / `PluginHost`",
    )


def check_file_line_budget(rel: Path, text: str) -> int | None:
    """Validate file SLOC budgets and trailing `#[cfg(test)] mod` placement; return prod SLOC."""
    stripped = strip_comments(text)
    if is_standalone_test_file(rel, rel):
        test_sloc = count_sloc(stripped)
        _require(
            test_sloc <= TEST_FILE_HARD_MAX_LINES,
            f"{rel} has {test_sloc} test SLOC (exceeds test file limit {TEST_FILE_HARD_MAX_LINES})",
        )
        return None
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
            inline_test_sloc = count_sloc(stripped[test_mod.start() : close_idx + 1])
            _require(
                inline_test_sloc <= INLINE_TEST_HARD_MAX_LINES,
                f"{rel} has {inline_test_sloc} inline test SLOC "
                f"(hard limit {INLINE_TEST_HARD_MAX_LINES}; move tests to `tests.rs` or `tests/`)",
            )
    prod_sloc = count_sloc(prod_part)
    _require(
        prod_sloc <= PROD_HARD_MAX_LINES,
        f"{rel} has {prod_sloc} production SLOC (hard limit {PROD_HARD_MAX_LINES})",
    )
    return prod_sloc


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

    # 2. Recursive UI & crate isolation (`clippy.toml` `disallowed_*`, no wildcard super, no UI target_os)
    iris_src = ROOT / "crates/iris/src"
    ui_dir = iris_src / "ui"
    kit_dir = ui_dir / "kit"

    check_clippy_disallowed_configs(ROOT)

    for crate_dir in sorted(p for p in (ROOT / "crates").iterdir() if p.is_dir()):
        for subdir_name in ("src", "tests"):
            subdir = crate_dir / subdir_name
            if not subdir.is_dir():
                continue
            for path in sorted(subdir.rglob("*.rs")):
                rel = path.relative_to(ROOT)
                check_disallowed_lint_suppressions(rel, path.read_text(encoding="utf-8"))

    for path in sorted(iris_src.rglob("*.rs")):
        rel = path.relative_to(ROOT)
        raw = path.read_text(encoding="utf-8")
        check_iris_plugin_host_facade_imports(rel, raw)
        code = strip_comments(raw)
        if not is_standalone_test_file(path, rel):
            prod_code = prod_prefix(raw)
            _require("use super::*;" not in prod_code, f"{rel} uses forbidden wildcard `use super::*;`")
        if is_under(path, ui_dir):
            _require(not re.search(r"\btarget_os\b", code), f"{rel} scatters `target_os` cfg in UI layer")
        if is_under(path, kit_dir):
            _require(
                not re.search(r"\b(?:IrisWindow|AppModel)\b", code),
                f"{rel} couples `ui/kit` to root window/model types",
            )

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
    base_deps = {d["name"] for d in pkgs["iris-base"]["dependencies"]}
    forbidden_base = forbidden_headless | {"iris-core", "iris-plugin-host", "iris-api", "iris-mpv"}
    _require(not (base_deps & forbidden_base), f"iris-base must stay domain-free, found {base_deps & forbidden_base}")

    core_deps = {d["name"] for d in pkgs["iris-core"]["dependencies"]}
    forbidden_core = forbidden_headless | {"iris-plugin-host", "iris-api", "wasmtime"}
    _require(not (core_deps & forbidden_core), f"iris-core must stay headless and plugin-free, found {core_deps & forbidden_core}")

    host_deps = {d["name"] for d in pkgs["iris-plugin-host"]["dependencies"]}
    forbidden_host = forbidden_headless | {"iris-mpv"}
    _require(not (host_deps & forbidden_host), f"iris-plugin-host must stay headless and mpv-free, found {host_deps & forbidden_host}")

    # 3b. Wildcard-super ban across all headless & guest crates + top-level module cycle ratchet
    for crate_dir in (
        ROOT / "crates/iris-base/src",
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

    check_module_cycle_ratchet(ROOT / "crates")

    # 4. File SLOC budgets, lock poisoning, registered OS thread spawns & crate/docs sync
    soft_warnings = []
    for path in sorted([*(ROOT / "crates").rglob("*.rs"), *(ROOT / "examples/plugins").rglob("*.rs")]):
        rel = path.relative_to(ROOT)
        text = path.read_text(encoding="utf-8")
        prod_sloc = check_file_line_budget(rel, text)
        if prod_sloc is not None and prod_sloc > PROD_WARN_LINES:
            soft_warnings.append(f"{rel} ({prod_sloc} prod SLOC)")
        if not is_standalone_test_file(path, rel):
            check_lock_poisoning(rel, text)
            if is_under(path, ROOT / "crates"):
                check_thread_spawns(rel, text)

    arch_doc = (ROOT / "docs/ARCHITECTURE.md").read_text(encoding="utf-8")
    check_thread_inventory_docs(
        (ROOT / "crates/iris-base/src/rt/mod.rs").read_text(encoding="utf-8"),
        arch_doc,
    )
    crates_on_disk = {
        p.name for p in (ROOT / "crates").iterdir() if p.is_dir() and (p / "Cargo.toml").is_file()
    }
    workspace_non_example_crates = {
        Path(p["manifest_path"]).parent.name
        for p in pkgs.values()
        if is_under(Path(p["manifest_path"]), ROOT / "crates")
    }
    _require(
        crates_on_disk == workspace_non_example_crates,
        f"Unregistered crates/ directories: {crates_on_disk ^ workspace_non_example_crates}",
    )
    check_crate_docs_sync(
        crates_on_disk,
        arch_doc,
        (ROOT / "README.md").read_text(encoding="utf-8"),
        (ROOT / "README.zh-CN.md").read_text(encoding="utf-8"),
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
    subprocess.run(
        [sys.executable, "-B", str(ROOT / "scripts/build-official-plugins.py"), "--check", "--output", str(ROOT / "Resources/Plugins")],
        cwd=ROOT,
        check=True,
        stdout=subprocess.PIPE,
    )

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
        print("Note (files > 650 prod SLOC):", ", ".join(soft_warnings))
    print(
        "All checks passed: Python syntax, recursive UI/mpv/clippy boundary, devtools gate, "
        "layer dependencies, module cycle ratchet, SLOC budgets (<850 prod / <400 inline test / <1500 test), "
        "lock poisoning, docs sync, SVG icons, plugin manifests, and toolchain sync: OK"
    )


if __name__ == "__main__":
    main()
