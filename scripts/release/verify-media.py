#!/usr/bin/env python3
"""Execute the source-built media SDK's own license/configuration APIs.

Runs only on the target Windows/macOS host. This is a release gate, not a media
smoke test: no files, playback, network, window or user mpv configuration is used.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk", type=Path, required=True)
    args = parser.parse_args()
    sdk = args.sdk.resolve()
    manifest = json.loads((sdk / "compliance/build-manifest.json").read_text())
    windows = manifest["target"].startswith("windows")
    if windows != (os.name == "nt"):
        parser.error("Execute this check on the target operating system")
    prefix = sdk / "prefix"
    directory = prefix / ("bin" if windows else "lib")
    cookie = os.add_dll_directory(str(directory)) if windows else None
    result = {"target": manifest["target"], "ffmpeg": {}}
    libraries = []
    try:
        for name in ("avcodec", "avfilter", "avformat", "avutil", "swresample", "swscale"):
            pattern = name + "-*.dll" if windows else "lib" + name + ".dylib"
            paths = list(directory.glob(pattern))
            if len(paths) != 1:
                raise RuntimeError("Expected exactly one native library: " + pattern)
            lib = ctypes.CDLL(str(paths[0]))
            libraries.append(lib)
            license_fn = getattr(lib, name + "_license")
            license_fn.restype = ctypes.c_char_p
            configuration_fn = getattr(lib, name + "_configuration")
            configuration_fn.restype = ctypes.c_char_p
            license_text = license_fn().decode()
            configuration = configuration_fn().decode()
            if license_text != "LGPL version 2.1 or later":
                raise RuntimeError("Unexpected runtime FFmpeg license: " + license_text)
            for required in ("--disable-gpl", "--disable-version3", "--disable-nonfree", "--enable-shared", "--disable-static", "--disable-autodetect"):
                if required not in configuration.split():
                    raise RuntimeError("Missing runtime FFmpeg configuration: " + required)
            if any(flag in configuration for flag in ("--enable-gpl", "--enable-version3", "--enable-nonfree", "--enable-chromaprint", "--enable-libfftw")):
                raise RuntimeError("Disallowed runtime FFmpeg configuration")
            result["ffmpeg"][name] = {"license": license_text, "configuration": configuration}
        paths = list(directory.glob("*mpv*.dll" if windows else "libmpv.dylib"))
        if len(paths) != 1:
            raise RuntimeError("Expected exactly one libmpv library")
        mpv = ctypes.CDLL(str(paths[0]))
        mpv.mpv_create.restype = ctypes.c_void_p
        mpv.mpv_set_option_string.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p]
        mpv.mpv_initialize.argtypes = [ctypes.c_void_p]
        mpv.mpv_get_property_string.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        mpv.mpv_get_property_string.restype = ctypes.c_void_p
        mpv.mpv_free.argtypes = [ctypes.c_void_p]
        mpv.mpv_terminate_destroy.argtypes = [ctypes.c_void_p]
        handle = mpv.mpv_create()
        if not handle:
            raise RuntimeError("mpv_create failed")
        try:
            for name, value in ((b"config", b"no"), (b"vo", b"null"), (b"ao", b"null")):
                if mpv.mpv_set_option_string(handle, name, value) < 0:
                    raise RuntimeError("mpv option failed: " + name.decode())
            if mpv.mpv_initialize(handle) < 0:
                raise RuntimeError("mpv_initialize failed")
            for name in ("mpv-version", "mpv-configuration"):
                value = mpv.mpv_get_property_string(handle, name.encode())
                if not value:
                    raise RuntimeError("mpv runtime property missing: " + name)
                try:
                    result[name] = ctypes.string_at(value).decode()
                finally:
                    mpv.mpv_free(value)
            configuration = result["mpv-configuration"]
            for required in ("-Dgpl=false", "-Dlibmpv=true", "-Dcplayer=false"):
                if required not in configuration:
                    raise RuntimeError("Missing runtime mpv configuration: " + required)
            if windows and "-Dd3d11=enabled" not in configuration:
                raise RuntimeError("Windows libmpv lacks native D3D11")
        finally:
            mpv.mpv_terminate_destroy(handle)
        print(json.dumps(result, indent=2))
    finally:
        if cookie:
            cookie.close()


if __name__ == "__main__":
    main()
