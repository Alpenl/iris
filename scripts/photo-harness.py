#!/usr/bin/env python3
"""Photo edge-case checks for Iris, driven over the IRIS_CONTROL pipe.

Starts its own Iris instance (target/release/iris) with a private FIFO and a
private settings file, so a window you already have open is never touched.
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
import argparse, json, os, queue, re, subprocess, sys, threading, time

_ap = argparse.ArgumentParser(description="Photo edge-case checks for Iris.")
_ap.add_argument("--repo", default=os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                 help="repository root (default: the parent of scripts/)")
_ap.add_argument("--bin", help="Iris binary (default: REPO/target/release/iris)")
_ap.add_argument("outdir")
_ap.add_argument("tests", nargs="+", choices=["probe", "exif", "heic", "icc", "huge", "flip"])
_args = _ap.parse_args()

REPO, OUT = _args.repo, os.path.abspath(_args.outdir)
TESTS = _args.tests
BIN = _args.bin or os.path.join(REPO, "target/release/iris")
MEDIA = os.path.join(REPO, "test-media/photo")
if not os.path.isfile(BIN):
    sys.exit(f"no Iris binary at {BIN}; run `cargo build --release` first")
if not os.path.isdir(MEDIA):
    sys.exit(f"no test media at {MEDIA}; run scripts/make-photo-test-media.sh first")
os.makedirs(OUT, exist_ok=True)


class Iris:
    def __init__(self):
        self.fifo = os.path.join(OUT, "ctl.fifo")
        if os.path.exists(self.fifo):
            os.remove(self.fifo)
        os.mkfifo(self.fifo)
        env = dict(os.environ, IRIS_CONTROL=self.fifo,
                   IRIS_SETTINGS=os.path.join(OUT, "settings.conf"))
        env.pop("GPUI_DISABLE_DIRECT_COMPOSITION", None)
        self.proc = subprocess.Popen([BIN],
                                     env=env, stderr=subprocess.PIPE, text=True, bufsize=1)
        self.lines = queue.Queue()
        self.log = open(os.path.join(OUT, "iris-stderr.log"), "a")
        self.mpv_msgs = []
        threading.Thread(target=self._read, daemon=True).start()
        self.w = open(self.fifo, "w")  # blocks until Iris opens the reader

    def _read(self):
        for line in self.proc.stderr:
            t = time.perf_counter()
            self.log.write(f"{t:.4f} {line}")
            self.log.flush()
            if line.startswith("[mpv/"):
                self.mpv_msgs.append(line.strip())
            self.lines.put((t, line.rstrip("\n")))

    def send(self, cmd):
        self.w.write(cmd + "\n")
        self.w.flush()

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
            if p == path:
                w, t = self.get("video-params/w")
                if w is not None:
                    return t - t0
            time.sleep(0.002)
        return None

    def rss_mb(self):
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(self.proc.pid)],
                             capture_output=True, text=True).stdout.strip()
        return int(out) / 1024 if out else None

    def quit(self):
        try:
            self.send("quit")
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()


def video_rect(state_line):
    m = re.search(r"video_rect=Some\((.*?)\) dock=", state_line)
    if not m:
        return None
    nums = [float(x) for x in re.findall(r"-?\d+(?:\.\d+)?", m.group(1))]
    return nums[:4]  # x, y, w, h in window points


def props(iris, names):
    return {n: iris.get(n)[0] for n in names}


results = {}
iris = Iris()
time.sleep(1.0)
iris.send("resize 1440 900")
time.sleep(0.5)

try:
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
            f = os.path.join(MEDIA, "icc", name)
            load = iris.open_and_wait(f)
            time.sleep(0.4)
            st = iris.state()
            snap = os.path.join(OUT, f"icc-{name}.png")
            iris.snap(snap)
            r[name] = dict(load=load, rect=video_rect(st), snap=snap,
                           **props(iris, ["video-params/primaries", "video-params/gamma",
                                          "video-params/colormatrix"]),
                           mpv=list(iris.mpv_msgs))
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
