import re
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import package


class WindowsSigningTests(unittest.TestCase):
    def test_both_signers_share_the_selected_powershell_command(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            system_root = root / "Windows $system"
            inbox = system_root / "System32/WindowsPowerShell/v1.0/powershell.exe"
            inbox.parent.mkdir(parents=True)
            inbox.touch()
            pwsh = root / "PowerShell $7/pwsh.exe"
            payload = root / "Iris $payload"
            payload.mkdir()
            executable = payload / "Iris.exe"
            output = root / "Output"
            output.mkdir()
            script = root / "Release $q scripts/sign-windows.ps1"

            for preferred, expected in ((pwsh, pwsh), (None, inbox)):
                with self.subTest(preferred=preferred), patch.dict(package.os.environ, SystemRoot=str(system_root)), \
                     patch.object(package, "__file__", str(script.with_name("package.py"))), \
                     patch.object(package.shutil, "which", side_effect={"pwsh.exe": preferred, "ISCC": "ISCC.exe"}.get), \
                     patch.object(package, "run") as run:
                    package.sign_payload(payload, executable, payload, payload, {}, "windows-x86_64", "release",
                                         "0.0.1", True, output)
                    package.build_installer(payload, output, "iris", "0.0.1", True)
                    direct, installer, verify = (call.args[0] for call in run.call_args_list)
                    options = "-NoProfile -ExecutionPolicy Bypass -File"
                    self.assertEqual(direct, [expected, *options.split(), script, "-File", executable])
                    signtool = next(arg for arg in installer if str(arg).startswith("/Siris="))
                    quoted = re.sub(r"\$(\$|q|f)", lambda m: {"$": "$", "q": '"', "f": f'"{executable}"'}[m[1]],
                                    signtool.removeprefix("/Siris="))
                    self.assertEqual(quoted, f'"{expected}" {options} "{script}" -File "{executable}"')
                    self.assertEqual(verify[:4], ["signtool", "verify", "/pa", "/all"])


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


if __name__ == "__main__":
    unittest.main()
