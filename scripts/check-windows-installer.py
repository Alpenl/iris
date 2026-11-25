"""Install a Windows distribution on a clean runner and verify its shell registrations."""

import argparse
from pathlib import Path
import subprocess
import tempfile
import time
import winreg


REGISTRATION = r"Software\RegisteredApplications"
APP_ROOT = r"Software\Iris"
CAPABILITIES = r"Software\Iris\Capabilities"
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


def key_exists(key):
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, key):
            return True
    except FileNotFoundError:
        return False


def residue():
    keys = [key for key in (UNINSTALL, CAPABILITIES, APP_ROOT, rf"{CLASSES}\{PROGID}") if key_exists(key)]
    keys.extend(key for key in OPEN_WITH if value(key, PROGID) is not None)
    if value(REGISTRATION, "Iris") is not None:
        keys.append(REGISTRATION)
    return keys


def verify_signed(exe):
    subprocess.run(["signtool", "verify", "/pa", "/all", str(exe)], check=True, timeout=45)


def run_setup(exe, *args):
    subprocess.run([str(exe), "/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", *args],
                   check=True, timeout=180)


def check(installer, signed):
    if not installer.is_file():
        raise FileNotFoundError(installer)
    if signed:
        verify_signed(installer)
    with tempfile.TemporaryDirectory(prefix="iris-installer-") as tmp:
        dest = Path(tmp) / "Iris"
        exe = dest / "iris.exe"
        command = rf"{CLASSES}\{PROGID}\shell\open\command"
        if residue():
            raise RuntimeError("An Iris installation already exists. Use a clean runner.")
        try:
            run_setup(installer, f"/DIR={dest}", "/NOICONS")
            if not exe.is_file() or not (dest / "unins000.exe").is_file() or not key_exists(UNINSTALL):
                raise RuntimeError("Installation is incomplete")
            if signed:
                verify_signed(exe)
                verify_signed(dest / "unins000.exe")
            if value(REGISTRATION, "Iris") != (CAPABILITIES, winreg.REG_SZ):
                raise RuntimeError("Missing RegisteredApplications entry")
            if value(command) != (f'"{exe}" "%1"', winreg.REG_SZ):
                raise RuntimeError("Wrong shell open command")
            for key in OPEN_WITH:
                extension = key.split("\\")[-2]
                if value(CAPABILITIES + r"\FileAssociations", extension) != (PROGID, winreg.REG_SZ):
                    raise RuntimeError(f"Missing application capability for {extension}")
                if value(key, PROGID) != ("", winreg.REG_SZ):
                    raise RuntimeError(f"Missing Open With registration in {key}")
        finally:
            uninstaller = dest / "unins000.exe"
            if uninstaller.is_file():
                run_setup(uninstaller)
            deadline = time.monotonic() + 30
            # Inno delegates to a temporary child. Wait for the complete install tree and registrations.
            while dest.exists() or residue():
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"Uninstall left installation files or registry entries: {dest}, {residue()}")
                time.sleep(0.1)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("installer", type=Path)
    parser.add_argument("--signed", action="store_true", help="Verify installer, app and uninstaller signatures")
    args = parser.parse_args()
    check(args.installer.resolve(), args.signed)
