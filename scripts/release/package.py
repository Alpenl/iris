#!/usr/bin/env python3
"""Build Iris against the verified media SDK and package without system media fallbacks.

Use --mode local for explicitly unsigned distributions. --mode release requires
platform signing credentials (and macOS notarization); incomplete credentials fail.
Windows requires MSVC, the Windows SDK, and Inno Setup 6 (ISCC in PATH).
Rust license compliance requires cargo-deny and an existing Cargo.lock.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import zipfile

ROOT = Path(__file__).resolve().parents[2]
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parent))
from importlib.util import spec_from_file_location, module_from_spec
_spec = spec_from_file_location("media_recipe", Path(__file__).with_name("build-media.py"))
_media = module_from_spec(_spec)
_spec.loader.exec_module(_media)
run = _media.run


def sha(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def application_inputs():
    files = [
        ROOT / name
        for name in (
            "Cargo.toml",
            "Cargo.lock",
            "deny.toml",
            "rust-toolchain.toml",
            "LICENSE-MIT",
            "LICENSE-APACHE",
        )
        if (ROOT / name).is_file()
    ]
    files += [
        p
        for directory in ("crates", "Resources")
        for p in (ROOT / directory).rglob("*")
        if p.is_file() and p.name != ".DS_Store"
    ]
    return {p.relative_to(ROOT).as_posix(): sha(p) for p in sorted(files)}


def save_checkpoint(path, state):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(state, indent=2) + "\n")
    temporary.replace(path)


def verify_vendor_archive(path):
    expected, actual = {}, {}
    with tarfile.open(path, mode="r|gz") as archive:
        for member in archive:
            if not member.isfile():
                continue
            file = archive.extractfile(member)
            if member.name.endswith("/.cargo-checksum.json"):
                checksum = json.load(file)
                base = member.name.rsplit("/", 1)[0]
                expected.update({base + "/" + name: digest for name, digest in checksum["files"].items()})
            else:
                actual[member.name] = hashlib.file_digest(file, "sha256").hexdigest()
    if not expected or any(actual.get(name) != digest for name, digest in expected.items()):
        raise RuntimeError("Archived vendored source checksum mismatch")


def imports_pe(path):
    """Read both ordinary and delay-load PE imports; never execute target code."""
    data = path.read_bytes()
    pe = struct.unpack_from("<I", data, 0x3c)[0]
    if data[pe:pe+4] != b"PE\0\0":
        raise RuntimeError(f"Not a PE binary: {path}")
    machine, count = struct.unpack_from("<HH", data, pe + 4)
    if machine != 0x8664:
        raise RuntimeError(f"Not Windows x64: {path}")
    size = struct.unpack_from("<H", data, pe + 20)[0]
    optional = pe + 24
    if struct.unpack_from("<H", data, optional)[0] != 0x20b:
        raise RuntimeError("PE32+ expected")
    sections = [struct.unpack_from("<IIII", data, optional + size + i*40 + 8) for i in range(count)]
    def offset(rva):
        for virtual_size, virtual_address, raw_size, raw_offset in sections:
            if virtual_address <= rva < virtual_address + max(virtual_size, raw_size):
                return raw_offset + rva - virtual_address
        raise RuntimeError(f"PE RVA out of range in {path}")
    def string(rva):
        start = offset(rva)
        return data[start:data.index(0, start)].decode("ascii")
    names = []
    for index, width in ((1, 20), (13, 32)):
        rva, length = struct.unpack_from("<II", data, optional + 112 + index*8)
        if not rva:
            continue
        start = offset(rva)
        for pos in range(start, start + length, width):
            descriptor = data[pos:pos+width]
            if not any(descriptor):
                break
            values = struct.unpack("<" + "I"*(width//4), descriptor)
            if index == 13 and not values[0] & 1:
                raise RuntimeError("Unsupported VA-based delay import")
            names.append(string(values[3] if index == 1 else values[1]))
    return names


def windows_dependencies(executable, prefix, manifest, destination):
    available = {p.name.lower(): p for p in prefix.rglob("*.dll")}
    # OS components only. Third party runtimes must come from our checksum manifest.
    system = set("advapi32 avrt bcrypt cabinet cfgmgr32 comctl32 comdlg32 crypt32 cryptbase cryptnet d2d1 d3d11 d3d12 d3dcompiler_47 dcomp dbghelp dnsapi dwrite dwmapi dxgi dxva2 gdi32 hid imm32 iphlpapi kernel32 kernelbase mf mfplat mfreadwrite mfuuid mmdevapi mpr msimg32 msvcrt ncrypt netapi32 normaliz ntdll ole32 oleacc oleaut32 opengl32 powrprof propsys psapi rpcrt4 secur32 setupapi shell32 shcore shlwapi synchronization ucrtbase user32 userenv usp10 uxtheme version windowscodecs winhttp wininet winmm winscard winspool wintrust ws2_32 wtsapi32".split())
    system.update({"bcryptprimitives", "icuuc"})
    queue = [executable]
    seen = set()
    graph = {}
    while queue:
        binary = queue.pop()
        if binary.name.lower() in seen:
            continue
        seen.add(binary.name.lower())
        deps = imports_pe(binary)
        graph[binary.name] = deps
        for dep in deps:
            key = dep.lower()
            if key in available:
                source = available[key]
                relative = source.relative_to(prefix).as_posix()
                if manifest["files"].get(relative) != sha(source):
                    raise RuntimeError("Unverified media DLL: " + str(source))
                target = destination / source.name
                if not target.exists():
                    shutil.copy2(source, target)
                queue.append(target)
            elif key.removesuffix(".dll") not in system and not key.startswith(("api-ms-win-", "ext-ms-win-")):
                raise RuntimeError(f"Unapproved external DLL {dep} imported by {binary}; refusing host fallback")
    return graph


def mac_dependencies(executable, prefix, manifest, frameworks, arch):
    available = {p.name: p for p in (prefix / "lib").glob("*.dylib")}
    queue = [executable]
    seen = set()
    graph = {}
    while queue:
        binary = queue.pop()
        if binary.name in seen:
            continue
        seen.add(binary.name)
        archs = run(["lipo", "-archs", binary]).strip().split()
        if arch not in archs:
            raise RuntimeError(f"Wrong architecture: {binary}: {archs}")
        deps = [line.strip().split(" (", 1)[0] for line in run(["otool", "-L", binary]).splitlines()[1:]]
        graph[binary.name] = deps
        identity = run(["otool", "-D", binary]).splitlines()[1:]
        if identity:
            run(["install_name_tool", "-id", "@rpath/" + binary.name, binary])
        for dep in deps:
            if identity and dep == identity[0].strip():
                continue
            if dep.startswith(("/System/Library/", "/usr/lib/")):
                continue
            if dep.startswith(("/opt/homebrew/", "/usr/local/", "/opt/local/")):
                raise RuntimeError("Homebrew/MacPorts media forbidden: " + dep)
            name = Path(dep).name
            source = available.get(name)
            if source is None or (dep.startswith("/") and not Path(dep).is_relative_to(prefix)):
                raise RuntimeError("External dylib outside pinned SDK: " + dep)
            if manifest["files"].get(source.relative_to(prefix).as_posix()) != sha(source):
                raise RuntimeError("Unverified media dylib: " + str(source))
            target = frameworks / name
            if not target.exists():
                shutil.copy2(source, target, follow_symlinks=True)
                queue.append(target)
            run(["install_name_tool", "-change", dep, "@rpath/" + name, binary])
        # Remove every build-machine rpath, including implicit Cargo/build.rs paths.
        loads = run(["otool", "-l", binary])
        for rpath in re.findall(r"cmd LC_RPATH\s+cmdsize \d+\s+path (.*?) \(offset", loads):
            run(["install_name_tool", "-delete_rpath", rpath, binary])
        run(["install_name_tool", "-add_rpath", "@executable_path/../Frameworks" if binary == executable else "@loader_path", binary])
    return graph


def rust_notices(destination, env, checkpoint, state):
    if state.get("rust_notices"):
        if not (destination / "rust-license-manifest.json").is_file():
            raise RuntimeError("Completed Rust notices artifact missing")
        return
    if not state.get("license_gate"):
        run(["cargo", "deny", "--locked", "check", "licenses"], ROOT, env)
        state["license_gate"] = True
        save_checkpoint(checkpoint, state)
    vendor = destination / "rust-vendor"
    if not state.get("vendor"):
        vendored = subprocess.run(["cargo", "vendor", "--locked", "--versioned-dirs", str(vendor)],
                                  cwd=ROOT, env=env, check=True, stdout=subprocess.PIPE, text=True)
        config = vendored.stdout.replace(str(vendor).replace("\\", "\\\\"), "rust-vendor")
        (destination / "cargo-vendor-config.toml").write_text(config)
        state["vendor"] = True
        save_checkpoint(checkpoint, state)
    if not vendor.is_dir() or not (destination / "cargo-vendor-config.toml").is_file():
        raise RuntimeError("Completed vendor checkpoint artifacts are missing")
    # Cargo diagnostics belong on stderr; only stdout is a JSON document.
    metadata_result = subprocess.run(["cargo", "metadata", "--locked", "--format-version", "1"],
                                     cwd=ROOT, env=env, check=True, stdout=subprocess.PIPE, text=True)
    metadata = json.loads(metadata_result.stdout)
    notices = destination / "rust-licenses"
    notices.mkdir(exist_ok=True)
    entries = []
    for package in metadata["packages"]:
        if package["source"] is None:
            continue
        directory = vendor / (package["name"] + "-" + package["version"])
        if not directory.is_dir():
            raise RuntimeError("Vendored crate missing: " + str(directory))
        checksum_path = directory / ".cargo-checksum.json"
        if not checksum_path.is_file():
            raise RuntimeError("Vendored crate checksum manifest missing: " + str(directory))
        checksums = json.loads(checksum_path.read_text())
        for relative, digest in checksums["files"].items():
            file = directory / relative
            if not file.is_file() or sha(file) != digest:
                raise RuntimeError("Vendored crate file checksum mismatch: " + str(file))
        files = []
        for file in directory.rglob("*"):
            if file.is_file() and re.match(r"(?i)(licen[sc]e|copying|copyright|notice)([.\-_].*)?$", file.name):
                target = notices / directory.name / file.relative_to(directory)
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(file, target)
                files.append(str(file.relative_to(directory)))
        if package["license_file"]:
            custom = Path(package["license_file"])
            original = Path(package["manifest_path"]).parent / custom
            target = notices / directory.name / custom.name
            if not target.exists():
                if not original.is_file():
                    raise RuntimeError("Declared license file missing: " + str(original))
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(original, target)
                files.append(str(custom))
        entries.append({"name": package["name"], "version": package["version"], "license": package["license"], "license_file": package["license_file"], "notices": files})
    (destination / "rust-license-manifest.json").write_text(json.dumps(entries, indent=2) + "\n")
    for name in ("LICENSE-MIT", "LICENSE-APACHE"):
        shutil.copy2(ROOT / name, destination / name)
    state["rust_notices"] = True
    save_checkpoint(checkpoint, state)


def application_source(destination):
    # Bundle only explicit application build inputs (never docs/, examples/, or web assets).
    app = destination / "iris-source"
    app.mkdir(parents=True, exist_ok=True)
    for rel in application_inputs():
        file = ROOT / rel
        target = app / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(file, target)
    shutil.copytree(
        ROOT / "scripts/release",
        app / "scripts/release",
        dirs_exist_ok=True,
        ignore=shutil.ignore_patterns(".DS_Store", "__pycache__", "*.pyc"),
    )
    for name in ("LICENSE-MIT", "LICENSE-APACHE"):
        shutil.copy2(ROOT / name, destination / name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--version", required=True, help="Three numeric components, e.g. 0.1.0")
    parser.add_argument("--mode", choices=["local", "release"], required=True)
    parser.add_argument("--resume", action="store_true", help="Resume only when application and SDK inputs are unchanged")
    parser.add_argument("--adopt-completed-vendor", action="store_true",
                        help="Migrate a pre-checkpoint run known to have completed build/license/vendor; requires --resume")
    parser.add_argument("--reuse-rust", type=Path, help="Reuse verified Rust source/license artifacts from a prior package output")
    parser.add_argument(
        "--source-mode",
        choices=["embedded", "sidecar"],
        default="embedded",
        help="embedded (default) bundles corresponding source tarballs inside compliance/; sidecar emits them alongside the package",
    )
    args = parser.parse_args()
    if not re.fullmatch(r"\d+\.\d+\.\d+", args.version):
        parser.error("version must be numeric major.minor.patch")
    sdk, output = args.sdk.resolve(), args.output.resolve()
    compliance = sdk / "compliance"
    manifest = json.loads((compliance / "build-manifest.json").read_text())
    prefix = sdk / "prefix"
    windows = manifest["target"] == "windows-x86_64"
    if windows != (os.name == "nt"):
        parser.error("Package on the target OS (Windows MSVC or native macOS)")
    if output.exists() and not args.resume:
        parser.error("Use a fresh output directory or --resume")
    if args.adopt_completed_vendor and not args.resume:
        parser.error("--adopt-completed-vendor requires --resume")
    checkpoint = output / "package-checkpoint.json"
    identity = {"application": application_inputs(), "sdk_manifest": sha(compliance / "build-manifest.json"),
                "mode": args.mode, "version": args.version, "target": manifest["target"]}
    if args.source_mode != "embedded":
        identity["source_mode"] = args.source_mode
    state = {"identity": identity}
    if checkpoint.exists():
        state = json.loads(checkpoint.read_text())
        if state["identity"] != identity:
            parser.error("Application, SDK, mode or version changed; refusing stale package resume")
    elif args.resume and not args.adopt_completed_vendor:
        parser.error("No checkpoint exists; legacy successful vendor stage needs explicit --adopt-completed-vendor")
    # Verify the entire SDK before compiling, including import libraries and headers.
    for relative, digest in manifest["files"].items():
        file = prefix / relative
        if not file.is_file() or sha(file) != digest:
            raise RuntimeError("SDK checksum mismatch: " + relative)
    if manifest["ffmpeg"] != {"gpl": False, "version3": False, "nonfree": False, "chromaprint": False, "shared": True}:
        raise RuntimeError("Media manifest license gate rejected")
    for key, value in (("gpl", False), ("libmpv", True), ("cplayer", False)):
        if manifest["mpv"].get(key) != value:
            raise RuntimeError("mpv manifest license gate rejected")
    if args.mode == "release":
        required = ["IRIS_WINDOWS_PFX", "IRIS_WINDOWS_PFX_PASSWORD", "IRIS_TIMESTAMP_URL"] if windows else ["IRIS_MAC_SIGN_IDENTITY", "IRIS_NOTARY_PROFILE"]
        for name in required:
            if not os.environ.get(name):
                raise RuntimeError("Signed release requires " + name)
    output.mkdir(parents=True, exist_ok=args.resume)
    if not args.resume:
        save_checkpoint(checkpoint, state)
    if not (output / "media-runtime-verification.json").is_file():
        verification_env = os.environ.copy()
        if not windows:
            verification_env["DYLD_LIBRARY_PATH"] = str(prefix / "lib")
        verification = run([sys.executable, Path(__file__).with_name("verify-media.py"), "--sdk", sdk], env=verification_env)
        (output / "media-runtime-verification.json").write_text(verification)
    env = os.environ.copy()
    env["MPV_LIB_DIR"] = str(prefix / "lib")
    env["PKG_CONFIG_PATH"] = str(prefix / "lib/pkgconfig")
    target = {"windows-x86_64": "x86_64-pc-windows-msvc", "macos-arm64": "aarch64-apple-darwin", "macos-x86_64": "x86_64-apple-darwin"}[manifest["target"]]
    if windows:
        # Avoid shipping an untracked VCRUNTIME DLL; this affects Iris, not the replaceable media DLLs.
        env["RUSTFLAGS"] = "-C target-feature=+crt-static"
    else:
        # Header padding is required for install_name_tool to rewrite load commands.
        env["RUSTFLAGS"] = "-C link-arg=-Wl,-headerpad_max_install_names"
    cargo_target = Path(env.get("CARGO_TARGET_DIR", str(output / "cargo-target"))).resolve()
    env["CARGO_TARGET_DIR"] = str(cargo_target)
    built = cargo_target / target / "release" / ("iris.exe" if windows else "iris")
    if args.adopt_completed_vendor:
        prior_notices = output / ("stage/Iris/compliance" if windows else "stage/Iris.app/Contents/Resources/compliance")
        config = prior_notices / "cargo-vendor-config.toml"
        if not built.is_file() or not config.is_file() or not (prior_notices / "rust-vendor").is_dir():
            parser.error("Legacy build/vendor artifacts are incomplete")
        # Older runs did not record hashes: reject any input newer than their actual build.
        if any((ROOT / name).stat().st_mtime_ns > built.stat().st_mtime_ns for name in identity["application"]):
            parser.error("Application/license inputs changed after legacy build; refusing adoption")
        staged = output / ("stage/Iris/iris.exe" if windows else "stage/Iris.app/Contents/MacOS/iris")
        if not staged.is_file() or sha(staged) != sha(built):
            parser.error("Legacy staged executable differs from successful build")
        state.update(build=sha(built), license_gate=True, vendor=True)
        save_checkpoint(checkpoint, state)
    if not state.get("build"):
        run(["cargo", "build", "--locked", "--release", "--target", target, "-p", "iris"], ROOT, env)
        state["build"] = sha(built)
        save_checkpoint(checkpoint, state)
    elif not built.is_file() or sha(built) != state["build"]:
        raise RuntimeError("Completed application build artifact changed or disappeared")
    stage = output / "stage"
    stage.mkdir(exist_ok=args.resume)
    if windows:
        payload = stage / "Iris"
        payload.mkdir(exist_ok=args.resume)
        executable = payload / "iris.exe"
        frameworks = payload
    else:
        payload = stage / "Iris.app"
        contents = payload / "Contents"
        (contents / "MacOS").mkdir(parents=True, exist_ok=args.resume)
        frameworks = contents / "Frameworks"
        frameworks.mkdir(exist_ok=args.resume)
        executable = contents / "MacOS/iris"
        with (contents / "Info.plist").open("wb") as file:
            plistlib.dump({"CFBundleExecutable": "iris", "CFBundleIdentifier": "io.github.alpenl.iris",
                          "CFBundleName": "Iris", "CFBundlePackageType": "APPL", "CFBundleShortVersionString": args.version,
                          "CFBundleVersion": args.version, "LSMinimumSystemVersion": "11.0", "NSHighResolutionCapable": True,
                          "LSApplicationCategoryType": "public.app-category.video"}, file)
    if not executable.exists():
        shutil.copy2(built, executable)
    elif sha(executable) != state["build"]:
        raise RuntimeError("Staged executable changed; refusing resume")
    notices = payload / "compliance" if windows else payload / "Contents/Resources/compliance"
    shutil.copytree(compliance, notices, dirs_exist_ok=True, ignore=shutil.ignore_patterns(".DS_Store"))
    if args.source_mode == "sidecar":
        (notices / "corresponding-source.tar.gz").unlink(missing_ok=True)
    # Fonts and SVG icons are compiled into the binary via include_bytes!;
    # ship only their OFL license notices to avoid duplicating embedded assets.
    font_licenses = notices / "font-licenses"
    font_licenses.mkdir(exist_ok=True)
    for ofl in sorted((ROOT / "Resources/Fonts").glob("*-OFL.txt")):
        shutil.copy2(ofl, font_licenses / ofl.name)
    archive_dir = notices if args.source_mode == "embedded" else output
    if args.reuse_rust and not state.get("rust_notices"):
        prior = args.reuse_rust.resolve()
        previous = json.loads((prior / "package-checkpoint.json").read_text())
        for name, digest in identity["application"].items():
            if name.endswith("Cargo.toml") or name in ("Cargo.lock", "deny.toml"):
                if previous["identity"]["application"].get(name) != digest:
                    parser.error("Rust dependency/license inputs changed; cannot reuse Rust artifacts")
        old_notices = prior / ("stage/Iris/compliance" if windows else "stage/Iris.app/Contents/Resources/compliance")
        archive = old_notices / "rust-corresponding-source.tar.gz"
        if not archive.is_file():
            archive = prior / "rust-corresponding-source.tar.gz"
        if not previous.get("rust_notices") or not archive.is_file() or sha(archive) != previous.get("rust_source"):
            parser.error("Prior Rust source artifacts lack a valid completion checkpoint")
        verify_vendor_archive(archive)
        shutil.copy2(archive, archive_dir / "rust-corresponding-source.tar.gz")
        for name in ("rust-license-manifest.json", "cargo-vendor-config.toml"):
            shutil.copy2(old_notices / name, notices / name)
        shutil.copytree(old_notices / "rust-licenses", notices / "rust-licenses", dirs_exist_ok=True)
        state.update(rust_notices=True, rust_source=sha(archive))
        save_checkpoint(checkpoint, state)
    rust_notices(notices, env, checkpoint, state)
    if not state.get("iris_source"):
        application_source(notices)
    for directory, archive, flag in (("rust-vendor", "rust-corresponding-source.tar.gz", "rust_source"),
                                     ("iris-source", "iris-corresponding-source.tar.gz", "iris_source")):
        target_archive = archive_dir / archive
        if not state.get(flag):
            _media.source_archive(notices / directory, target_archive, manifest["source_date_epoch"])
            state[flag] = sha(target_archive)
            save_checkpoint(checkpoint, state)
        elif not target_archive.is_file() or sha(target_archive) != state[flag]:
            raise RuntimeError("Completed corresponding source archive changed: " + archive)
        if (notices / directory).exists():
            shutil.rmtree(notices / directory)
    graph = windows_dependencies(executable, prefix, manifest, frameworks) if windows else mac_dependencies(executable, prefix, manifest, frameworks, manifest["target"].removeprefix("macos-"))
    state = {"mode": args.mode, "signing": "unsigned-local", "notarization": "not-requested", "target": target, "version": args.version, "native_dependencies": graph}
    if windows and args.mode == "release":
        for file in [executable, *sorted(frameworks.glob("*.dll"))]:
            run(["pwsh", "-NoProfile", "-File", Path(__file__).with_name("sign-windows.ps1"), "-File", file])
        state["signing"] = "authenticode-verified"
    if not windows:
        identity = os.environ["IRIS_MAC_SIGN_IDENTITY"] if args.mode == "release" else "-"
        for file in sorted(frameworks.glob("*.dylib")):
            run(["codesign", "--force", "--sign", identity, *( ["--timestamp", "--options", "runtime"] if args.mode == "release" else []), file])
            run(["codesign", "--verify", "--strict", file])
        state["signing"] = "developer-id-hardened-runtime" if args.mode == "release" else "ad-hoc-local-not-developer-id"
    # Manifest must exist before signing the app; do not mutate signed resources afterward.
    (notices / "package-manifest.json").write_text(json.dumps(state, indent=2) + "\n")
    artifact = "Iris-" + args.version + "-" + manifest["target"] + "-" + args.mode
    if windows:
        iss = Path(__file__).with_name("windows-installer.iss")
        iscc = shutil.which("ISCC")
        if not iscc:
            installed = Path(os.environ["LOCALAPPDATA"]) / "Programs/Inno Setup 6/ISCC.exe"
            if not installed.is_file():
                raise RuntimeError("Inno Setup compiler required; run scripts/release/install-inno.ps1")
            iscc = str(installed)
        installer = output / (artifact + "-setup.exe")
        completed = json.loads(checkpoint.read_text())
        if completed.get("installer"):
            if not installer.is_file() or sha(installer) != completed["installer"]:
                raise RuntimeError("Completed installer changed or disappeared")
        else:
            run([iscc, "/DAppVersion=" + args.version, "/DPayload=" + str(payload), "/DArtifact=" + artifact,
                 "/DOutput=" + str(output), "/DLicenseFile=" + str(ROOT / "LICENSE-MIT"), iss])
            completed["installer"] = sha(installer)
            save_checkpoint(checkpoint, completed)
        if args.mode == "release":
            run(["pwsh", "-NoProfile", "-File", Path(__file__).with_name("sign-windows.ps1"), "-File", installer])
        with zipfile.ZipFile(output / (artifact + ".zip"), "w", zipfile.ZIP_DEFLATED, strict_timestamps=False) as zip:
            for file in sorted(payload.rglob("*")):
                if file.is_file():
                    zip.write(file, file.relative_to(stage))
    else:
        identity = os.environ["IRIS_MAC_SIGN_IDENTITY"] if args.mode == "release" else "-"
        run(["codesign", "--force", "--sign", identity, *(["--timestamp", "--options", "runtime"] if args.mode == "release" else []), payload])
        run(["codesign", "--verify", "--deep", "--strict", payload])
        if args.mode == "release":
            details = run(["codesign", "--display", "--verbose=4", payload])
            if "Authority=Developer ID Application:" not in details or "runtime" not in details:
                raise RuntimeError("Developer ID hardened runtime verification failed")
            submission = output / "notary-submission.zip"
            run(["ditto", "-c", "-k", "--keepParent", payload, submission])
            result = json.loads(run(["xcrun", "notarytool", "submit", submission, "--keychain-profile", os.environ["IRIS_NOTARY_PROFILE"], "--wait", "--output-format", "json"]))
            (output / "notary-result.json").write_text(json.dumps(result, indent=2))
            if result.get("status") != "Accepted":
                raise RuntimeError("Apple notarization did not accept this application")
            run(["xcrun", "stapler", "staple", payload])
            run(["xcrun", "stapler", "validate", payload])
            run(["spctl", "--assess", "--type", "execute", "--verbose=2", payload])
            state["notarization"] = "accepted-and-stapled"
            submission.unlink()
        run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", payload, output / (artifact + ".zip")])
        run(["hdiutil", "create", "-volname", "Iris", "-srcfolder", stage, "-ov", "-format", "UDZO", output / (artifact + ".dmg")])
        if args.mode == "release":
            run(["codesign", "--sign", os.environ["IRIS_MAC_SIGN_IDENTITY"], "--timestamp", output / (artifact + ".dmg")])
            result = json.loads(run(["xcrun", "notarytool", "submit", output / (artifact + ".dmg"), "--keychain-profile", os.environ["IRIS_NOTARY_PROFILE"], "--wait", "--output-format", "json"]))
            (output / "notary-dmg-result.json").write_text(json.dumps(result, indent=2))
            if result.get("status") != "Accepted":
                raise RuntimeError("Apple notarization did not accept the distribution disk image")
            run(["xcrun", "stapler", "staple", output / (artifact + ".dmg")])
            run(["xcrun", "stapler", "validate", output / (artifact + ".dmg")])
    # Sidecars accompany every distribution, including the installable package.
    for suffix in (".zip", "-setup.exe" if windows else ".dmg"):
        binary = output / (artifact + suffix)
        if not binary.is_file():
            raise RuntimeError("Expected distribution missing: " + str(binary))
        shutil.copy2(compliance / "corresponding-source.tar.gz", output / (binary.name + ".corresponding-source.tar.gz"))
        if args.source_mode == "sidecar":
            for extra in ("rust-corresponding-source.tar.gz", "iris-corresponding-source.tar.gz"):
                shutil.copy2(output / extra, output / (binary.name + "." + extra))
        bundle = {**state, "binary_sha256": sha(binary), "media_build": manifest}
        (output / (binary.name + ".build-manifest.json")).write_text(json.dumps(bundle, indent=2) + "\n")
        with tarfile.open(output / (binary.name + ".licenses.tar.gz"), "w:gz") as tar:
            tar.add(notices, arcname="compliance")
    distributed = sorted(p for p in output.glob(artifact + "*") if p.is_file())
    (output / "SHA256SUMS").write_text("".join(sha(p) + "  " + p.name + "\n" for p in distributed))
    # Keep Cargo's incremental cache, especially when supplied by the caller.
    print("Packaged:", output, "signing:", state["signing"], "notarization:", state["notarization"])


if __name__ == "__main__":
    main()
