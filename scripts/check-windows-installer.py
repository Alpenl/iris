"""Install a Windows distribution on a clean runner and verify its shell registrations."""

import argparse
from pathlib import Path
import subprocess
import tempfile
import time
import winreg


PROGID = "Iris.Media"
CLASSES = r"Software\Classes"
UNINSTALL = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{D8B2D853-0F4D-490C-B55E-61F686D0C1E8}_is1"
# Exercise one extension from each advertised media category.
OPEN_WITH = [rf"{CLASSES}\.{ext}\OpenWithProgids" for ext in ("mkv", "png", "flac")]


def value(key, name=None):
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, key) as handle:
            return winreg.QueryValueEx(handle, name)
    except FileNotFoundError:
        return None


def run_setup(exe, *args):
    subprocess.run([str(exe), "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", *args],
                   check=True, timeout=180)


def check(installer):
    with tempfile.TemporaryDirectory(prefix="iris-installer-") as tmp:
        dest = Path(tmp) / "Iris"
        exe = dest / "iris.exe"
        command = rf"{CLASSES}\{PROGID}\shell\open\command"
        if (value(UNINSTALL, "DisplayName") or value(command)
                or any(value(key, PROGID) for key in OPEN_WITH)):
            raise RuntimeError("An Iris installation already exists. Use a clean runner.")
        try:
            run_setup(installer, f"/DIR={dest}", "/NOICONS")
            if value(command) != (f'"{exe}" "%1"', winreg.REG_SZ):
                raise RuntimeError("Wrong shell open command")
            for key in OPEN_WITH:
                if value(key, PROGID) != ("", winreg.REG_SZ):
                    raise RuntimeError(f"Missing Open With registration in {key}")
        finally:
            uninstaller = dest / "unins000.exe"
            if uninstaller.is_file():
                run_setup(uninstaller)
        deadline = time.monotonic() + 10
        while exe.exists() and time.monotonic() < deadline:
            time.sleep(0.1)
        if exe.exists() or value(command) or any(value(key, PROGID) for key in OPEN_WITH):
            raise RuntimeError("Uninstall left behind the executable or shell registrations")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("installer", type=Path)
    check(parser.parse_args().installer.resolve())
