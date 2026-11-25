#!/usr/bin/env python3
"""Repeated actual-host JIT smoke measurements; not a playback/UI benchmark."""
import argparse
import ctypes
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile
import time


def windows_sample(process):
    from ctypes import wintypes

    class Counters(ctypes.Structure):
        _fields_ = [("cb", wintypes.DWORD), ("faults", wintypes.DWORD),
                    *[(name, ctypes.c_size_t) for name in ("peak_ws", "ws", "peak_paged", "paged", "peak_nonpaged", "nonpaged", "pagefile", "peak_pagefile")]]

    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]
    counters = Counters()
    counters.cb = ctypes.sizeof(counters)
    handle = wintypes.HANDLE(int(process._handle))
    if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
        return None, None
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.GetProcessTimes.argtypes = [wintypes.HANDLE, *([ctypes.POINTER(wintypes.FILETIME)] * 4)]
    fields = [wintypes.FILETIME() for _ in range(4)]
    if not kernel.GetProcessTimes(handle, *(ctypes.byref(v) for v in fields)):
        return counters.peak_ws, None
    cpu = sum((v.dwHighDateTime << 32) | v.dwLowDateTime for v in fields[2:]) / 10_000_000
    return counters.peak_ws, cpu


def measure(executable):
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        start = time.monotonic()
        process = subprocess.Popen([str(executable), "--check-plugin-runtime"], stdout=stdout, stderr=stderr)
        rss, cpu = 0, None
        try:
            while process.poll() is None:
                if time.monotonic() - start > 180:
                    raise TimeoutError("plugin runtime benchmark exceeded 180 seconds")
                if os.name == "nt":
                    sample, cpu_sample = windows_sample(process)
                    rss = max(rss, sample or 0)
                    cpu = cpu_sample if cpu_sample is not None else cpu
                else:
                    result = subprocess.run(["ps", "-o", "rss=", "-p", str(process.pid)], capture_output=True, text=True)
                    if result.stdout.strip().isdigit():
                        rss = max(rss, int(result.stdout) * 1024)
                time.sleep(0.02)
        finally:
            if process.poll() is None:
                process.kill()
            process.wait()
        if process.returncode:
            stderr.seek(0)
            raise RuntimeError(stderr.read(8192).decode("utf-8", errors="replace"))
        stdout.seek(0)
        result = json.load(stdout)
        if result.get("status") != "ok" or result.get("protocol") != 2 or result.get("abi") != 2:
            raise RuntimeError("invalid runtime receipt")
        return {"wall_ms": (time.monotonic() - start) * 1000, "peak_rss_bytes": rss,
                "cpu_seconds_sampled": cpu, "receipt": result}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline", type=Path, help="compare the same machine/profile against a recorded 25%% regression budget")
    args = parser.parse_args()
    if not 2 <= args.runs <= 30:
        parser.error("--runs must be between 2 and 30")
    executable = args.executable.resolve(strict=True)
    rows = []
    for index in range(args.runs):
        rows.append(measure(executable))
        print(f"Completed JIT sample {index + 1}/{args.runs}", flush=True)
    def summary(values):
        return {"p50": statistics.median(values), "p95_nearest_rank": sorted(values)[math.ceil(len(values) * .95) - 1]}
    plugin_summary = {}
    for identity in ("iris.guides", "iris.webdav"):
        plugin_rows = [next(p for p in row["receipt"]["plugins"] if p["plugin"] == identity) for row in rows]
        plugin_summary[identity] = {key: summary([p[key] for p in plugin_rows]) for key in ("compile_ms", "cache_hit_ms", "instantiate_ms", "callback_ms")}
    result = {"scope": "fresh host processes; cold in-process module cache, then one cache hit; filesystem cache not flushed",
              "limitations": "RSS sampled every 20ms (Windows OS peak working set); CPU sample may omit final interval. No playback, GPU, UI frame time or allocation counts.",
              "platform": platform.platform(), "machine": platform.machine(), "cpu": platform.processor(), "logical_cpus": os.cpu_count(),
              "executable_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
              "wall_ms": summary([r["wall_ms"] for r in rows]), "peak_rss_bytes": summary([r["peak_rss_bytes"] for r in rows]),
              "plugins": plugin_summary, "samples": rows}
    root = Path(__file__).resolve().parents[1]
    result["source_commit"] = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    result["source_dirty"] = bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=root))
    result["rustc"] = subprocess.check_output(["rustc", "--version"], text=True).strip()
    result["regression_budget"] = {"scope": "same machine, OS and build profile; 5-run pilot; p95 is nearest rank, not a population estimate",
        "wall_p95_ms": result["wall_ms"]["p95_nearest_rank"] * 1.25,
        "rss_p95_bytes": math.ceil(result["peak_rss_bytes"]["p95_nearest_rank"] * 1.25)}
    failures = []
    if args.baseline:
        baseline = json.loads(args.baseline.read_bytes())
        for key in ("platform", "machine", "cpu", "logical_cpus", "rustc"):
            if baseline.get(key) != result[key]:
                failures.append("incomparable host/toolchain: " + key)
        if baseline["samples"][0]["receipt"]["debug"] != rows[0]["receipt"]["debug"]:
            failures.append("incomparable build profile")
        for field, metric in (("wall_p95_ms", "wall_ms"), ("rss_p95_bytes", "peak_rss_bytes")):
            if result[metric]["p95_nearest_rank"] > baseline["regression_budget"][field]:
                failures.append("regression: " + field)
        result["baseline_failures"] = failures
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    if failures:
        raise SystemExit("; ".join(failures))


if __name__ == "__main__":
    main()
