#!/usr/bin/env python3
"""Pinned LGPL media SDK. Run on macOS, Linux cross-host, or Windows/MSYS2.

Prerequisites: Python >=3.12, Meson 1.7.0, CMake >=3.27, Ninja, pkg-config,
make, nasm (x86_64), and a POSIX sh. macOS also needs Xcode command line tools.
Windows native builds need MSYS2 sh/make/pkgconf in PATH, NOT MSYS2 media libs.
The Windows target uses checksum-pinned LLVM-MinGW, including on Linux/WSL.
"""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[2]
LOCK = ROOT / "scripts/release/sources.lock.json"


def run(args, cwd=None, env=None):
    print("+", shlex.join(map(str, args)), flush=True)
    result = subprocess.run(list(map(str, args)), cwd=cwd, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    if result.returncode:
        print(result.stdout, file=sys.stderr, flush=True)
        raise subprocess.CalledProcessError(result.returncode, args, output=result.stdout)
    return result.stdout


def download(pin, destination):
    if not destination.exists():
        with urllib.request.urlopen(pin["url"], timeout=180) as response, destination.open("wb") as out:
            shutil.copyfileobj(response, out)
    digest = hashlib.file_digest(destination.open("rb"), "sha256").hexdigest()
    if digest != pin["sha256"]:
        raise RuntimeError(f"Checksum mismatch: {destination}: {digest}")


def extract(archive, destination, strip_root=True):
    destination.mkdir(parents=True)
    if zipfile.is_zipfile(archive):
        with zipfile.ZipFile(archive) as z:
            z.extractall(destination)
        return
    with tarfile.open(archive) as tar:
        members = tar.getmembers()
        for member in members:
            if strip_root:
                member.name = member.name.partition("/")[2]
        tar.extractall(destination, members=[m for m in members if m.name], filter="data")


def source_archive(source, destination, epoch):
    def normalize(info):
        info.uid = info.gid = 0
        info.uname = info.gname = ""
        info.mtime = epoch
        return info
    with destination.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as gz:
        with tarfile.open(fileobj=gz, mode="w") as tar:
            for item in sorted(source.rglob("*")):
                if item.is_file() or item.is_symlink():
                    tar.add(item, arcname=str(item.relative_to(source)), recursive=False, filter=normalize)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=["windows-x86_64", "macos-arm64", "macos-x86_64"])
    parser.add_argument("--work", type=Path, default=ROOT / "target/media-build")
    parser.add_argument("--output", type=Path, default=ROOT / "target/media-sdk")
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    parser.add_argument("--resume", action="store_true", help="Resume completed stages in this exact work/output pair")
    args = parser.parse_args()
    windows = args.target.startswith("windows")
    if not windows and platform.system() != "Darwin":
        parser.error("macOS SDK must be built on macOS")
    arch = "x86_64" if windows else args.target.removeprefix("macos-")
    if not windows and platform.machine() not in ({"arm64", "aarch64"} if arch == "arm64" else {"x86_64"}):
        parser.error("Use a native macOS runner for each architecture; Intel must not be silently skipped")
    work, output = args.work.resolve(), args.output.resolve()
    if (output.exists() or (work / "build").exists()) and not args.resume:
        parser.error("Use fresh paths or --resume for this exact work/output pair")
    if " " in str(work) or " " in str(output):
        parser.error("Native media toolchains require paths without spaces")
    work.mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=args.resume)
    cache = work / "downloads"
    src = work / "sources"
    cache.mkdir(exist_ok=True)
    src.mkdir(exist_ok=args.resume)
    prefix = output / "prefix"
    prefix.mkdir(exist_ok=args.resume)
    evidence = output / "compliance"
    evidence.mkdir(exist_ok=args.resume)
    lock = json.loads(LOCK.read_text())
    epoch = lock["source_date_epoch"]
    identity_path = work / "build-identity.json"
    identity = {"target": args.target, "output": str(output)}
    if identity_path.exists() and json.loads(identity_path.read_text()) != identity:
        parser.error("Resume target/output does not match the original build")
    identity_path.write_text(json.dumps(identity))
    env = os.environ.copy()
    # Never discover Homebrew, MSYS2 or host media dependencies via pkg-config/CMake.
    env.update(SOURCE_DATE_EPOCH=str(epoch), LC_ALL="C", TZ="UTC", PKG_CONFIG_PATH="",
               PKG_CONFIG_LIBDIR=str(prefix / "lib/pkgconfig"), CMAKE_PREFIX_PATH=str(prefix))
    for key in ("CPATH", "LIBRARY_PATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "LDFLAGS", "CFLAGS", "CXXFLAGS"):
        env.pop(key, None)
    records = []
    stamps = work / "completed-commands"
    stamps.mkdir(exist_ok=True)
    def execute(command, cwd=None):
        key = hashlib.sha256((str(cwd) + json.dumps(list(map(str, command)))).encode()).hexdigest()
        stamp = stamps / key
        if args.resume and stamp.exists():
            return stamp.read_text()
        try:
            text = run(command, cwd, env)
        except subprocess.CalledProcessError as failure:
            with (evidence / "commands.log").open("a", encoding="utf-8") as log:
                log.write("FAILED $ " + shlex.join(map(str, command)) + "\n" + failure.output + "\n")
            raise
        with (evidence / "commands.log").open("a", encoding="utf-8") as log:
            log.write("$ " + shlex.join(map(str, command)) + "\n" + text + "\n")
        stamp.write_text(text)
        print(text, flush=True)
        return text
    for tool in ("meson", "cmake", "ninja", "pkg-config", "make", "sh"):
        if not shutil.which(tool):
            raise RuntimeError(f"Required build tool not found: {tool}")
    if arch == "x86_64" and not shutil.which("nasm"):
        raise RuntimeError("nasm is required for x86_64 SIMD")
    toolchain = None
    cmake_platform = []
    meson_platform = []
    ff_platform = []
    if windows:
        pin = lock["windows_toolchain" if os.name == "nt" else "linux_windows_toolchain"]
        archive = cache / ("toolchain.zip" if os.name == "nt" else "toolchain.tar.xz")
        download(pin, archive)
        if not (work / "toolchain").exists():
            extract(archive, work / "toolchain", strip_root=False)
        toolchain = work / "toolchain" / pin["directory"]
        env["PATH"] = str(toolchain / "bin") + os.pathsep + env["PATH"]
        cc, cxx = "x86_64-w64-mingw32-clang", "x86_64-w64-mingw32-clang++"
        cross = work / "windows.ini"
        cross.write_text("[binaries]\nc = '" + cc + "'\ncpp = '" + cxx + "'\nar = 'llvm-ar'\nstrip = 'llvm-strip'\nwindres = 'x86_64-w64-mingw32-windres'\npkg-config = 'pkg-config'\n[host_machine]\nsystem = 'windows'\ncpu_family = 'x86_64'\ncpu = 'x86_64'\nendian = 'little'\n[properties]\nneeds_exe_wrapper = true\n")
        meson_platform = ["--cross-file", str(cross)]
        cmake_platform = ["-DCMAKE_SYSTEM_NAME=Windows", "-DCMAKE_RC_COMPILER=x86_64-w64-mingw32-windres"]
        ff_platform = ["--target-os=mingw32", "--arch=x86_64", "--enable-cross-compile",
                       "--cc=" + cc, "--cxx=" + cxx, "--ar=llvm-ar", "--ranlib=llvm-ranlib", "--strip=llvm-strip",
                       "--enable-d3d11va", "--enable-dxva2", "--enable-schannel"]
        # LLVM's permissive runtimes are statically linked; only our LGPL DLLs remain replaceable.
        env["LDFLAGS"] = "-static-libstdc++ -static-libgcc"
    else:
        cc, cxx = "clang", "clang++"
        env["MACOSX_DEPLOYMENT_TARGET"] = "11.0"
        cmake_platform = ["-DCMAKE_OSX_ARCHITECTURES=" + arch, "-DCMAKE_OSX_DEPLOYMENT_TARGET=11.0"]
        ff_platform = ["--arch=" + ("aarch64" if arch == "arm64" else arch), "--cc=clang", "--cxx=clang++",
                       "--enable-videotoolbox", "--enable-audiotoolbox", "--enable-securetransport",
                       "--install-name-dir=@rpath"]
    env.update(CC=cc, CXX=cxx)
    execute([cc, "--version"])
    for tool in ("meson", "cmake", "ninja", "pkg-config", "make"):
        records.append({"tool": tool, "version": execute([tool, "--version"]).splitlines()[0]})
    needed = list(lock["sources"])
    if not windows:
        needed = [n for n in needed if n not in {"shaderc", "spirv-cross", "spirv-headers", "spirv-tools", "glslang", "llvm-mingw", "mingw-w64", "llvm-runtime"}]
    for name in needed:
        pin = lock["sources"][name]
        if re.search(r"(?<!L)GPL|nonfree", pin["license"], re.I):
            raise RuntimeError(f"Disallowed source license: {name}")
        archive = cache / (name + ".tar.gz")
        download(pin, archive)
        if not (src / name).exists():
            extract(archive, src / name)
    # Explicit vendoring, not network-capable Meson/CMake fallback downloads.
    for name, destination in (("fast-float", "fast_float"), ("glad", "glad"), ("jinja", "jinja"), ("markupsafe", "markupsafe"), ("vulkan-headers", "Vulkan-Headers")):
        target = src / "libplacebo/3rdparty" / destination
        if target.exists() and not any(target.iterdir()):
            target.rmdir()
        if not target.exists():
            shutil.copytree(src / name, target)
    shutil.copytree(src / "vulkan-headers/include", prefix / "include", dirs_exist_ok=True)
    env["PYTHONPATH"] = os.pathsep.join(str(src / n / suffix) for n, suffix in (("glad", "."), ("jinja", "src"), ("markupsafe", "src")))
    def meson(name, options):
        build = work / "build" / name
        command = ["meson", "setup", str(build), str(src / name), "--prefix=" + str(prefix),
                   "--libdir=lib", "--buildtype=release", "--default-library=shared", "--wrap-mode=nofallback",
                   "--auto-features=disabled", *meson_platform, *options]
        if windows:
            command += ["-Dc_link_args=['-static-libstdc++','-static-libgcc']", "-Dcpp_link_args=['-static-libstdc++','-static-libgcc']"]
        execute(command)
        if name == "libplacebo":
            # Required even with Vulkan disabled: its public ABI includes Vulkan types.
            execute(["meson", "setup", "--reconfigure", str(build), str(src / name),
                     "-Dc_args=-I" + str(src / "vulkan-headers/include")])
        execute(["meson", "compile", "-C", str(build), "-j", args.jobs])
        execute(["meson", "install", "-C", str(build)])
        shutil.copytree(build / "meson-info", evidence / (name + "-meson-info"), dirs_exist_ok=True)
        shutil.copy2(build / "meson-logs/meson-log.txt", evidence / (name + "-meson.log"))
    def cmake(name, options):
        build = work / "build" / name
        execute(["cmake", "-S", str(src / name), "-B", str(build), "-G", "Ninja",
                 "-DCMAKE_BUILD_TYPE=Release", "-DCMAKE_INSTALL_PREFIX=" + str(prefix), "-DCMAKE_INSTALL_LIBDIR=lib",
                 "-DCMAKE_C_COMPILER=" + cc, "-DCMAKE_CXX_COMPILER=" + cxx,
                 "-DCMAKE_POSITION_INDEPENDENT_CODE=ON", "-DCMAKE_FIND_USE_PACKAGE_REGISTRY=OFF",
                 "-DCMAKE_FIND_USE_SYSTEM_PACKAGE_REGISTRY=OFF", *cmake_platform, *options])
        execute(["cmake", "--build", str(build), "--parallel", args.jobs])
        execute(["cmake", "--install", str(build)])
        shutil.copy2(build / "CMakeCache.txt", evidence / (name + "-CMakeCache.txt"))
    meson("freetype", ["-Dzlib=internal", "-Dharfbuzz=disabled", "-Dbrotli=disabled", "-Dbzip2=disabled", "-Dpng=disabled", "-Dtests=disabled"])
    meson("fribidi", ["-Ddocs=false", "-Dbin=false", "-Dtests=false"])
    meson("harfbuzz", ["-Dfreetype=enabled", "-Dtests=disabled", "-Dutilities=disabled"])
    meson("libass", ["-Dfontconfig=disabled", "-Dlibunibreak=disabled", "-Dasm=" + ("enabled" if arch == "x86_64" else "disabled"),
                     "-Ddirectwrite=" + ("enabled" if windows else "disabled"), "-Dcoretext=" + ("disabled" if windows else "enabled")])
    meson("lcms2", ["-Dtests=disabled", "-Dutils=false", "-Dfastfloat=false", "-Dthreaded=false"])
    if windows:
        for name in ("glslang", "spirv-tools", "spirv-headers"):
            target = src / "shaderc/third_party" / name
            if target.exists() and not any(target.iterdir()):
                target.rmdir()
            if not target.exists():
                shutil.copytree(src / name, target)
        cmake("shaderc", ["-DSHADERC_SKIP_TESTS=ON", "-DSHADERC_SKIP_EXAMPLES=ON", "-DSHADERC_SKIP_COPYRIGHT_CHECK=ON",
                          "-DSHADERC_ENABLE_WERROR_COMPILE=OFF", "-DSPIRV_SKIP_EXECUTABLES=ON", "-DSPIRV_SKIP_TESTS=ON",
                          "-DENABLE_GLSLANG_BINARIES=OFF", "-DGLSLANG_TESTS=OFF", "-DBUILD_SHARED_LIBS=OFF",
                          "-DCMAKE_SHARED_LINKER_FLAGS=-static-libstdc++ -static-libgcc"])
        cmake("spirv-cross", ["-DSPIRV_CROSS_SHARED=ON", "-DSPIRV_CROSS_STATIC=OFF", "-DSPIRV_CROSS_CLI=OFF",
                              "-DSPIRV_CROSS_ENABLE_TESTS=OFF", "-DCMAKE_SHARED_LINKER_FLAGS=-static-libstdc++ -static-libgcc"])
    meson("libplacebo", ["-Ddemos=false", "-Dtests=false", "-Dopengl=enabled", "-Dlcms=enabled", "-Dglslang=disabled",
                        "-Dvulkan=disabled", "-Dd3d11=" + ("enabled" if windows else "disabled"),
                        "-Dshaderc=" + ("enabled" if windows else "disabled")])
    meson("dav1d", ["-Denable_tools=false", "-Denable_tests=false", "-Denable_examples=false", "-Denable_docs=false"])
    cmake("zlib", ["-DZLIB_BUILD_EXAMPLES=OFF", "-DCMAKE_POLICY_VERSION_MINIMUM=3.5",
                   "-DINSTALL_PKGCONFIG_DIR=" + str(prefix / "lib/pkgconfig")])
    if windows:
        # Upstream CMake installs libzlib.dll.a but its own zlib.pc specifies -lz.
        # Use the identical import table under pkg-config's canonical linker name.
        shutil.copy2(prefix / "lib/libzlib.dll.a", prefix / "lib/libz.dll.a")
    ffbuild = work / "build/ffmpeg"
    ffbuild.mkdir(exist_ok=args.resume)
    execute(["sh", str(src / "ffmpeg/configure"), "--prefix=" + str(prefix), "--libdir=" + str(prefix / "lib"),
             "--enable-shared", "--disable-static", "--disable-gpl", "--disable-version3", "--disable-nonfree",
             "--disable-autodetect", "--disable-programs", "--disable-doc", "--disable-avdevice",
             "--disable-debug", "--enable-pic", "--disable-chromaprint", "--enable-libdav1d", "--enable-zlib",
             "--pkg-config=pkg-config", "--extra-cflags=-I" + str(prefix / "include"),
             "--extra-ldflags=-L" + str(prefix / "lib"), *ff_platform], ffbuild)
    config = (ffbuild / "config.h").read_text()
    for flag in ("GPL", "VERSION3", "NONFREE", "CHROMAPRINT"):
        if not re.search(r"^#define CONFIG_" + flag + r" 0$", config, re.M):
            raise RuntimeError("FFmpeg license configuration rejected: " + flag)
    components = (ffbuild / "config_components.h").read_text()
    for capability in ("HEVC_DECODER", "LIBDAV1D_DECODER", "PNG_DECODER", "MJPEG_DECODER", "TIFF_DECODER", "WEBP_DECODER",
                       "MOV_DEMUXER", "SCALE_FILTER", "FORMAT_FILTER", "HFLIP_FILTER", "VFLIP_FILTER"):
        if not re.search(r"^#define CONFIG_" + capability + r" 1$", components, re.M):
            raise RuntimeError("Required photo capability missing: " + capability)
    shutil.copy2(ffbuild / "config_components.h", evidence / "ffmpeg-config_components.h")
    execute(["make", "-j", args.jobs, *(["DLLTOOL=llvm-dlltool"] if windows else [])], ffbuild)
    if windows:
        # FFmpeg's MinGW make rule ignores import-library generator failures.
        # Generate and require every MSVC import library explicitly before installation.
        for name in ("avcodec", "avfilter", "avformat", "avutil", "swresample", "swscale"):
            definitions = list((ffbuild / ("lib" + name)).glob(name + "-*.def"))
            if len(definitions) != 1:
                raise RuntimeError("Missing FFmpeg export definition: " + name)
            execute(["llvm-dlltool", "-m", "i386:x86-64", "-d", str(definitions[0]), "-D", definitions[0].stem + ".dll",
                     "-l", str(ffbuild / ("lib" + name) / (name + ".lib"))])
    execute(["make", "install", *(["DLLTOOL=llvm-dlltool"] if windows else [])], ffbuild)
    for file in ("config.h", "ffbuild/config.mak", "ffbuild/config.log"):
        shutil.copy2(ffbuild / file, evidence / ("ffmpeg-" + Path(file).name))
    options = ["-Dgpl=false", "-Dlibmpv=true", "-Dcplayer=false", "-Dbuild-date=false", "-Dlua=disabled",
               "-Dlcms2=enabled", "-Dgl=enabled", "-Dplain-gl=enabled", "-Dvector=enabled",
               "-Dmanpage-build=disabled", "-Dhtml-build=disabled", "-Dpdf-build=disabled"]
    if windows:
        options += ["-Dwin32-threads=enabled", "-Dwasapi=enabled", "-Dd3d11=enabled", "-Dshaderc=enabled",
                    "-Dspirv-cross=enabled", "-Dd3d-hwaccel=enabled", "-Dgl-win32=enabled"]
    else:
        options += ["-Dcoreaudio=enabled", "-Dgl-cocoa=enabled", "-Dcocoa=enabled", "-Dvideotoolbox-gl=enabled"]
    meson("mpv", options)
    mpvoptions = json.loads((work / "build/mpv/meson-info/intro-buildoptions.json").read_text())
    values = {entry["name"]: entry["value"] for entry in mpvoptions}
    for name, expected in (("gpl", False), ("libmpv", True), ("cplayer", False), ("default_library", "shared")):
        if values.get(name) != expected:
            raise RuntimeError(f"mpv license configuration rejected: {name}")
    mpvconfig = (work / "build/mpv/config.h").read_text()
    if not re.search(r"^#define HAVE_GPL 0$", mpvconfig, re.M):
        raise RuntimeError("mpv actual compiled configuration enables GPL")
    if windows and not re.search(r"^#define HAVE_D3D11 1$", mpvconfig, re.M):
        raise RuntimeError("D3D11 was not compiled")
    shutil.copy2(work / "build/mpv/config.h", evidence / "mpv-config.h")
    # Some upstream versions omit this public header from their install list.
    # Install the unmodified header from the exact pinned corresponding source.
    shutil.copy2(src / "mpv/include/mpv/render_d3d11.h", prefix / "include/mpv/render_d3d11.h")
    if windows:
        dlls = list((prefix / "bin").glob("*mpv*.dll"))
        if len(dlls) != 1:
            raise RuntimeError("Expected exactly one libmpv DLL")
        symbols = execute(["llvm-readobj", "--coff-exports", str(dlls[0])])
        exports = re.findall(r"^\s+Name: (\w+)$", symbols, re.M)
        if "mpv_render_context_create" not in exports:
            raise RuntimeError("libmpv client exports missing")
        definition = prefix / "lib/mpv.def"
        definition.write_text("LIBRARY " + dlls[0].name + "\nEXPORTS\n" + "\n".join(exports) + "\n")
        execute(["llvm-dlltool", "-m", "i386:x86-64", "-d", str(definition), "-l", str(prefix / "lib/mpv.lib")])
    licenses = evidence / "licenses"
    licenses.mkdir(exist_ok=args.resume)
    for name in needed:
        found = []
        for file in sorted((src / name).rglob("*")):
            if file.is_file() and re.match(r"(?i)(copying|copyright|licen[sc]e|notice|ftl)([.\-_].*)?$", file.name):
                relative = file.relative_to(src / name)
                target = licenses / name / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(file, target)
                found.append(str(relative))
        if not found:
            raise RuntimeError("No upstream license notices found: " + name)
    shutil.copy2(LOCK, evidence / LOCK.name)
    shutil.copytree(ROOT / "scripts/release", src / "iris-build-recipes", dirs_exist_ok=True, ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
    shutil.copytree(evidence, src / "build-evidence", dirs_exist_ok=True, ignore=shutil.ignore_patterns("corresponding-source.tar.gz"))
    source_archive(src, evidence / "corresponding-source.tar.gz", epoch)
    inventory = {p.relative_to(prefix).as_posix(): hashlib.file_digest(p.open("rb"), "sha256").hexdigest()
                 for p in sorted(prefix.rglob("*")) if p.is_file()}
    manifest = {"schema": 1, "target": args.target, "host": platform.platform(), "source_date_epoch": epoch,
                "sources": {n: lock["sources"][n] for n in needed}, "tools": records,
                "ffmpeg": {"gpl": False, "version3": False, "nonfree": False, "chromaprint": False, "shared": True},
                "mpv": {"gpl": False, "libmpv": True, "cplayer": False, "d3d11": windows},
                "files": inventory, "signing": "unsigned media SDK; sign each library during application packaging"}
    (evidence / "build-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (evidence / "NOTICES.txt").write_text(
        "Iris media SDK: dynamically linked LGPL-2.1-or-later mpv, FFmpeg, libplacebo and FriBidi.\n"
        "No GPL, version3, nonfree, FFTW or Chromaprint components are enabled.\n"
        "FreeType is used under its FreeType License (FTL), not its alternate GPL license.\n"
        "This software uses the FreeType Project (https://freetype.org).\n"
        "Full upstream license and copyright notices are in licenses/. Exact sources, configuration, and recipes\n"
        "are in corresponding-source.tar.gz. You may replace the shared media libraries with compatible modified\n"
        "versions; reverse engineering for debugging such modifications is permitted under the LGPL.\n"
        "Apple Developer ID signatures can be removed and replaced by your own signatures for modified local builds.\n")
    print("Media SDK created:", output)


if __name__ == "__main__":
    main()
