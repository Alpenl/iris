#!/usr/bin/env python3
"""Build Iris against the verified media SDK and package without system media fallbacks.

Use --mode local for unsigned distributions. For a signed release, run
--prepare-only before provisioning signing credentials. Run --resume
--package-only with the same inputs after provisioning credentials.
Windows requires MSVC, the Windows SDK, and Inno Setup 6 (ISCC in PATH).
Signing prefers PowerShell 7 and falls back to inbox Windows PowerShell.
macOS bundles resolve relocated SDK dylibs through the verified manifest.
Rust compliance requires cargo-deny 0.22.0 and the workspace Cargo.lock.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import plistlib
import posixpath
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[2]
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parent))
from importlib.util import spec_from_file_location, module_from_spec
_spec = spec_from_file_location("media_recipe", Path(__file__).with_name("build-media.py"))
_media = module_from_spec(_spec)
_spec.loader.exec_module(_media)
run = _media.run

SIGNING_ENV = ("IRIS_WINDOWS_PFX", "IRIS_WINDOWS_PFX_PASSWORD", "IRIS_TIMESTAMP_URL",
               "IRIS_MAC_SIGN_IDENTITY", "IRIS_NOTARY_PROFILE", "IRIS_NOTARY_KEYCHAIN")

# Versions verified to ship an SPDX declaration without a standalone notice file.
# Keep the corresponding checked Cargo.toml in their notice directory.
METADATA_LICENSES = frozenset("""
block@0.1.6 convert_case@0.4.0 dtor@0.0.6 dtor-proc-macro@0.0.5 dwrote@0.11.5 gloo-timers@0.3.0
hexf-parse@0.2.1 leak@0.1.2 leaky-cow@0.1.1 lyon@1.0.19 lyon_algorithms@1.0.22 lyon_geom@1.0.20
lyon_path@1.0.19 lyon_tessellation@1.0.23 mac@0.1.1 malloc_buf@0.0.6 naga@25.0.1 pathfinder_geometry@0.5.1
pathfinder_simd@0.5.6 profiling@1.0.18 profiling-procmacros@1.0.18 r-efi@5.3.0 r-efi@6.0.0 seahash@4.1.0
simd_helpers@0.1.0 stacksafe@0.1.4 stacksafe-macro@0.1.4 svg_fmt@0.4.5 taffy@0.9.0 winapi-i686-pc-windows-gnu@0.4.0
winapi-x86_64-pc-windows-gnu@0.4.0 zune-inflate@0.2.54
""".split())


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
        for directory in ("crates", "Resources", "scripts/release")
        for p in (ROOT / directory).rglob("*")
        if p.is_file() and p.name != ".DS_Store" and "__pycache__" not in p.parts and p.suffix != ".pyc"
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
    """Read PE32+ imports using only ranges backed by file data."""
    data = path.read_bytes()

    def malformed():
        raise RuntimeError(f"Malformed Windows x64 PE: {path}")

    if len(data) < 64 or data[:2] != b"MZ":
        malformed()
    pe = struct.unpack_from("<I", data, 0x3c)[0]
    if pe + 24 > len(data) or data[pe:pe + 4] != b"PE\0\0":
        malformed()
    machine, count = struct.unpack_from("<HH", data, pe + 4)
    size = struct.unpack_from("<H", data, pe + 20)[0]
    optional = pe + 24
    table = optional + size
    if machine != 0x8664 or size < 112 or table + count * 40 > len(data):
        malformed()
    if struct.unpack_from("<H", data, optional)[0] != 0x20b:
        malformed()
    directory_count = struct.unpack_from("<I", data, optional + 108)[0]
    if directory_count > (size - 112) // 8:
        malformed()
    sections = [struct.unpack_from("<IIII", data, table + i * 40 + 8) for i in range(count)]

    def file_span(rva, length):
        for virtual_size, virtual_address, raw_size, raw_offset in sections:
            delta = rva - virtual_address
            if 0 <= delta < max(virtual_size, raw_size):
                limit = min(raw_offset + raw_size, len(data))
                start = raw_offset + delta
                if not raw_offset or start + length > limit:
                    malformed()
                return start, limit
        malformed()

    names = []
    for index, width in ((1, 20), (13, 32)):
        if index >= directory_count:
            continue
        rva, length = struct.unpack_from("<II", data, optional + 112 + index * 8)
        if not rva and not length:
            continue
        if not rva or length < width:
            malformed()
        start, _ = file_span(rva, length)
        for pos in range(start, start + length - width + 1, width):
            values = struct.unpack_from("<" + "I" * (width // 4), data, pos)
            if not any(values):
                break
            if index == 13 and not values[0] & 1:
                raise RuntimeError("Unsupported VA-based delay import")
            name_rva = values[3] if index == 1 else values[1]
            name_start, end = file_span(name_rva, 1)
            nul = data.find(b"\0", name_start, end)
            if nul <= name_start or not data[name_start:nul].isascii():
                malformed()
            names.append(data[name_start:nul].decode("ascii"))
        else:
            malformed()
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


def _mac_sdk_dylibs(prefix, files, arch):
    """Index pinned paths and install IDs without consulting import paths on disk."""
    paths, identities = {}, {}
    origins = set()
    by_id, by_name = {}, {}
    for name, digest in files.items():
        relative = PurePosixPath(name)
        if relative.suffix != ".dylib":
            continue
        if relative.is_absolute() or ".." in relative.parts:
            raise RuntimeError(f"Invalid SDK dylib path: {name}")
        source = (prefix / name).resolve(strict=True)
        if not source.is_relative_to(prefix) or files.get(source.relative_to(prefix).as_posix()) != digest:
            raise RuntimeError(f"Unverified media dylib: {prefix / name}")
        if source not in identities:
            if not source.is_file() or sha(source) != digest:
                raise RuntimeError(f"Unverified media dylib: {source}")
            output = run(["otool", "-arch", arch, "-D", source]).splitlines()[1:]
            if len(output) != 1 or not output[0].strip():
                raise RuntimeError(f"Missing dylib install ID: {source}")
            identity = output[0].strip()
            identities[source] = identity
            by_id.setdefault(identity, set()).add(source)
        paths[relative] = source
        by_name.setdefault(relative.name, set()).add(source)
        identity_path = PurePosixPath(identities[source])
        # An ID ending in a known path in this SDK identifies its original prefix.
        # This also lets other absolute imports name pinned aliases under that prefix.
        if identity_path.is_absolute() and identity_path.parts[-len(relative.parts):] == relative.parts:
            origin = identity_path.parents[len(relative.parts) - 1]
            if origin != PurePosixPath("/"):
                origins.add(origin)
    return paths, by_id, by_name, origins, identities


def mac_dependencies(executable, prefix, manifest, frameworks, arch):
    paths, by_id, by_name, origins, identities = _mac_sdk_dylibs(prefix, manifest["files"], arch)
    current = PurePosixPath(prefix.as_posix())

    def resolve(dep, path, loader):
        candidates = set(by_id.get(dep, ()))
        if path.is_absolute():
            if path.is_relative_to(current):
                candidates.add(paths.get(path.relative_to(current)))
            else:
                for origin in origins:
                    if path.is_relative_to(origin):
                        candidates.add(paths.get(path.relative_to(origin)))
        else:
            marker, _, tail = dep.partition("/")
            if marker == "@loader_path":
                directory = loader.relative_to(prefix).parent.as_posix() if loader else "lib"
                candidates.add(paths.get(PurePosixPath(posixpath.normpath(posixpath.join(directory, tail)))))
            elif marker == "@rpath" or dep == path.name:
                relative = PurePosixPath(tail if marker == "@rpath" else dep)
                candidates.add(paths.get(PurePosixPath("lib") / relative))
                candidates.add(paths.get(relative))
                candidates.discard(None)
                if not candidates and len(relative.parts) == 1:
                    candidates.update(by_name.get(relative.name, ()))
        candidates.discard(None)
        if not candidates or path.suffix != ".dylib":
            raise RuntimeError("External dylib outside pinned SDK: " + dep)
        if len(candidates) != 1:
            raise RuntimeError("Ambiguous pinned SDK dylib: " + dep)
        return next(iter(candidates))

    bundled = {}
    queue = [(executable, None)]
    graph = {}
    while queue:
        binary, source = queue.pop()
        archs = run(["lipo", "-archs", binary]).strip().split()
        if arch not in archs:
            raise RuntimeError(f"Wrong architecture: {binary}: {archs}")
        deps = [line.strip().rsplit(" (compatibility version ", 1)[0]
                for line in run(["otool", "-arch", arch, "-L", binary]).splitlines()[1:]]
        graph[binary.name] = deps
        identity = identities.get(source)
        if identity:
            run(["install_name_tool", "-id", "@rpath/" + binary.name, binary])
        for dep in dict.fromkeys(deps):
            if dep == identity:
                continue
            path = PurePosixPath(posixpath.normpath(dep))
            if dep.startswith(("/opt/homebrew/", "/usr/local/", "/opt/local/")) or path.as_posix().startswith(
                    ("/opt/homebrew/", "/usr/local/", "/opt/local/")):
                raise RuntimeError("Homebrew/MacPorts media forbidden: " + dep)
            if path.as_posix().startswith(("/System/Library/", "/usr/lib/")):
                continue
            dependency = resolve(dep, path, source)
            target = frameworks / path.name
            if target not in bundled:
                shutil.copy2(dependency, target)
                bundled[target] = dependency
                queue.append((target, dependency))
            elif bundled[target] != dependency:
                raise RuntimeError(f"Conflicting dylibs for bundle path: {target.name}")
            run(["install_name_tool", "-change", dep, "@rpath/" + target.name, binary])
        # Remove every build-machine rpath, including implicit Cargo/build.rs paths.
        loads = run(["otool", "-arch", arch, "-l", binary])
        for rpath in re.findall(r"cmd LC_RPATH\s+cmdsize \d+\s+path (.*?) \(offset", loads):
            run(["install_name_tool", "-delete_rpath", rpath, binary])
        run(["install_name_tool", "-add_rpath", "@executable_path/../Frameworks" if binary == executable else "@loader_path", binary])
    return graph


def rust_notices(destination, env, checkpoint, state):
    if state.get("rust_notices"):
        if not (destination / "rust-license-manifest.json").is_file():
            raise RuntimeError("Completed Rust notices artifact missing")
        return
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
            if custom.is_absolute() or ".." in custom.parts:
                raise RuntimeError(f"Declared license must be within the vendored crate: {directory}: {custom}")
            original = directory / custom
            if custom.as_posix() not in checksums["files"] or not original.is_file():
                raise RuntimeError(f"Declared license missing from verified vendor files: {original}")
            if str(custom) not in files:
                target = notices / directory.name / custom
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(original, target)
                files.append(str(custom))
        upstream_notice = bool(files)
        if not files:
            crate = f"{package['name']}@{package['version']}"
            if crate not in METADATA_LICENSES or not package["license"]:
                raise RuntimeError(f"No upstream license notice in vendored crate: {crate}")
            # Preserve the actual published declaration when this version contains no notice file.
            target = notices / directory.name / "Cargo.toml"
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(directory / "Cargo.toml", target)
            files.append("Cargo.toml")
        entries.append({"name": package["name"], "version": package["version"], "license": package["license"],
                        "license_file": package["license_file"], "notices": sorted(files),
                        "notice_basis": "upstream-files" if upstream_notice else "published-SPDX-declaration"})
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
    for name in ("LICENSE-MIT", "LICENSE-APACHE"):
        shutil.copy2(ROOT / name, destination / name)


def tree_hashes(folder):
    return {p.relative_to(folder).as_posix(): sha(p) for p in sorted(folder.rglob("*")) if p.is_file()}


def completed_file(state, file):
    digest = state.get("distributions", {}).get(file.name)
    if digest and (not file.is_file() or sha(file) != digest):
        raise RuntimeError(f"Completed distribution changed or disappeared: {file}")
    return bool(digest)


def record_file(state, checkpoint, file):
    state.setdefault("distributions", {})[file.name] = sha(file)
    save_checkpoint(checkpoint, state)


def reuse_rust(prior, state, notices, archive_dir):
    previous = json.loads((prior / "package-checkpoint.json").read_text())
    for name, digest in state["identity"]["application"].items():
        if name.endswith("Cargo.toml") or name in ("Cargo.lock", "deny.toml", "scripts/release/package.py"):
            if previous["identity"]["application"].get(name) != digest:
                raise RuntimeError("Rust dependency or policy inputs changed; cannot reuse Rust artifacts")
    unsigned = prior / "unsigned"
    if not previous.get("prepared") or tree_hashes(unsigned) != previous["prepared"]:
        raise RuntimeError("Prior Rust artifacts lack a valid preparation checkpoint")
    archive = unsigned / "source/rust-corresponding-source.tar.gz"
    verify_vendor_archive(archive)
    shutil.copy2(archive, archive_dir / archive.name)
    old_notices = unsigned / "compliance"
    for name in ("rust-license-manifest.json", "cargo-vendor-config.toml", "LICENSE-MIT", "LICENSE-APACHE"):
        shutil.copy2(old_notices / name, notices / name)
    shutil.copytree(old_notices / "rust-licenses", notices / "rust-licenses", dirs_exist_ok=True)
    state.update(rust_notices=True, rust_source=sha(archive))


def stage_payload(payload, built, windows, version):
    if windows:
        payload.mkdir(parents=True)
        executable = payload / "iris.exe"
        frameworks = payload
    else:
        contents = payload / "Contents"
        (contents / "MacOS").mkdir(parents=True)
        frameworks = contents / "Frameworks"
        frameworks.mkdir()
        executable = contents / "MacOS/iris"
        with (contents / "Info.plist").open("wb") as file:
            plistlib.dump({"CFBundleExecutable": "iris", "CFBundleIdentifier": "io.github.alpenl.iris",
                           "CFBundleName": "Iris", "CFBundlePackageType": "APPL", "CFBundleShortVersionString": version,
                           "CFBundleVersion": version, "LSMinimumSystemVersion": "11.0", "NSHighResolutionCapable": True,
                           "LSApplicationCategoryType": "public.app-category.video",
                           "CFBundleDocumentTypes": [{"CFBundleTypeName": "Media", "CFBundleTypeRole": "Viewer",
                               "LSHandlerRank": "Alternate", "LSItemContentTypes": ["public.movie", "public.audio", "public.image"]}]}, file)
    shutil.copy2(built, executable)
    return executable, frameworks


def notarize(submission, target, evidence):
    profile = ["--keychain-profile", os.environ["IRIS_NOTARY_PROFILE"]]
    if os.environ.get("IRIS_NOTARY_KEYCHAIN"):
        profile += ["--keychain", os.environ["IRIS_NOTARY_KEYCHAIN"]]
    result = json.loads(run(["xcrun", "notarytool", "submit", submission, *profile, "--wait", "--output-format", "json"]))
    evidence.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    if result.get("status") != "Accepted":
        raise RuntimeError(f"Apple notarization did not accept {target}")
    run(["xcrun", "stapler", "staple", target])
    run(["xcrun", "stapler", "validate", target])

def _windows_signer():
    host = shutil.which("pwsh.exe")
    if host:
        host = Path(host).resolve()
    else:
        system_root = os.environ.get("SystemRoot")
        host = Path(system_root) / "System32/WindowsPowerShell/v1.0/powershell.exe" if system_root else None
        if host is None or not host.is_file():
            raise RuntimeError("Windows signing requires pwsh.exe or inbox Windows PowerShell")
    script = Path(__file__).resolve().with_name("sign-windows.ps1")
    return [host, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", script, "-File"]


def sign_payload(payload, executable, frameworks, notices, graph, target, mode, version, windows, output):
    release = mode == "release"
    record = {"mode": mode, "signing": "unsigned-local", "notarization": "not-requested",
              "target": target, "version": version, "native_dependencies": graph}
    if windows and release:
        signer = _windows_signer()
        for file in [executable, *sorted(frameworks.glob("*.dll"))]:
            run([*signer, file])
        record["signing"] = "authenticode-verified"
    if not windows:
        signing = ["--timestamp", "--options", "runtime"] if release else []
        identity = os.environ["IRIS_MAC_SIGN_IDENTITY"] if release else "-"
        for file in sorted(frameworks.glob("*.dylib")):
            run(["codesign", "--force", "--sign", identity, *signing, file])
            run(["codesign", "--verify", "--strict", file])
        record["signing"] = "developer-id-hardened-runtime" if release else "ad-hoc-local-not-developer-id"
        if release:
            # The sealed resource records the release requirement. The external manifest records the outcome.
            record["notarization"] = "required; see accompanying build manifest for result"
    (notices / "package-manifest.json").write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    if not windows:
        run(["codesign", "--force", "--sign", identity, *signing, payload])
        run(["codesign", "--verify", "--deep", "--strict", payload])
        if release:
            details = run(["codesign", "--display", "--verbose=4", payload])
            if "Authority=Developer ID Application:" not in details or "runtime" not in details:
                raise RuntimeError("Developer ID hardened runtime verification failed")
            submission = output / "notary-submission.zip"
            submission.unlink(missing_ok=True)
            run(["ditto", "-c", "-k", "--keepParent", payload, submission])
            notarize(submission, payload, output / "notary-result.json")
            run(["spctl", "--assess", "--type", "execute", "--verbose=2", payload])
            submission.unlink()
            record["notarization"] = "accepted-and-stapled"
    return record


def build_installer(payload, output, artifact, version, release):
    iscc = shutil.which("ISCC")
    if not iscc:
        installed = Path(os.environ["LOCALAPPDATA"]) / "Programs/Inno Setup 6/ISCC.exe"
        if not installed.is_file():
            raise RuntimeError("Inno Setup compiler required; run scripts/release/install-inno.ps1")
        iscc = installed
    args = [iscc, "/DAppVersion=" + version, "/DPayload=" + str(payload), "/DArtifact=" + artifact,
            "/DOutput=" + str(output), "/DLicenseFile=" + str(ROOT / "LICENSE-MIT")]
    if release:
        # Use a clean cache so ISCC cannot reuse an uninstaller signed by some previous release.
        cache = output / "signed-uninstaller"
        if cache.exists():
            shutil.rmtree(cache)
        cache.mkdir()
        # Use Inno's quote and dollar macros after Windows argument quoting.
        signer = subprocess.list2cmdline(_windows_signer()).replace("$", "$$").replace('"', "$q")
        args += ["/DSignedRelease", "/DSignedUninstallerDir=" + str(cache),
                 f"/Siris={signer} $f"]
    run([*args, Path(__file__).with_name("windows-installer.iss")])
    if release:
        run(["signtool", "verify", "/pa", "/all", output / (artifact + "-setup.exe")])


def media_source(compliance, manifest):
    lock = json.loads(_media.LOCK.read_text(encoding="utf-8"))
    _media.validate_lock(lock)
    sdk_lock = json.loads((compliance / _media.LOCK.name).read_text(encoding="utf-8"))
    if sdk_lock != lock or any(lock["sources"].get(name) != pin for name, pin in manifest["sources"].items()):
        raise RuntimeError("Media SDK sources differ from the current verified source lock")
    if not {"ffmpeg", "mpv"} <= manifest["sources"].keys():
        raise RuntimeError("Media SDK manifest omits runtime source pins")
    return compliance / "corresponding-source.tar.gz"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--version", required=True, help="Must match the authorized 0.0.x workspace version")
    parser.add_argument("--mode", choices=["local", "release"], required=True)
    parser.add_argument("--resume", action="store_true", help="Resume only when application and SDK inputs are unchanged")
    phases = parser.add_mutually_exclusive_group()
    phases.add_argument("--prepare-only", action="store_true", help="Compile and collect unsigned release inputs without signing credentials")
    phases.add_argument("--package-only", action="store_true", help="Package completed preparation without invoking Cargo; requires --resume")
    parser.add_argument("--reuse-rust", type=Path, help="Reuse verified Rust source/license artifacts from a prior package output")
    parser.add_argument(
        "--source-mode",
        choices=["embedded", "sidecar"],
        default="sidecar",
        help="sidecar (default) ships source archives alongside the installers; embedded includes them in compliance/"
    )
    args = parser.parse_args()
    if args.mode == "release" and not (args.prepare_only or args.package_only):
        parser.error("Release requires --prepare-only followed by --resume --package-only")
    with (ROOT / "Cargo.toml").open("rb") as file:
        workspace_version = tomllib.load(file)["workspace"]["package"]["version"]
    if not re.fullmatch(r"0\.0\.(0|[1-9][0-9]*)", args.version) or args.version != workspace_version:
        parser.error(f"only the current 0.0.x workspace version ({workspace_version}) is authorized")
    sdk, output = args.sdk.resolve(), args.output.resolve()
    compliance = sdk / "compliance"
    manifest = json.loads((compliance / "build-manifest.json").read_text())
    media_archive = media_source(compliance, manifest)
    media_digest = sha(media_archive)
    prefix = sdk / "prefix"
    windows = manifest["target"] == "windows-x86_64"
    if (windows and os.name != "nt") or (not windows and sys.platform != "darwin"):
        parser.error("Package on the target OS (Windows MSVC or native macOS)")
    if output.exists() and not args.resume:
        parser.error("Use a fresh output directory or --resume")
    checkpoint = output / "package-checkpoint.json"
    identity = {"application": application_inputs(), "sdk_manifest": sha(compliance / "build-manifest.json"),
                "mode": args.mode, "version": args.version, "target": manifest["target"]}
    identity["source_mode"] = args.source_mode
    identity["media_source"] = media_digest
    state = {"identity": identity}
    if checkpoint.exists():
        state = json.loads(checkpoint.read_text())
        if state["identity"] != identity:
            parser.error("Application, SDK, mode or version changed; refusing stale package resume")
    elif args.resume:
        parser.error("No package checkpoint exists")
    if args.package_only and not (state.get("prepared") and state.get("build") and state.get("license_gate")):
        parser.error("--package-only requires --resume and completed preparation")
    # Verify the entire SDK before compiling, including import libraries and headers.
    for relative, digest in manifest["files"].items():
        path = Path(relative)
        file = prefix / path
        if not windows and (path.is_absolute() or ".." in path.parts or not file.resolve().is_relative_to(prefix)):
            raise RuntimeError("SDK path outside pinned prefix: " + relative)
        if not file.is_file() or sha(file) != digest:
            raise RuntimeError("SDK checksum mismatch: " + relative)
    if manifest["ffmpeg"] != {"gpl": False, "version3": False, "nonfree": False, "chromaprint": False, "shared": True}:
        raise RuntimeError("Media manifest license gate rejected")
    for key, value in (("gpl", False), ("libmpv", True), ("cplayer", False)):
        if manifest["mpv"].get(key) != value:
            raise RuntimeError("mpv manifest license gate rejected")
    output.mkdir(parents=True, exist_ok=args.resume)
    if not args.resume:
        save_checkpoint(checkpoint, state)
    runtime_evidence = output / "media-runtime-verification.json"
    if not state.get("runtime_verification") and not args.package_only:
        verification_env = {key: value for key, value in os.environ.items() if key not in SIGNING_ENV}
        if not windows:
            verification_env["DYLD_LIBRARY_PATH"] = str(prefix / "lib")
        verification = run([sys.executable, Path(__file__).with_name("verify-media.py"), "--sdk", sdk], env=verification_env)
        runtime_evidence.write_text(verification, encoding="utf-8")
        state["runtime_verification"] = sha(runtime_evidence)
        save_checkpoint(checkpoint, state)
    elif not runtime_evidence.is_file() or sha(runtime_evidence) != state.get("runtime_verification"):
        raise RuntimeError("Completed media runtime verification changed or disappeared")
    env = {key: value for key, value in os.environ.items() if key not in SIGNING_ENV}
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
    built = output / "unsigned" / ("iris.exe" if windows else "iris")
    if not state.get("build"):
        run(["cargo", "build", "--locked", "--release", "--target", target, "-p", "iris"], ROOT, env)
        built.parent.mkdir(exist_ok=True)
        shutil.copy2(cargo_target / target / "release" / built.name, built)
        state["build"] = sha(built)
        save_checkpoint(checkpoint, state)
    elif not built.is_file() or sha(built) != state["build"]:
        raise RuntimeError("Completed unsigned application changed or disappeared")
    prepared = output / "unsigned"
    notices = prepared / "compliance"
    archive_dir = prepared / "source"
    if not state.get("prepared"):
        notices.mkdir(exist_ok=True)
        archive_dir.mkdir(exist_ok=True)
        shutil.copytree(compliance, notices, dirs_exist_ok=True,
                        ignore=shutil.ignore_patterns(".DS_Store", "corresponding-source.tar.gz"))
        font_licenses = notices / "font-licenses"
        font_licenses.mkdir(exist_ok=True)
        for ofl in sorted((ROOT / "Resources/Fonts").glob("*-OFL.txt")):
            shutil.copy2(ofl, font_licenses / ofl.name)
        if not state.get("license_gate"):
            run(["cargo", "deny", "--locked", "check", "advisories", "licenses", "bans", "sources"], ROOT, env)
            state["license_gate"] = True
            save_checkpoint(checkpoint, state)
        if args.reuse_rust and not state.get("rust_notices"):
            reuse_rust(args.reuse_rust.resolve(), state, notices, archive_dir)
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
        state["prepared"] = tree_hashes(prepared)
        save_checkpoint(checkpoint, state)
    elif tree_hashes(prepared) != state["prepared"]:
        raise RuntimeError("Completed unsigned package inputs changed or disappeared")
    if args.prepare_only:
        return

    release = args.mode == "release"
    if release:
        required = SIGNING_ENV[:3] if windows else SIGNING_ENV[3:5]
        for name in required:
            if not os.environ.get(name):
                raise RuntimeError("Signed release requires " + name)
    artifact = f"Iris-{args.version}-{manifest['target']}-{args.mode}"
    stage = output / "stage"
    payload = stage / ("Iris" if windows else "Iris.app")
    packaged_notices = payload / ("compliance" if windows else "Contents/Resources/compliance")
    sources = {file.name: file for file in sorted(archive_dir.glob("*.tar.gz"))}
    sources[media_archive.name] = media_archive
    source_names = {name: f"{artifact}.{name}" for name in sources}
    if state.get("payload"):
        if tree_hashes(stage) != state["payload"]:
            raise RuntimeError("Completed signed package payload changed or disappeared")
        package = state["manifest"]
        if not windows:
            run(["codesign", "--verify", "--deep", "--strict", payload])
            if release:
                run(["xcrun", "stapler", "validate", payload])
    else:
        if stage.exists():
            shutil.rmtree(stage)
        executable, frameworks = stage_payload(payload, built, windows, args.version)
        shutil.copytree(notices, packaged_notices)
        if args.source_mode == "embedded":
            for name, file in sources.items():
                shutil.copy2(file, packaged_notices / name)
        else:
            (packaged_notices / "CORRESPONDING-SOURCE.txt").write_text(
                "Download these exact source archives alongside the Iris distribution:\n"
                + "".join(f"{sha(sources[name])}  {filename}\n" for name, filename in sorted(source_names.items())),
                encoding="utf-8",
            )
        graph = windows_dependencies(executable, prefix, manifest, frameworks) if windows else mac_dependencies(
            executable, prefix, manifest, frameworks, manifest["target"].removeprefix("macos-"))
        package = sign_payload(payload, executable, frameworks, packaged_notices, graph,
                               target, args.mode, args.version, windows, output)
        state.update(payload=tree_hashes(stage), manifest=package)
        save_checkpoint(checkpoint, state)
    zip_path = output / (artifact + ".zip")
    if not completed_file(state, zip_path):
        zip_path.unlink(missing_ok=True)
        if windows:
            with zipfile.ZipFile(zip_path, "w", zipfile.ZIP_DEFLATED, strict_timestamps=False) as archive:
                for file in sorted(payload.rglob("*")):
                    if file.is_file():
                        archive.write(file, file.relative_to(stage))
        else:
            run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", payload, zip_path])
        record_file(state, checkpoint, zip_path)

    setup = output / (artifact + ("-setup.exe" if windows else ".dmg"))
    if not completed_file(state, setup):
        setup.unlink(missing_ok=True)
        if windows:
            build_installer(payload, output, artifact, args.version, release)
        else:
            run(["hdiutil", "create", "-volname", "Iris", "-srcfolder", stage, "-ov", "-format", "UDZO", setup])
            if release:
                run(["codesign", "--sign", os.environ["IRIS_MAC_SIGN_IDENTITY"], "--timestamp", setup])
                run(["codesign", "--verify", "--strict", setup])
                notarize(setup, setup, output / "notary-dmg-result.json")
        record_file(state, checkpoint, setup)
    elif release:
        if windows:
            run(["signtool", "verify", "/pa", "/all", setup])
        else:
            run(["codesign", "--verify", "--strict", setup])
            run(["xcrun", "stapler", "validate", setup])

    if args.source_mode == "sidecar":
        for name, file in sources.items():
            shutil.copy2(file, output / source_names[name])
    licenses = output / (artifact + ".licenses.tar.gz")
    if not completed_file(state, licenses):
        with tarfile.open(licenses, "w:gz") as archive:
            archive.add(packaged_notices, arcname="compliance",
                        filter=lambda entry: None if entry.name.removeprefix("compliance/") in sources else entry)
        record_file(state, checkpoint, licenses)
    for binary in (zip_path, setup):
        bundle = {**package, "binary_sha256": state["distributions"][binary.name], "media_build": manifest,
                  "corresponding_source": source_names if args.source_mode == "sidecar" else "embedded"}
        if release and not windows and binary == setup:
            bundle["disk_image_notarization"] = "accepted-and-stapled"
        (output / (binary.name + ".build-manifest.json")).write_text(json.dumps(bundle, indent=2) + "\n")
    distributed = sorted(p for p in output.glob(artifact + "*") if p.is_file())
    (output / "SHA256SUMS").write_text("".join(f"{sha(p)}  {p.name}\n" for p in distributed))
    print("Packaged:", output, "signing:", package["signing"], "notarization:", package["notarization"])


if __name__ == "__main__":
    main()
