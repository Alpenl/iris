#!/usr/bin/env python3
import contextlib
import hashlib
from importlib.util import module_from_spec, spec_from_file_location
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import types
import unittest
from unittest.mock import patch
import zipfile

sys.dont_write_bytecode = True
RELEASE_DIR = Path(__file__).resolve().parent / "release"
sys.path.insert(0, str(RELEASE_DIR))

import package  # noqa: E402

_verify_spec = spec_from_file_location("verify_media", RELEASE_DIR / "verify-media.py")
verify_media = module_from_spec(_verify_spec)
_verify_spec.loader.exec_module(verify_media)

_official_spec = spec_from_file_location("official_plugins", RELEASE_DIR.parent / "build-official-plugins.py")
official_plugins = module_from_spec(_official_spec)
_official_spec.loader.exec_module(official_plugins)

_installer_spec = spec_from_file_location("check_windows_installer", RELEASE_DIR.parent / "check-windows-installer.py")
check_windows_installer = module_from_spec(_installer_spec)
_installer_spec.loader.exec_module(check_windows_installer)

_ci_spec = spec_from_file_location("run_plugin_v2_ci", RELEASE_DIR.parent / "run-plugin-v2-ci.py")
run_plugin_v2_ci = module_from_spec(_ci_spec)
_ci_spec.loader.exec_module(run_plugin_v2_ci)

_check_spec = spec_from_file_location("check_project", RELEASE_DIR.parent / "check-project.py")
check_project = module_from_spec(_check_spec)
_check_spec.loader.exec_module(check_project)

_webp_spec = spec_from_file_location("make_webp_test_media", RELEASE_DIR.parent / "make-webp-test-media.py")
make_webp_test_media = module_from_spec(_webp_spec)
_webp_spec.loader.exec_module(make_webp_test_media)


class OfficialPluginVerificationTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.embedded = self.root / "Resources/Plugins"
        for name in ("guides", "webdav"):
            crate = self.root / "crates" / f"iris-plugin-{name}"
            crate.mkdir(parents=True)
            (crate / "plugin.json").write_text(json.dumps({
                "id": f"iris.{name}", "version": "0.0.2", "protocol": 2, "abi": 2, "entry": "plugin.wasm",
            }), encoding="utf-8", newline="\n")
            wasm = self.root / "target/wasm32-unknown-unknown/release" / f"iris_plugin_{name}.wasm"
            wasm.parent.mkdir(parents=True, exist_ok=True)
            wasm.write_bytes(b"\x00asm\x01\x00\x00\x00")

    def invoke(self, *args, build=False, env_override=None):
        options = [] if build else ["--no-build"]
        patched_env = os.environ.copy()
        patched_env.pop("CARGO_TARGET_DIR", None)
        if env_override:
            patched_env.update(env_override)
        with patch.dict(os.environ, patched_env, clear=True), \
             patch.object(official_plugins, "ROOT", self.root), \
             patch.object(official_plugins, "source_hash", return_value="current-source"), \
             patch.object(sys, "argv", ["build-official-plugins.py", *options, *map(str, args)]), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            official_plugins.main()

    def snapshot(self):
        return {p.name: p.read_bytes() for p in self.embedded.iterdir()}

    def test_verification_compares_independent_output_and_preserves_stale_embedded_bytes(self):
        self.invoke("--output", self.embedded)
        self.invoke("--verify-embedded")  # identical, independently packaged build
        index_path = self.embedded / "index.json"
        index = json.loads(index_path.read_text(encoding="utf-8"))
        index["source_sha256"] = "stale-source"
        index_path.write_text(json.dumps(index), encoding="utf-8", newline="\n")
        before = self.snapshot()
        with self.assertRaisesRegex(AssertionError, "stale"):
            self.invoke("--verify-embedded")
        self.assertEqual(self.snapshot(), before)

    def test_source_stamp_covers_only_the_plugin_dependency_closure(self):
        def lock(host_version="1.0.0", leaf_checksum="aa"):
            return f"""version = 4

[[package]]
name = "plugin"
version = "0.1.0"
dependencies = ["dep 1.0.0", "leaf"]

[[package]]
name = "dep"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "11"

[[package]]
name = "dep"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "22"

[[package]]
name = "leaf"
version = "0.3.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "{leaf_checksum}"

[[package]]
name = "host"
version = "{host_version}"
dependencies = ["dep 2.0.0 (registry+https://github.com/rust-lang/crates.io-index)"]
"""

        closure = json.loads(official_plugins.lock_closure(lock(), ["plugin"]))
        self.assertEqual(
            [(p["name"], p["version"]) for p in closure["package"]],
            [("dep", "1.0.0"), ("leaf", "0.3.0"), ("plugin", "0.1.0")],
        )
        base = official_plugins.lock_closure(lock(), ["plugin"])
        self.assertEqual(official_plugins.lock_closure(lock(host_version="9.0.0"), ["plugin"]), base)
        self.assertNotEqual(official_plugins.lock_closure(lock(leaf_checksum="bb"), ["plugin"]), base)
        self.assertEqual(official_plugins.lock_closure(lock().replace("\n", "\r\n"), ["plugin"]), base)
        with self.assertRaisesRegex(AssertionError, "exactly one package"):
            official_plugins.lock_closure(lock(), ["dep"])

    def test_index_json_uses_lf_and_check_rejects_crlf(self):
        self.invoke("--output", self.embedded)
        index_path = self.embedded / "index.json"
        raw = index_path.read_bytes()
        self.assertNotIn(b"\r", raw)
        self.assertTrue(raw.endswith(b"\n"))
        index_path.write_bytes(raw.replace(b"\n", b"\r\n"))
        with self.assertRaisesRegex(AssertionError, "LF"):
            self.invoke("--check", "--output", self.embedded)

    def test_respects_external_cargo_target_dir_clears_rustflags_and_remaps_all_homes(self):
        external_target = self.root / "custom-target"
        fake_cargo = self.root / "cargo-home"
        fake_rustup = self.root / "rustup-home"
        fake_sysroot = fake_rustup / "toolchains/1.100.0"
        for d in (fake_cargo, fake_rustup, fake_sysroot):
            d.mkdir(parents=True, exist_ok=True)
        for name in ("guides", "webdav"):
            wasm = external_target / "wasm32-unknown-unknown/release" / f"iris_plugin_{name}.wasm"
            wasm.parent.mkdir(parents=True, exist_ok=True)
            wasm.write_bytes(b"\x00asm\x01\x00\x00\x00ext")
            # Remove workspace target wasm so packaging proves it reads from CARGO_TARGET_DIR.
            (self.root / "target/wasm32-unknown-unknown/release" / f"iris_plugin_{name}.wasm").unlink()
        with patch.object(official_plugins, "_rustc_sysroot", return_value=fake_sysroot), \
             patch.object(official_plugins.subprocess, "run") as run_mock:
            self.invoke(
                "--output",
                self.embedded,
                build=True,
                env_override={
                    "CARGO_TARGET_DIR": str(external_target),
                    "CARGO_HOME": str(fake_cargo),
                    "RUSTUP_HOME": str(fake_rustup),
                    "RUSTFLAGS": "-C target-feature=+crt-static",
                },
            )
            run_mock.assert_called_once()
            build_env = run_mock.call_args.kwargs["env"]
            self.assertEqual(build_env["CARGO_TARGET_DIR"], str(external_target))
            self.assertNotIn("RUSTFLAGS", build_env)
            self.assertEqual(build_env["CARGO_PROFILE_RELEASE_STRIP"], "symbols")
            remaps = build_env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
            self.assertTrue(any(r.endswith("=/iris") for r in remaps))
            self.assertTrue(any(r.endswith("=/cargo") for r in remaps))
            self.assertTrue(any(r.endswith("=/rustup") for r in remaps))
            self.assertTrue(any(r.endswith("=/rustup-sysroot") for r in remaps))
        with zipfile.ZipFile(self.embedded / "iris.guides.irisplugin") as archive:
            self.assertEqual(archive.read("plugin.wasm"), b"\x00asm\x01\x00\x00\x00ext")

    def test_rejects_host_path_leaks_in_built_or_embedded_wasm(self):
        wasm = self.root / "target/wasm32-unknown-unknown/release/iris_plugin_guides.wasm"
        for leaked in (
            b"\x00asm\x01\x00\x00\x00C:\\Users\\alice\\.cargo\\registry\\src\\lib.rs",
            b"\x00asm\x01\x00\x00\x00/Users/bob/.rustup/toolchains/lib.rs",
            b"\x00asm\x01\x00\x00\x00/home/runner/.cargo/registry/lib.rs",
            b"\x00asm\x01\x00\x00\x00" + str(self.root).encode("utf-8") + b"/crates/lib.rs",
        ):
            with self.subTest(leaked=leaked):
                wasm.write_bytes(leaked)
                with self.assertRaisesRegex(RuntimeError, "leaked"):
                    self.invoke("--output", self.embedded)

    def test_rejects_official_capability_and_network_expansion(self):
        guides_manifest = self.root / "crates/iris-plugin-guides/plugin.json"
        guides_manifest.write_text(json.dumps({
            "id": "iris.guides", "version": "0.0.2", "protocol": 2, "abi": 2,
            "entry": "plugin.wasm", "permissions": ["ui_overlay", "net_tcp"],
        }), encoding="utf-8", newline="\n")
        with self.assertRaisesRegex(AssertionError, "expanded permissions"):
            self.invoke("--output", self.embedded)
        guides_manifest.write_text(json.dumps({
            "id": "iris.guides", "version": "0.0.2", "protocol": 2, "abi": 2,
            "entry": "plugin.wasm", "permissions": ["ui_overlay"], "network": {"net_public": True},
        }), encoding="utf-8", newline="\n")
        with self.assertRaisesRegex(AssertionError, "network declaration"):
            self.invoke("--output", self.embedded)

    def test_verification_enforces_windows_byte_equality_and_posix_manifest_and_stamp_checks(self):
        self.invoke("--output", self.embedded)
        before = self.snapshot()
        wasm = self.root / "target/wasm32-unknown-unknown/release/iris_plugin_webdav.wasm"
        wasm.write_bytes(wasm.read_bytes() + b"crates/iris-api/src/v2/integer.rs")
        with patch.object(official_plugins, "_is_windows_host", return_value=True):
            with self.assertRaisesRegex(AssertionError, "differs"):
                self.invoke("--verify-embedded")
        # On POSIX (e.g. macOS CI), OS Location::caller() slash differences are tolerated
        # only when source_sha256, embedded archive hashes, manifests, and leak checks all pass.
        with patch.object(official_plugins, "_is_windows_host", return_value=False):
            self.invoke("--verify-embedded")
            # Changing source manifest without rebuilding embedded bundle is still rejected on POSIX.
            webdav_manifest = self.root / "crates/iris-plugin-webdav/plugin.json"
            webdav_manifest.write_text(json.dumps({
                "id": "iris.webdav", "version": "0.0.2", "protocol": 2, "abi": 2,
                "entry": "plugin.wasm", "permissions": ["media_source"],
            }), encoding="utf-8", newline="\n")
            with self.assertRaisesRegex(AssertionError, "plugin.json differs"):
                self.invoke("--verify-embedded")
        self.assertEqual(self.snapshot(), before)

    def test_in_place_verification_fails_before_building_or_mutating_embedded_packages(self):
        self.invoke("--output", self.embedded)
        before = self.snapshot()
        alias = self.embedded / ".." / "Plugins"
        with patch.object(official_plugins.subprocess, "run") as build:
            with self.assertRaises(SystemExit) as error:
                self.invoke("--verify-embedded", "--output", alias, build=True)
            self.assertEqual(error.exception.code, 2)
            build.assert_not_called()
        self.assertEqual(self.snapshot(), before)

    def test_release_uses_the_isolated_output_verify_directory(self):
        out = self.root / "dist-out"
        with patch.object(package, "run") as run:
            package.verify_official_plugins({}, out)
        self.assertEqual(run.call_args.args, ([
            sys.executable, "-B", package.ROOT / "scripts/build-official-plugins.py",
            "--verify-embedded", "--output", str(out / "official-plugin-verify"),
        ], package.ROOT, {}))


class PluginRuntimeTests(unittest.TestCase):
    def test_staged_host_must_execute_both_official_plugins_with_current_abi(self):
        receipt = {"status": "ok", "protocol": 2, "abi": 2, "plugins": [{"plugin": "iris.guides"}, {"plugin": "iris.webdav"}]}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for windows in (False, True):
                with patch.object(package.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(receipt), "")) as run:
                    package.check_plugin_runtime(root, windows, root)
                    self.assertEqual(run.call_args.args[0][0], str(root / ("iris.exe" if windows else "Contents/MacOS/iris")))
                    self.assertTrue(run.call_args.kwargs["check"])
                    self.assertEqual(run.call_args.kwargs["timeout"], 180)
            run_plugin_v2_ci.verify_runtime_receipt(receipt)
            for bad in ({**receipt, "abi": 1}, {**receipt, "protocol": 1}, {**receipt, "plugins": [{"plugin": "iris.guides"}]}, {**receipt, "plugins": [{"plugin": "iris.guides"}] * 2}):
                with patch.object(package.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(bad), "")):
                    with self.assertRaises(RuntimeError):
                        package.check_plugin_runtime(root, True, root)
                with self.assertRaises(RuntimeError):
                    run_plugin_v2_ci.verify_runtime_receipt(bad)

    def test_signed_entitlement_mismatch_is_rejected_before_distribution(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            frameworks = root / "Frameworks"
            frameworks.mkdir()
            with patch.object(package, "run", return_value="<?xml version=\"1.0\"?><plist version=\"1.0\"><dict/></plist>"):
                with self.assertRaisesRegex(RuntimeError, "entitlements"):
                    package.sign_payload(root, root / "iris", frameworks, root, {}, "macos-arm64", "local", "0.0.1", False, root)


class WindowsSigningTests(unittest.TestCase):
    def test_both_signers_share_the_selected_powershell_and_signtool_commands(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            system_root = root / "Windows $system"
            inbox = system_root / "System32/WindowsPowerShell/v1.0/powershell.exe"
            inbox.parent.mkdir(parents=True)
            inbox.touch()
            pwsh = root / "PowerShell $7/pwsh.exe"
            iscc = root / "Inno/ISCC.exe"
            payload = root / "Iris $payload"
            payload.mkdir()
            executable = payload / "Iris.exe"
            output = root / "Output"
            output.mkdir()
            script = root / "Release $q scripts/sign-windows.ps1"
            sdk_signtool = root / "Windows Kits/10/bin/10.0.22621.0/x64/signtool.exe"
            sdk_signtool.parent.mkdir(parents=True)
            sdk_signtool.write_bytes(b"MZ")

            for preferred, expected in ((pwsh, pwsh), (None, inbox)):
                with self.subTest(preferred=preferred), patch.dict(
                     package.os.environ,
                     {"SystemRoot": str(system_root), "WindowsSdkVerBinPath": str(sdk_signtool.parent.parent)},
                     clear=False,
                 ), \
                     patch.object(package, "__file__", str(script.with_name("package.py"))), \
                     patch.object(package.shutil, "which", side_effect={"pwsh.exe": preferred, "ISCC": str(iscc)}.get), \
                     patch.object(package, "run") as run:
                    package.sign_payload(payload, executable, payload, payload, {}, "windows-x86_64", "release",
                                         "0.0.1", True, output)
                    package.build_installer(payload, output, "iris", "0.0.1", True)
                    direct, installer, verify = (call.args[0] for call in run.call_args_list)
                    options = "-NoProfile -ExecutionPolicy Bypass -File"
                    self.assertEqual(
                        direct,
                        [expected.resolve(), *options.split(), script, "-SignTool", sdk_signtool.resolve(), "-File", executable],
                    )
                    signtool = next(arg for arg in installer if str(arg).startswith("/Siris="))
                    quoted = re.sub(r"\$(\$|q|f)", lambda m: {"$": "$", "q": '"', "f": f'"{executable}"'}[m[1]],
                                    signtool.removeprefix("/Siris="))
                    self.assertEqual(
                        quoted,
                        f'"{expected.resolve()}" {options} "{script}" -SignTool "{sdk_signtool.resolve()}" -File "{executable}"',
                    )
                    self.assertEqual(verify[:4], [sdk_signtool.resolve(), "verify", "/pa", "/all"])

    def test_windows_signtool_resolves_trusted_sdk_and_override_and_ignores_cwd(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            pf86 = root / "Program Files (x86)"
            old_sdk = pf86 / "Windows Kits/10/bin/10.0.19041.0/x64/signtool.exe"
            new_sdk = pf86 / "Windows Kits/10/bin/10.0.22621.0/x64/signtool.exe"
            override_exe = root / "custom/signtool.exe"
            cwd_dir = root / "cwd"
            for exe in (old_sdk, new_sdk, override_exe, cwd_dir / "signtool.exe"):
                exe.parent.mkdir(parents=True, exist_ok=True)
                exe.write_bytes(b"MZ")
            for resolver in (package._windows_signtool, check_windows_installer.find_signtool):
                with self.subTest(resolver=resolver.__module__):
                    with patch.dict(os.environ, {"ProgramFiles(x86)": str(pf86), "PATH": ""}, clear=True):
                        self.assertEqual(resolver(), new_sdk.resolve())
                    with patch.dict(os.environ, {"IRIS_SIGNTOOL": str(override_exe), "PATH": ""}, clear=True):
                        self.assertEqual(resolver(), override_exe.resolve())
                    with patch.dict(os.environ, {"IRIS_SIGNTOOL": "relative/signtool.exe", "PATH": ""}, clear=True):
                        with self.assertRaisesRegex(RuntimeError, "IRIS_SIGNTOOL"):
                            resolver()
                    old_cwd = Path.cwd()
                    try:
                        os.chdir(cwd_dir)
                        with patch.dict(os.environ, {"PATH": f".{os.pathsep}relative"}, clear=True):
                            with self.assertRaisesRegex(RuntimeError, "signtool"):
                                resolver()
                    finally:
                        os.chdir(old_cwd)

    def test_windows_signer_ignores_relative_and_current_directory_pwsh(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            cwd_dir = root / "cwd"
            cwd_dir.mkdir()
            rogue = cwd_dir / "pwsh.exe"
            rogue.write_bytes(b"MZ")
            system_root = root / "Windows"
            inbox = system_root / "System32/WindowsPowerShell/v1.0/powershell.exe"
            inbox.parent.mkdir(parents=True)
            inbox.write_bytes(b"MZ")
            sdk_signtool = root / "Windows Kits/10/bin/10.0.22621.0/x64/signtool.exe"
            sdk_signtool.parent.mkdir(parents=True)
            sdk_signtool.write_bytes(b"MZ")
            old_cwd = Path.cwd()
            try:
                os.chdir(cwd_dir)
                with patch.dict(
                    package.os.environ,
                    {
                        "PATH": f".{os.pathsep}relative",
                        "SystemRoot": str(system_root),
                        "IRIS_SIGNTOOL": str(sdk_signtool),
                    },
                    clear=False,
                ):
                    signer = package._windows_signer()
                    self.assertEqual(signer[0], inbox.resolve())
                    self.assertEqual(signer[signer.index("-SignTool") + 1], sdk_signtool.resolve())
            finally:
                os.chdir(old_cwd)

    @unittest.skipUnless(os.name == "nt", "Windows PowerShell native quoting test")
    def test_sign_windows_ps1_preserves_quotes_and_rejects_relative_path_hijack(self):
        inbox = Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32/WindowsPowerShell/v1.0/powershell.exe"
        if not inbox.is_file():
            self.skipTest("inbox powershell.exe unavailable")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            # Use a tiny compiled C# shim so Win32 CommandLineToArgvW parses the exact string without cmd.exe mangling.
            csc_candidates = sorted(Path(os.environ.get("SystemRoot", r"C:\Windows")).glob("Microsoft.NET/Framework64/*/csc.exe"))
            if not csc_candidates:
                self.skipTest("csc.exe unavailable")
            cs_src = root / "signtool.cs"
            cs_src.write_text(
                "using System; using System.IO; using System.Text;\n"
                "class P { static int Main(string[] a) {\n"
                "  File.AppendAllLines(Environment.GetEnvironmentVariable(\"OUT_LINES\"), a, Encoding.UTF8);\n"
                "  File.AppendAllText(Environment.GetEnvironmentVariable(\"OUT_LINES\"), \"---\\n\", Encoding.UTF8);\n"
                "  return 0;\n"
                "} }\n",
                encoding="utf-8",
                newline="\n",
            )
            fake_exe = root / "trusted bin/signtool.exe"
            fake_exe.parent.mkdir(parents=True)
            subprocess.run([str(csc_candidates[-1]), "/nologo", f"/out:{fake_exe}", str(cs_src)],
                           check=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            out_lines = root / "lines.txt"
            tricky_pw = 'pa"ss\\"word with $spaces\\'
            target_file = root / "Iris App.exe"
            target_file.write_bytes(b"MZ")
            base_env = os.environ.copy()
            base_env.pop("IRIS_SIGNTOOL", None)
            base_env.update(
                OUT_LINES=str(out_lines),
                IRIS_WINDOWS_PFX=str(root / "cert.pfx"),
                IRIS_WINDOWS_PFX_PASSWORD=tricky_pw,
                IRIS_TIMESTAMP_URL="http://timestamp.example.com",
            )
            # 1. Explicit -SignTool executes trusted binary and preserves tricky quoting.
            subprocess.run(
                [str(inbox), "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                 str(RELEASE_DIR / "sign-windows.ps1"), "-SignTool", str(fake_exe), "-File", str(target_file)],
                env={**base_env, "PATH": ""},
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            calls = [block.splitlines() for block in out_lines.read_text(encoding="utf-8-sig").split("---\n") if block]
            self.assertEqual(len(calls), 2)
            self.assertEqual(calls[0], [
                "sign", "/fd", "SHA256", "/td", "SHA256",
                "/tr", "http://timestamp.example.com",
                "/f", str(root / "cert.pfx"),
                "/p", tricky_pw,
                str(target_file),
            ])
            self.assertEqual(calls[1], ["verify", "/pa", "/all", str(target_file)])

            # 2. Rogue signtool.exe in CWD / relative PATH entry must NOT be executed when -SignTool is omitted.
            rogue_dir = root / "rogue"
            rogue_dir.mkdir()
            shutil.copy2(fake_exe, rogue_dir / "signtool.exe")
            out_lines.unlink()
            rejected = subprocess.run(
                [str(inbox), "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                 str(RELEASE_DIR / "sign-windows.ps1"), "-File", str(target_file)],
                cwd=rogue_dir,
                env={**base_env, "PATH": f".{os.pathsep}..\\rogue"},
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            self.assertNotEqual(rejected.returncode, 0)
            self.assertFalse(out_lines.exists(), "Rogue relative-PATH signtool.exe must never be invoked")

    def test_installer_check_waits_for_cleanup_and_preserves_primary_error_when_uninstaller_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            installer = Path(directory) / "Iris-setup.exe"
            installer.write_bytes(b"MZ")
            waited = []

            def fake_run_setup(exe, *_args):
                dest = Path(exe) if exe.name == "unins000.exe" else Path(_args[0].removeprefix("/DIR=")) / "unins000.exe"
                if exe == installer:
                    dest.parent.mkdir(parents=True, exist_ok=True)
                    dest.write_bytes(b"MZ")
                    (dest.parent / "iris.exe").write_bytes(b"MZ")
                else:
                    raise subprocess.CalledProcessError(1, [str(exe)])

            def fake_sleep(seconds):
                waited.append(seconds)

            with patch.object(check_windows_installer, "residue", return_value=[]), \
                 patch.object(check_windows_installer, "key_exists", return_value=False), \
                 patch.object(check_windows_installer, "run_setup", side_effect=fake_run_setup), \
                 patch.object(check_windows_installer.time, "sleep", side_effect=fake_sleep):
                with self.assertRaisesRegex(RuntimeError, "Installation is incomplete"):
                    check_windows_installer.check(installer, signed=False, wait_timeout=0.05)
            self.assertTrue(waited, "Cleanup wait loop must still run before TemporaryDirectory teardown")


class NotarizationAndArchiveTests(unittest.TestCase):
    def test_notarize_separates_stderr_warnings_and_saves_rejected_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            submission = root / "Iris.zip"
            submission.write_bytes(b"zip")
            app = root / "Iris.app"
            evidence = root / "notary-result.json"

            ok_submit = subprocess.CompletedProcess(
                args=[], returncode=0,
                stdout='{"id":"sub-1","status":"Accepted"}\n',
                stderr="Warning: keychain search list notice\n",
            )
            with patch.dict(package.os.environ, {"IRIS_NOTARY_PROFILE": "iris-prof"}, clear=False), \
                 patch.object(package, "_run_capture", return_value=ok_submit), \
                 patch.object(package, "run") as run_mock:
                package.notarize(submission, app, evidence)
                saved = json.loads(evidence.read_text(encoding="utf-8"))
                self.assertEqual(saved["status"], "Accepted")
                self.assertIn("keychain", saved["stderr"])
                self.assertEqual(len(run_mock.call_args_list), 2)

            bad_submit = subprocess.CompletedProcess(
                args=[], returncode=1,
                stdout='{"id":"sub-2","status":"Invalid"}\n',
                stderr="Submission rejected\n",
            )
            bad_log = subprocess.CompletedProcess(
                args=[], returncode=0,
                stdout='{"issues":[{"severity":"error","message":"Unsigned binary"}]}\n',
                stderr="",
            )
            evidence_bad = root / "notary-bad.json"
            with patch.dict(package.os.environ, {"IRIS_NOTARY_PROFILE": "iris-prof"}, clear=False), \
                 patch.object(package, "_run_capture", side_effect=[bad_submit, bad_log]), \
                 patch.object(package, "run") as run_mock:
                with self.assertRaisesRegex(RuntimeError, "did not accept"):
                    package.notarize(submission, app, evidence_bad)
                self.assertFalse(run_mock.called)
                saved_bad = json.loads(evidence_bad.read_text(encoding="utf-8"))
                self.assertEqual(saved_bad["status"], "Invalid")
                self.assertEqual(saved_bad["returncode"], 1)
                self.assertEqual(saved_bad["notary_log"]["issues"][0]["message"], "Unsigned binary")

    def test_verify_vendor_archive_requires_exact_file_set_match(self):
        with tempfile.TemporaryDirectory() as directory:
            archive_path = Path(directory) / "vendor.tar.gz"
            src = b"pub fn ok() {}\n"
            digest = hashlib.sha256(src).hexdigest()
            checksum = json.dumps({"files": {"src/lib.rs": digest}}).encode("utf-8")

            def write_archive(extra=False):
                with tarfile.open(archive_path, "w:gz") as tar:
                    for name, data in (("demo-0.1.0/src/lib.rs", src), ("demo-0.1.0/.cargo-checksum.json", checksum)):
                        info = tarfile.TarInfo(name)
                        info.size = len(data)
                        tar.addfile(info, io.BytesIO(data))
                    if extra:
                        untracked = b"untracked"
                        info = tarfile.TarInfo("demo-0.1.0/injected.rs")
                        info.size = len(untracked)
                        tar.addfile(info, io.BytesIO(untracked))

            write_archive(extra=False)
            package.verify_vendor_archive(archive_path)
            write_archive(extra=True)
            with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
                package.verify_vendor_archive(archive_path)


class MacDependencyTests(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        root = Path(folder.name).resolve()
        self.prefix = root / "relocated SDK/prefix"
        self.frameworks = root / "Frameworks"
        self.frameworks.mkdir()
        self.executable = root / "iris"
        self.files = {}
        self.ids = {}
        self.dependencies = {}
        self.commands = []

    def dylib(self, relative, identity, pinned=True):
        path = self.prefix / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(relative, encoding="utf-8", newline="\n")
        if pinned:
            self.files[relative] = package.sha(path)
            self.ids[path.name] = identity
            self.dependencies[path.name] = [identity]
        return path

    def run_tool(self, args):
        self.commands.append(args)
        binary = args[-1]
        if args[0] == "codesign":
            return ""
        if args[0] == "lipo":
            return "arm64\n"
        if args[0] == "otool":
            header = f"{binary}:\n"
            if "-D" in args:
                return f"{header}{self.ids[binary.name]}\n"
            if "-L" in args:
                return header + "".join(f"\t{dep} (compatibility version 1.0.0, current version 1.0.0)\n"
                                        for dep in self.dependencies[binary.name])
            return header + "cmd LC_RPATH\ncmdsize 32\npath /old build/prefix/lib (offset 12)\n"
        self.assertEqual(args[0], "install_name_tool")
        return ""

    def resolve(self, imports):
        self.dependencies[self.executable.name] = imports
        with patch.object(package, "run", self.run_tool):
            return package.mac_dependencies(self.executable, self.prefix, {"files": self.files}, self.frameworks, "arm64")

    def test_relocated_absolute_install_id_and_nested_sdk_paths(self):
        mpv_id = "/old build/prefix/lib/libmpv.2.dylib"
        dep = "/old build/prefix/lib/private/libhelper.dylib"
        system = "/usr/lib/libSystem.B.dylib"
        self.dylib("lib/libmpv.2.dylib", mpv_id)
        self.dylib("lib/private/libhelper.dylib", "@rpath/private/libhelper.dylib")
        self.dependencies["libmpv.2.dylib"].append(dep)
        graph = self.resolve([mpv_id, system])
        self.assertEqual(graph[self.executable.name], [mpv_id, system])
        self.assertEqual({path.name for path in self.frameworks.iterdir()}, {"libmpv.2.dylib", "libhelper.dylib"})
        self.assertIn(["install_name_tool", "-change", mpv_id, "@rpath/libmpv.2.dylib", self.executable], self.commands)
        self.assertIn(["install_name_tool", "-change", dep, "@rpath/libhelper.dylib",
                       self.frameworks / "libmpv.2.dylib"], self.commands)
        self.assertTrue(any(command[1] == "-delete_rpath" for command in self.commands))
        self.assertFalse(any(command[1:3] == ["-change", system] for command in self.commands))

    def test_resolves_a_pinned_symlink_alias_after_relocation(self):
        identity = "/old build/prefix/lib/libmedia.2.dylib"
        source = self.dylib("lib/libmedia.2.dylib", identity)
        alias = source.with_name("libmedia.dylib")
        try:
            alias.symlink_to(source.name)
        except OSError as error:
            self.skipTest(f"Cannot create a test symlink: {error}")
        self.files["lib/libmedia.dylib"] = package.sha(alias)
        self.dependencies[alias.name] = [identity]
        self.resolve(["/old build/prefix/lib/libmedia.dylib"])
        staged = self.frameworks / alias.name
        self.assertEqual(staged.read_bytes(), source.read_bytes())
        self.assertFalse(staged.is_symlink())

    def test_absolute_install_id_can_differ_from_sdk_layout(self):
        old_id = "/original staging/lib/libmedia.dylib"
        self.dylib("lib/private/libmedia.dylib", old_id)
        self.resolve([old_id])
        self.assertTrue((self.frameworks / "libmedia.dylib").is_file())

    def test_rejects_external_and_unpinned_imports(self):
        verified = "/old build/prefix/lib/libverified.dylib"
        self.dylib("lib/libverified.dylib", verified)
        self.dylib("lib/libunpinned.dylib", "", pinned=False)
        for dep, message in (
            ("/opt/homebrew/lib/libverified.dylib", "Homebrew/MacPorts"),
            ("/external/lib/libverified.dylib", "External dylib"),
            ("/old build/prefix/lib/libunpinned.dylib", "External dylib"),
        ):
            with self.subTest(dep=dep), self.assertRaisesRegex(RuntimeError, message):
                self.resolve([dep])

    def test_does_not_read_install_ids_from_dylibs_with_wrong_hash(self):
        dep = "@rpath/libchanged.dylib"
        self.dylib("lib/libchanged.dylib", dep).write_bytes(b"changed")
        with self.assertRaisesRegex(RuntimeError, "Unverified media dylib"):
            self.resolve([dep])
        self.assertFalse(self.commands)

    def test_verify_media_stages_relocated_mac_sdk_with_loader_rpath(self):
        used_symlinks = True
        for name in ("mpv", "avcodec", "avfilter", "avformat", "avutil", "swresample", "swscale"):
            real_rel = f"lib/lib{name}.1.dylib"
            real_id = f"/old build/prefix/{real_rel}"
            real = self.dylib(real_rel, real_id)
            alias = real.with_name(f"lib{name}.dylib")
            try:
                alias.symlink_to(real.name)
                self.files[f"lib/lib{name}.dylib"] = package.sha(alias)
            except OSError:
                used_symlinks = False
                self.dylib(f"lib/lib{name}.dylib", f"/old build/prefix/lib/lib{name}.dylib")
        self.dependencies["libmpv.1.dylib" if used_symlinks else "libmpv.dylib"].append(
            "/old build/prefix/lib/libavcodec.1.dylib"
        )

        def loader_mock(name, path):
            spec = spec_from_file_location(name, path)
            mod = module_from_spec(spec)
            spec.loader.exec_module(mod)
            mod.run = self.run_tool
            return mod

        with tempfile.TemporaryDirectory() as stage_tmp:
            stage_dir = Path(stage_tmp).resolve()
            with patch.object(verify_media, "module_from_spec", side_effect=lambda _: loader_mock("iris_package", RELEASE_DIR / "package.py")):
                class _Spec:
                    class loader:
                        @staticmethod
                        def exec_module(_mod):
                            pass
                with patch.object(verify_media, "spec_from_file_location", return_value=_Spec):
                    verify_media.stage_relocated_mac_sdk(
                        self.prefix,
                        {"target": "macos-arm64", "files": self.files},
                        stage_dir,
                    )
            expected_mpv = "libmpv.1.dylib" if used_symlinks else "libmpv.dylib"
            if used_symlinks:
                self.assertTrue((stage_dir / "libmpv.dylib").is_symlink())
            self.assertTrue((stage_dir / expected_mpv).is_file())
            self.assertTrue((stage_dir / "libavcodec.1.dylib").is_file())
            self.assertIn(
                ["install_name_tool", "-add_rpath", "@loader_path", stage_dir / expected_mpv],
                self.commands,
            )


class PackagingSecurityAndComplianceTests(unittest.TestCase):
    def test_application_inputs_excludes_deleted_icon_concepts(self):
        self.assertFalse((package.ROOT / "Resources/IconConcepts").exists())
        inputs = package.application_inputs()
        self.assertFalse(any("IconConcepts" in path for path in inputs))

    def test_verify_sdk_files_enforces_path_traversal_checks_on_all_platforms(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            prefix = root / "prefix"
            good = prefix / "bin/mpv-2.dll"
            good.parent.mkdir(parents=True)
            good.write_bytes(b"dll")
            outside = root / "outside.dll"
            outside.write_bytes(b"escape")
            package.verify_sdk_files(prefix, {"bin/mpv-2.dll": package.sha(good)})
            for bad_path in (
                "../outside.dll",
                "bin/../../outside.dll",
                "/etc/passwd",
                "\\Windows\\System32\\kernel32.dll",
                "C:/Windows/System32/kernel32.dll",
                "C:outside.dll",
                "bin\\mpv-2.dll",
                "",
            ):
                with self.subTest(bad_path=bad_path):
                    with self.assertRaisesRegex(RuntimeError, "SDK path outside pinned prefix"):
                        package.verify_sdk_files(prefix, {bad_path: package.sha(outside)})

    def test_metadata_licenses_match_crate_names_across_version_bumps(self):
        resolved = package.resolve_metadata_licenses()
        self.assertTrue(resolved)
        self.assertIn("wasmtime-internal-core", {entry.split("@", 1)[0] for entry in resolved})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            lock = root / "Cargo.lock"
            lock.write_text(
                '[[package]]\nname = "wasmtime-internal-core"\nversion = "99.1.0"\n'
                '[[package]]\nname = "unlisted-crate"\nversion = "1.0.0"\n',
                encoding="utf-8",
                newline="\n",
            )
            (root / "LICENSE-MIT").write_text("MIT\n", encoding="utf-8", newline="\n")
            (root / "LICENSE-APACHE").write_text("Apache\n", encoding="utf-8", newline="\n")
            self.assertEqual(package.resolve_metadata_licenses(lock), frozenset({"wasmtime-internal-core@99.1.0"}))

            # Verify rust_notices accepts a locked allowlisted crate+version and rejects an unlisted or unlocked one.
            dest = root / "out"
            vendor = dest / "rust-vendor"
            dest.mkdir()
            (dest / "cargo-vendor-config.toml").write_text("", encoding="utf-8", newline="\n")
            crate_dir = vendor / "wasmtime-internal-core-99.1.0"
            crate_dir.mkdir(parents=True)
            cargo_toml = crate_dir / "Cargo.toml"
            cargo_toml.write_text('[package]\nname = "wasmtime-internal-core"\nlicense = "Apache-2.0"\n',
                                  encoding="utf-8", newline="\n")
            (crate_dir / ".cargo-checksum.json").write_text(
                json.dumps({"files": {"Cargo.toml": package.sha(cargo_toml)}}),
                encoding="utf-8",
                newline="\n",
            )
            fake_metadata = subprocess.CompletedProcess([], 0, json.dumps({
                "packages": [{
                    "name": "wasmtime-internal-core",
                    "version": "99.1.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "license": "Apache-2.0",
                    "license_file": None,
                }]
            }), "")
            state = {"vendor": True}
            checkpoint = dest / "package-checkpoint.json"
            with patch.object(package, "ROOT", root), \
                 patch.object(package.subprocess, "run", return_value=fake_metadata):
                package.rust_notices(dest, {}, checkpoint, state)
            self.assertTrue((dest / "rust-licenses/wasmtime-internal-core-99.1.0/Cargo.toml").is_file())
            self.assertNotIn(b"\r", (dest / "rust-license-manifest.json").read_bytes())

            unlisted_dir = vendor / "unlisted-crate-1.0.0"
            unlisted_dir.mkdir(parents=True)
            unlisted_toml = unlisted_dir / "Cargo.toml"
            unlisted_toml.write_text('[package]\nname = "unlisted-crate"\nlicense = "MIT"\n',
                                     encoding="utf-8", newline="\n")
            (unlisted_dir / ".cargo-checksum.json").write_text(
                json.dumps({"files": {"Cargo.toml": package.sha(unlisted_toml)}}),
                encoding="utf-8",
                newline="\n",
            )
            bad_metadata = subprocess.CompletedProcess([], 0, json.dumps({
                "packages": [{
                    "name": "unlisted-crate",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "license": "MIT",
                    "license_file": None,
                }]
            }), "")
            with patch.object(package, "ROOT", root), \
                 patch.object(package.subprocess, "run", return_value=bad_metadata):
                with self.assertRaisesRegex(RuntimeError, "No upstream license notice"):
                    package.rust_notices(dest, {}, checkpoint, {"vendor": True})


class PluginV2CiTests(unittest.TestCase):
    def test_ensure_zig_verifies_archive_and_binary_sha256_before_execution_and_recovers(self):
        with tempfile.TemporaryDirectory() as directory:
            ci_dir = Path(directory).resolve()
            target = "x86_64-windows"
            name = f"zig-{target}-{run_plugin_v2_ci.ZIG_VERSION}"
            zig_exe = ci_dir / name / "zig.exe"
            zig_exe.parent.mkdir(parents=True)

            valid_zip_buf = io.BytesIO()
            with zipfile.ZipFile(valid_zip_buf, "w") as zf:
                zf.writestr(f"{name}/zig.exe", b"MZ-valid")
            valid_zip_bytes = valid_zip_buf.getvalue()
            valid_digest = hashlib.sha256(valid_zip_bytes).hexdigest()
            archive = ci_dir / f"{name}.zip"
            archive.write_bytes(valid_zip_bytes)
            zig_exe.write_bytes(b"MZ-valid")

            # 1. Valid archive + matching zig.exe bytes: skip download and re-extraction.
            with patch.dict(run_plugin_v2_ci.ZIG, {target: ("zip", valid_digest)}), \
                 patch.object(run_plugin_v2_ci, "_zig_binary_valid", return_value=True) as valid_mock, \
                 patch.object(run_plugin_v2_ci, "_download_zig") as dl_mock:
                self.assertEqual(run_plugin_v2_ci.ensure_zig(ci_dir, target), zig_exe)
                dl_mock.assert_not_called()
                valid_mock.assert_called_once_with(zig_exe)

            # 2. Tampered cached zig.exe on disk: NEVER execute tampered bytes; re-extract from verified archive.
            zig_exe.write_bytes(b"MZ-tampered")
            seen_bytes = []

            def check_zig_only_after_restore(candidate):
                seen_bytes.append(candidate.read_bytes())
                return candidate.read_bytes() == b"MZ-valid"

            with patch.dict(run_plugin_v2_ci.ZIG, {target: ("zip", valid_digest)}), \
                 patch.object(run_plugin_v2_ci, "_zig_binary_valid", side_effect=check_zig_only_after_restore), \
                 patch.object(run_plugin_v2_ci, "_download_zig") as dl_mock:
                self.assertEqual(run_plugin_v2_ci.ensure_zig(ci_dir, target), zig_exe)
                dl_mock.assert_not_called()
                self.assertEqual(seen_bytes, [b"MZ-valid"])
                self.assertEqual(zig_exe.read_bytes(), b"MZ-valid")

            # 3. Corrupt zip on disk: remove corrupt zip, re-download, and extract cleanly.
            zig_exe.unlink()
            archive.write_bytes(b"not a valid zip archive")

            def fake_download(dest_archive, expected_digest):
                self.assertEqual(expected_digest, valid_digest)
                dest_archive.write_bytes(valid_zip_bytes)

            with patch.dict(run_plugin_v2_ci.ZIG, {target: ("zip", valid_digest)}), \
                 patch.object(run_plugin_v2_ci, "_download_zig", side_effect=fake_download) as dl_mock, \
                 patch.object(run_plugin_v2_ci, "_zig_binary_valid", return_value=True):
                resolved = run_plugin_v2_ci.ensure_zig(ci_dir, target)
                self.assertEqual(resolved, zig_exe)
                self.assertEqual(zig_exe.read_bytes(), b"MZ-valid")
                dl_mock.assert_called_once()

    def test_configure_mpv_env_leaves_posix_unset_and_defaults_only_on_windows(self):
        with tempfile.TemporaryDirectory() as directory:
            sdk = Path(directory).resolve() / "media-sdk/prefix"
            (sdk / "lib").mkdir(parents=True)
            (sdk / "bin").mkdir(parents=True)

            # macOS / Linux with unset MPV_LIB_DIR: must stay unset so iris-mpv/build.rs finds Homebrew.
            posix_env = run_plugin_v2_ci.configure_mpv_env({"PATH": "/usr/bin"}, windows=False)
            self.assertNotIn("MPV_LIB_DIR", posix_env)
            self.assertEqual(posix_env["PATH"], "/usr/bin")

            # macOS / Linux with explicit caller MPV_LIB_DIR: preserved verbatim.
            explicit_posix = run_plugin_v2_ci.configure_mpv_env(
                {"MPV_LIB_DIR": "/custom/mpv/lib", "PATH": "/usr/bin"}, windows=False
            )
            self.assertEqual(explicit_posix["MPV_LIB_DIR"], "/custom/mpv/lib")

            # Windows with unset MPV_LIB_DIR: defaults to target/deps/mpv and prepends PATH.
            win_default = run_plugin_v2_ci.configure_mpv_env({"PATH": r"C:\Windows\System32"}, windows=True)
            self.assertEqual(win_default["MPV_LIB_DIR"], str(run_plugin_v2_ci.ROOT / "target/deps/mpv"))
            self.assertTrue(win_default["PATH"].startswith(win_default["MPV_LIB_DIR"] + os.pathsep))

            # Windows with SDK prefix/lib: prepends prefix/bin ahead of prefix/lib.
            win_sdk = run_plugin_v2_ci.configure_mpv_env(
                {"MPV_LIB_DIR": str(sdk / "lib"), "PATH": r"C:\Windows\System32"}, windows=True
            )
            self.assertEqual(
                win_sdk["PATH"].split(os.pathsep)[:2],
                [str(sdk / "bin"), str(sdk / "lib")],
            )


class CheckProjectHelperTests(unittest.TestCase):
    def test_strip_comments_preserves_urls_raw_strings_lifetimes_and_escaped_quotes(self):
        src = (
            'const URL: &\'static str = "https://example.com/a//b"; // trailing use super::*;\n'
            'const RAW: &str = r#"/* keep */ // keep "https://x""#;\n'
            'const SLASH: char = \'/\'; /* outer /* nested */ still comment use super::*; */ let ok = 1;\n'
            'fn is_quote(c: char) -> bool { matches!(c,\'\\\'\'|\'"\') } // strip after escaped quote: iris_mpv\n'
        )
        stripped = check_project.strip_comments(src)
        self.assertIn('"https://example.com/a//b"', stripped)
        self.assertIn('r#"/* keep */ // keep "https://x""#', stripped)
        self.assertIn("let ok = 1;", stripped)
        self.assertIn("matches!(c,'\\''|'\"')", stripped)
        self.assertNotIn("use super::*;", stripped)
        self.assertNotIn("iris_mpv", stripped)

    def test_check_file_line_budget_rejects_trailing_prod_code_and_ignores_commented_cfg_test(self):
        rel = Path("crates/iris/src/demo.rs")
        clean_src = (
            "/// Tested in `#[cfg(test)] mod tests` below.\n"
            "pub fn ok() -> usize { 1 }\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            '    #[test] fn unbalanced_braces_in_literals() { let _ = "{"; let _ = \'}\'; let _ = r#"{"#; }\n'
            "}\n"
        )
        self.assertEqual(check_project.check_file_line_budget(rel, clean_src), 2)

        sneaky_trailing = clean_src + "pub fn sneaky_after_tests() {}\n"
        with self.assertRaisesRegex(AssertionError, "after inline `mod tests`"):
            check_project.check_file_line_budget(rel, sneaky_trailing)

    def test_lock_poisoning_gate_checks_production_code_only(self):
        rel = Path("crates/iris-core/src/demo.rs")
        clean = (
            "fn ok(m: &Mutex<u8>) -> u8 { *m.lock().unwrap_or_else(PoisonError::into_inner) }\n"
            "// m.lock().unwrap() in a comment is fine\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            "    #[test] fn t() { let _ = M.lock().unwrap(); }\n"
            "}\n"
        )
        check_project.check_lock_poisoning(rel, clean)
        for bad in (
            "fn f() { let _ = M.lock().unwrap(); }\n",
            "fn f() { let _ = R.read()\n        .expect(\"poisoned\"); }\n",
            "fn f() { *R.write().unwrap() = 1; }\n",
        ):
            with self.assertRaisesRegex(AssertionError, "unwraps a lock result"):
                check_project.check_lock_poisoning(rel, bad)

    def test_future_incompat_gate_allows_only_known_dependency_warnings(self):
        block = (
            "The package `block v0.1.6` currently triggers the following future incompatibility lints:\n"
            "> \x1b[1m\x1b[33mwarning\x1b[0m\x1b[1m: static of uninhabited type\x1b[0m\n"
            ">   --> block-0.1.6/src/lib.rs:64:5\n"
            ">    = warning: this was previously accepted by the compiler but is being phased out\n"
        )
        proc_macro = (
            "The package `proc-macro-error2 v2.0.1` currently triggers the following future incompatibility lints:\n"
            "> warning[E0365]: extern crate `proc_macro` is private and cannot be re-exported\n"
        )

        def check(per_package):
            with tempfile.TemporaryDirectory() as directory:
                report = Path(directory) / "report.json"
                report.write_text(json.dumps({"version": 0, "reports": [{"id": 1, "per_package": per_package}]}), encoding="utf-8")
                check_project.check_future_incompatibilities(report)

        check({"block@0.1.6": block, "proc-macro-error2@2.0.1": proc_macro})
        for bad in (
            {"block@0.1.7": block},
            {"block@0.1.6": block + "> warning: another lint\n"},
            {"block@0.1.6": proc_macro},
            {"proc-macro-error2@2.0.1": "nothing parsed\n"},
        ):
            with self.assertRaisesRegex(RuntimeError, "Unexpected Cargo future incompatibility"):
                check(bad)

    def test_standalone_tests_rs_classified_as_test_file(self):
        self.assertTrue(check_project.is_standalone_test_file(Path("examples/plugins/baidupan/src/tests.rs"), Path("examples/plugins/baidupan/src/tests.rs")))
        self.assertFalse(check_project.is_standalone_test_file(Path("crates/iris/src/debug.rs"), Path("crates/iris/src/debug.rs")))


class MediaFixtureScriptTests(unittest.TestCase):
    def test_make_webp_test_media_rejects_missing_explicit_icc_overrides_without_falling_back(self):
        saved = {}

        class FakeImg:
            def save(self, path, **kwargs):
                saved[Path(path).name] = kwargs.get("icc_profile")

        fake_pil = types.ModuleType("PIL")
        fake_pil.features = types.SimpleNamespace(check=lambda name: name == "webp")
        fake_pil.Image = types.SimpleNamespace(new=lambda *_a, **_k: FakeImg())
        fake_pil.ImageCms = types.SimpleNamespace(
            createProfile=lambda _name: object(),
            ImageCmsProfile=lambda _p: types.SimpleNamespace(tobytes=lambda: b"srgb-bytes"),
        )
        with tempfile.TemporaryDirectory() as directory, patch.dict(sys.modules, {"PIL": fake_pil}):
            root = Path(directory).resolve()
            out = root / "icc"
            out.mkdir()
            (out / "p3.icc").write_bytes(b"fallback-p3")
            (out / "adobergb.icc").write_bytes(b"fallback-adobe")

            for flag in ("--display-p3", "--adobe-rgb"):
                err = io.StringIO()
                with contextlib.redirect_stderr(err), self.assertRaises(SystemExit) as ctx:
                    make_webp_test_media.main(["--output", str(out), flag, str(root / "missing.icc")])
                self.assertEqual(ctx.exception.code, 2)
                self.assertIn(f"{flag} ICC profile not found", err.getvalue())
                self.assertFalse(saved)

            custom_p3 = root / "custom-p3.icc"
            custom_adobe = root / "custom-adobe.icc"
            custom_p3.write_bytes(b"explicit-p3")
            custom_adobe.write_bytes(b"explicit-adobe")
            with contextlib.redirect_stdout(io.StringIO()):
                make_webp_test_media.main([
                    "--output", str(out),
                    "--display-p3", str(custom_p3),
                    "--adobe-rgb", str(custom_adobe),
                ])
            self.assertEqual(saved["red-p3-webp.webp"], b"explicit-p3")
            self.assertEqual(saved["red-adobergb-webp.webp"], b"explicit-adobe")


if __name__ == "__main__":
    unittest.main()
