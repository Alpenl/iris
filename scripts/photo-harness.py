#!/usr/bin/env python3
"""Photo edge-case checks for Iris, driven over the IRIS_CONTROL local stream.

Starts its own devtools Iris instance with a private Unix socket (macOS) or
named pipe (Windows) and settings file; existing user windows are never touched.
Run scripts/make-photo-test-media.sh first; the media goes to test-media/photo.

usage: python3 scripts/photo-harness.py [--repo DIR] [--bin PATH] OUTDIR TEST...
tests:
  probe  open one JPEG, print state and a few mpv properties
  exif   JPEG orientation 1/3/6/8 and a HEIC with orientation 6
  heic   small HEIC and a 48 MP grid HEIC
  icc    untagged / sRGB / P3 / Adobe RGB swatches (analyse with icc-report.py)
  huge   images past the 16384 px texture limit, with RSS before/after
  flip   ~48 MP JPEG and HEIC flip latency (key right), plus a 10-press burst

Writes OUTDIR/results.json (one JSON line appended per run), snapshots
OUTDIR/<test>-<file>.png and Iris's stderr in OUTDIR/iris-stderr.log.
Latency is measured until mpv reports video-params for the new file; texture
upload and presentation are not included.
"""
import argparse, ctypes, json, os, queue, re, runpy, subprocess, sys, tempfile, threading, time

ControlClient = runpy.run_path(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                          "control-client.py"))["ControlClient"]

_ap = argparse.ArgumentParser(description="Photo edge-case checks for Iris.")
_ap.add_argument("--repo", default=os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                 help="repository root (default: the parent of scripts/)")
_ap.add_argument("--bin", help="Iris debug/devtools binary (default: REPO/target/release/iris[.exe])")
_ap.add_argument("outdir")
_ap.add_argument("tests", nargs="+", choices=["probe", "exif", "heic", "icc", "huge", "flip"])
_args = _ap.parse_args()

REPO, OUT = _args.repo, os.path.abspath(_args.outdir)
TESTS = _args.tests
BIN = os.path.abspath(_args.bin or os.path.join(REPO, "target", "release",
                                              "iris.exe" if os.name == "nt" else "iris"))
MEDIA = os.path.abspath(os.path.join(REPO, "test-media/photo"))
if not os.path.isfile(BIN):
    sys.exit(f"no Iris binary at {BIN}; run `cargo build --release --features devtools` first")
if not os.path.isdir(MEDIA):
    sys.exit(f"no test media at {MEDIA}; run scripts/make-photo-test-media.sh first")
os.makedirs(OUT, exist_ok=True)


def normalized_media_path(path):
    path = os.path.normcase(os.path.normpath(path))
    if os.name == "nt":
        if path.startswith("\\\\?\\unc\\"):
            path = "\\\\" + path[8:]
        elif path.startswith("\\\\?\\"):
            path = path[4:]
    return path


class Iris:
    def __init__(self):
        self.private = tempfile.TemporaryDirectory(prefix="iris-photo-",
                                                   dir="/tmp" if sys.platform == "darwin" else None)
        self.endpoint = (r"\\.\pipe\iris-" + os.path.basename(self.private.name)
                         if os.name == "nt" else os.path.join(self.private.name, "control.sock"))
        self.control = None
        self.proc = None
        self.reader = None
        self.lines = queue.Queue()
        self.mpv_msgs = []
        self.log = None
        try:
            self.log = open(os.path.join(OUT, "iris-stderr.log"), "a", encoding="utf-8")
            env = dict(os.environ, IRIS_CONTROL=self.endpoint,
                       IRIS_SETTINGS=os.path.join(self.private.name, "settings.conf"))
            env.update(IRIS_LOG_DIR=os.path.join(self.private.name, "logs"),
                       IRIS_PLUGINS=os.path.join(self.private.name, "plugins"),
                       IRIS_TEST_VAULT=os.path.join(self.private.name, "vault.json"))
            env.pop("IRIS_LOG", None)
            env.pop("GPUI_DISABLE_DIRECT_COMPOSITION", None)
            self.proc = subprocess.Popen([BIN], env=env, stderr=subprocess.PIPE,
                                         text=True, encoding="utf-8", errors="replace", bufsize=1)
            self.reader = threading.Thread(target=self._read, daemon=True)
            self.reader.start()
            self.control = ControlClient(self.endpoint, timeout=15, process=self.proc)
        except BaseException:
            self.quit()
            raise

    def _read(self):
        try:
            for line in self.proc.stderr:
                t = time.perf_counter()
                self.log.write(f"{t:.4f} {line}")
                self.log.flush()
                if line.startswith("[mpv/") or re.search(r"\b(?:WARN|ERROR)\s+iris::mpv(?:\s|\])", line):
                    self.mpv_msgs.append(line.strip())
                self.lines.put((t, line.rstrip("\n")))
        finally:
            self.proc.stderr.close()
            self.log.close()

    def send(self, cmd):
        self.control.send(cmd)

    def wait_for(self, prefix, timeout=20):
        end = time.perf_counter() + timeout
        while True:
            left = end - time.perf_counter()
            if left <= 0:
                raise TimeoutError(prefix)
            try:
                t, line = self.lines.get(timeout=left)
            except queue.Empty:
                raise TimeoutError(prefix)
            if line.startswith(prefix):
                return t, line

    def get(self, prop):
        self.send(f"get {prop}")
        t, line = self.wait_for(f"[iris/control] get {prop} ")
        m = re.match(rf"\[iris/control\] get {re.escape(prop)} = (.*)$", line)
        return (m.group(1) if m else None), t

    def state(self):
        self.send("state")
        return self.wait_for("[iris/control] state")[1]

    def snap(self, path):
        self.send(f"snap {path}")
        _, line = self.wait_for("[iris/control] snap")
        return line.endswith("ok")

    def open_and_wait(self, path, timeout=30):
        """Opens a file; returns seconds until mpv reports video params for it."""
        self.mpv_msgs.clear()
        t0 = time.perf_counter()
        self.send(f"open {path}")
        end = t0 + timeout
        while time.perf_counter() < end:
            p, _ = self.get("path")
            if p is not None and normalized_media_path(p) == normalized_media_path(path):
                w, t = self.get("video-params/w")
                if w is not None:
                    return t - t0
            time.sleep(0.002)
        return None

    def rss_mb(self):
        if os.name == "nt":
            from ctypes import wintypes

            class MemoryCounters(ctypes.Structure):
                _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD),
                            ("PeakWorkingSetSize", ctypes.c_size_t), ("WorkingSetSize", ctypes.c_size_t),
                            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t), ("QuotaPagedPoolUsage", ctypes.c_size_t),
                            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t), ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                            ("PagefileUsage", ctypes.c_size_t), ("PeakPagefileUsage", ctypes.c_size_t)]

            kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            psapi = ctypes.WinDLL("psapi", use_last_error=True)
            kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
            kernel.OpenProcess.restype = wintypes.HANDLE
            kernel.CloseHandle.argtypes = [wintypes.HANDLE]
            kernel.CloseHandle.restype = wintypes.BOOL
            psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(MemoryCounters), wintypes.DWORD]
            psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
            handle = kernel.OpenProcess(0x0400 | 0x0010, False, self.proc.pid)
            if not handle:
                raise ctypes.WinError(ctypes.get_last_error())
            try:
                counters = MemoryCounters()
                counters.cb = ctypes.sizeof(counters)
                if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
                    raise ctypes.WinError(ctypes.get_last_error())
                return counters.WorkingSetSize / (1024 * 1024)
            finally:
                kernel.CloseHandle(handle)
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(self.proc.pid)],
                             capture_output=True, text=True, check=True).stdout.strip()
        return int(out) / 1024 if out else None

    def quit(self):
        try:
            if self.proc is not None and self.proc.poll() is None:
                try:
                    if self.control is not None:
                        self.send("quit")
                        self.proc.wait(timeout=5)
                    else:
                        self.proc.kill()
                        self.proc.wait(timeout=5)
                except (OSError, subprocess.TimeoutExpired):
                    self.proc.kill()
                    self.proc.wait(timeout=5)
        finally:
            if self.control is not None:
                self.control.close()
            if self.reader is not None:
                self.reader.join(timeout=5)
            elif self.log is not None:
                self.log.close()
            if self.endpoint and os.name != "nt":
                try:
                    os.unlink(self.endpoint)
                except FileNotFoundError:
                    pass
            self.private.cleanup()


def video_rect(state_line):
    m = re.search(r"video_rect=Some\((.*?)\) dock=", state_line)
    if not m:
        return None
    nums = [float(x) for x in re.findall(r"-?\d+(?:\.\d+)?", m.group(1))]
    return nums[:4]  # x, y, w, h in window points


def window_width(state_line):
    m = re.search(r"win=\((-?\d+(?:\.\d+)?),", state_line)
    if m:
        return float(m.group(1))
    # Fallback to docked full-width bar bounds when present.
    m = re.search(r"dir=false .*? dock=Some\(\(0(?:\.0+)?,\s*-?\d+(?:\.\d+)?,\s*(\d+(?:\.\d+)?)", state_line)
    return float(m.group(1)) if m else None


def props(iris, names):
    return {n: iris.get(n)[0] for n in names}


results = {}
iris = Iris()

try:
    time.sleep(1.0)
    iris.send("resize 1440 900")
    time.sleep(0.5)

    if "probe" in TESTS:
        print(iris.state())
        f = os.path.join(MEDIA, "exif/exif-o1.jpg")
        print("load", iris.open_and_wait(f))
        time.sleep(0.4)
        print(iris.state())
        print(iris.snap(os.path.join(OUT, "probe.png")))
        print(props(iris, ["current-vo", "video-params", "video-out-params", "gpu-api", "vo-configured"]))

    if "exif" in TESTS:
        r = {}
        for name in ["exif-o1.jpg", "exif-o3.jpg", "exif-o6.jpg", "exif-o8.jpg", "heic-o6.heic"]:
            f = os.path.join(MEDIA, "exif", name)
            load = iris.open_and_wait(f)
            time.sleep(0.4)
            st = iris.state()
            snap = os.path.join(OUT, f"exif-{name}.png")
            iris.snap(snap)
            r[name] = dict(load=load, rect=video_rect(st), snap=snap,
                           **props(iris, ["video-params/rotate", "video-params/w", "video-params/h",
                                          "video-out-params/dw", "video-out-params/dh"]),
                           mpv=list(iris.mpv_msgs))
        results["exif"] = r

    if "heic" in TESTS:
        r = {}
        for name in ["heic-small-1000x750.heic", "heic-48mp-8064x6048.heic"]:
            f = os.path.join(MEDIA, "heic", name)
            load = iris.open_and_wait(f)
            time.sleep(0.4)
            st = iris.state()
            snap = os.path.join(OUT, f"heic-{name}.png")
            iris.snap(snap)
            r[name] = dict(load=load, rect=video_rect(st), snap=snap,
                           **props(iris, ["video-params/w", "video-params/h", "video-params/pixelformat",
                                          "hwdec-current", "estimated-frame-count"]),
                           mpv=list(iris.mpv_msgs))
        results["heic"] = r

    if "icc" in TESTS:
        r = {}
        for name in sorted(os.listdir(os.path.join(MEDIA, "icc"))):
            if os.path.splitext(name)[1].lower() not in {".png", ".jpg", ".jpeg", ".webp"}:
                continue
            f = os.path.join(MEDIA, "icc", name)
            load = iris.open_and_wait(f)
            time.sleep(0.4)
            st = iris.state()
            snap = os.path.join(OUT, f"icc-{name}.png")
            if load is None or not iris.snap(snap):
                raise RuntimeError(f"Could not load or capture ICC fixture {f}")
            r[name] = dict(load=load, rect=video_rect(st), window_width=window_width(st), snap=snap, source=f,
                           **props(iris, ["video-params/primaries", "video-params/gamma",
                                          "video-params/colormatrix"]),
                           mpv=list(iris.mpv_msgs))
        if not r:
            raise RuntimeError("No ICC fixtures found")
        results["icc"] = r

    if "huge" in TESTS:
        r = {}
        for name in ["huge-20000x3000.png", "huge-4000x40000.png", "huge-12000x12000.jpg"]:
            f = os.path.join(MEDIA, "huge", name)
            before = iris.rss_mb()
            load = iris.open_and_wait(f, timeout=60)
            time.sleep(1.0)
            st = iris.state()
            snap = os.path.join(OUT, f"huge-{name}.png")
            iris.snap(snap)
            r[name] = dict(load=load, rect=video_rect(st), snap=snap, rss_before=before,
                           rss_after=iris.rss_mb(),
                           **props(iris, ["video-params/w", "video-params/h", "video-out-params/dw",
                                          "video-out-params/dh"]),
                           mpv=list(iris.mpv_msgs))
        results["huge"] = r

    if "flip" in TESTS:
        r = {}
        for folder, ext in [("flip-jpg", "jpg"), ("flip-heic", "heic")]:
            files = sorted(os.listdir(os.path.join(MEDIA, folder)))
            first = os.path.join(MEDIA, folder, files[0])
            first_load = iris.open_and_wait(first, timeout=60)
            time.sleep(0.5)
            iris.mpv_msgs.clear()
            lat = []
            for name in files[1:]:
                want = re.search(r"-w(\d+)\.", name).group(1)
                t0 = time.perf_counter()
                iris.send("key right")
                while True:
                    w, t = iris.get("video-params/w")
                    if w == want:
                        lat.append(round((t - t0) * 1000))
                        break
                    if time.perf_counter() - t0 > 30:
                        lat.append(None)
                        break
                time.sleep(0.3)
            # Burst: 10 presses 50 ms apart from the last image back to the first ones.
            iris.send("key left")
            time.sleep(1.5)
            target = files[-2 - 10] if len(files) > 12 else files[0]
            want = re.search(r"-w(\d+)\.", target).group(1)
            t0 = time.perf_counter()
            for _ in range(10):
                iris.send("key left")
                time.sleep(0.05)
            burst = None
            while time.perf_counter() - t0 < 60:
                w, t = iris.get("video-params/w")
                if w == want:
                    burst = round((t - t0) * 1000)
                    break
            r[folder] = dict(first_load=first_load, latencies_ms=lat, burst_10_ms=burst,
                             rss=iris.rss_mb(), mpv=list(iris.mpv_msgs)[:20])
        results["flip"] = r
finally:
    iris.quit()

with open(os.path.join(OUT, "results.json"), "a") as fh:
    fh.write(json.dumps(results, ensure_ascii=False) + "\n")
print(json.dumps(results, ensure_ascii=False, indent=1))
