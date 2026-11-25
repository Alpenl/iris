#!/usr/bin/env python3
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
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
RELEASE_DIR = Path(__file__).resolve().parent / "release"
sys.path.insert(0, str(RELEASE_DIR))

import package  # noqa: E402

_verify_spec = spec_from_file_location("verify_media", RELEASE_DIR / "verify-media.py")
verify_media = module_from_spec(_verify_spec)
_verify_spec.loader.exec_module(verify_media)


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
            for bad in ({**receipt, "abi": 1}, {**receipt, "protocol": 1}, {**receipt, "plugins": [{"plugin": "iris.guides"}]}, {**receipt, "plugins": [{"plugin": "iris.guides"}] * 2}):
                with patch.object(package.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(bad), "")):
                    with self.assertRaises(RuntimeError):
                        package.check_plugin_runtime(root, True, root)

    def test_signed_entitlement_mismatch_is_rejected_before_distribution(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            frameworks = root / "Frameworks"
            frameworks.mkdir()
            with patch.object(package, "run", return_value="<?xml version=\"1.0\"?><plist version=\"1.0\"><dict/></plist>"):
                with self.assertRaisesRegex(RuntimeError, "entitlements"):
                    package.sign_payload(root, root / "iris", frameworks, root, {}, "macos-arm64", "local", "0.0.1", False, root)


class WindowsSigningTests(unittest.TestCase):
    def test_both_signers_share_the_selected_powershell_command(self):
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

            for preferred, expected in ((pwsh, pwsh), (None, inbox)):
                with self.subTest(preferred=preferred), patch.dict(package.os.environ, SystemRoot=str(system_root)), \
                     patch.object(package, "__file__", str(script.with_name("package.py"))), \
                     patch.object(package.shutil, "which", side_effect={"pwsh.exe": preferred, "ISCC": str(iscc)}.get), \
                     patch.object(package, "run") as run:
                    package.sign_payload(payload, executable, payload, payload, {}, "windows-x86_64", "release",
                                         "0.0.1", True, output)
                    package.build_installer(payload, output, "iris", "0.0.1", True)
                    direct, installer, verify = (call.args[0] for call in run.call_args_list)
                    options = "-NoProfile -ExecutionPolicy Bypass -File"
                    self.assertEqual(direct, [expected.resolve(), *options.split(), script, "-File", executable])
                    signtool = next(arg for arg in installer if str(arg).startswith("/Siris="))
                    quoted = re.sub(r"\$(\$|q|f)", lambda m: {"$": "$", "q": '"', "f": f'"{executable}"'}[m[1]],
                                    signtool.removeprefix("/Siris="))
                    self.assertEqual(quoted, f'"{expected.resolve()}" {options} "{script}" -File "{executable}"')
                    self.assertEqual(verify[:4], ["signtool", "verify", "/pa", "/all"])

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
            old_cwd = Path.cwd()
            try:
                os.chdir(cwd_dir)
                with patch.dict(package.os.environ, {"PATH": f".{os.pathsep}relative", "SystemRoot": str(system_root)}, clear=False):
                    signer = package._windows_signer()
                    self.assertEqual(signer[0], inbox.resolve())
            finally:
                os.chdir(old_cwd)

    @unittest.skipUnless(os.name == "nt", "Windows PowerShell native quoting test")
    def test_sign_windows_ps1_preserves_quotes_and_slashes_under_inbox_powershell(self):
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
            )
            fake_exe = root / "signtool.exe"
            subprocess.run([str(csc_candidates[-1]), "/nologo", f"/out:{fake_exe}", str(cs_src)],
                           check=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            out_lines = root / "lines.txt"
            tricky_pw = 'pa"ss\\"word with $spaces\\'
            target_file = root / "Iris App.exe"
            target_file.write_bytes(b"MZ")
            env = os.environ.copy()
            env.update(
                PATH=str(root) + os.pathsep + env.get("PATH", ""),
                OUT_LINES=str(out_lines),
                IRIS_WINDOWS_PFX=str(root / "cert.pfx"),
                IRIS_WINDOWS_PFX_PASSWORD=tricky_pw,
                IRIS_TIMESTAMP_URL="http://timestamp.example.com",
            )
            subprocess.run(
                [str(inbox), "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                 str(RELEASE_DIR / "sign-windows.ps1"), "-File", str(target_file)],
                env=env,
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
        path.write_text(relative, encoding="utf-8")
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


if __name__ == "__main__":
    unittest.main()
