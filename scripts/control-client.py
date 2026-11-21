#!/usr/bin/env python3
"""Send newline-delimited commands to Iris's same-user local control endpoint.

Use a debug binary or a release binary built with --features devtools, set its
IRIS_CONTROL, then run:
  python scripts/control-client.py ENDPOINT "state" "get path"
With no command arguments, commands are read from stdin. Replies are written to
Iris's stderr, not returned over this write-only control connection.
"""

import argparse
import ctypes
import os
import socket
import sys
import time


class ControlClient:
    MAX_LINE = 16 * 1024

    def __init__(self, endpoint, timeout=10, process=None):
        self._socket = None
        self._handle = None
        self._kernel = None
        deadline = time.monotonic() + timeout
        if os.name == "nt":
            from ctypes import wintypes

            if not endpoint.startswith("\\\\.\\pipe\\iris-"):
                raise ValueError("Windows endpoint must start with \\\\.\\pipe\\iris-")
            self._kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            self._kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD,
                                                wintypes.DWORD, ctypes.c_void_p,
                                                wintypes.DWORD, wintypes.DWORD,
                                                wintypes.HANDLE]
            self._kernel.CreateFileW.restype = wintypes.HANDLE
            self._kernel.WriteFile.argtypes = [wintypes.HANDLE, ctypes.c_void_p,
                                              wintypes.DWORD, ctypes.POINTER(wintypes.DWORD),
                                              ctypes.c_void_p]
            self._kernel.WriteFile.restype = wintypes.BOOL
            self._kernel.CloseHandle.argtypes = [wintypes.HANDLE]
            self._kernel.CloseHandle.restype = wintypes.BOOL
        while True:
            if process is not None and process.poll() is not None:
                raise RuntimeError(f"Iris exited before control connected (exit {process.returncode})")
            try:
                if os.name == "nt":
                    # Identification-only SQOS prevents a server from impersonating us.
                    handle = self._kernel.CreateFileW(endpoint, 0x40000000, 0, None,
                                                      3, 0x00100000 | 0x00010000, None)
                    if handle == ctypes.c_void_p(-1).value:
                        error = ctypes.get_last_error()
                        if error not in (2, 231):  # Not created yet, or previous client disconnecting.
                            raise ctypes.WinError(error)
                        raise FileNotFoundError(error, "control pipe is not ready", endpoint)
                    self._handle = handle
                else:
                    stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    try:
                        stream.settimeout(max(0.01, deadline - time.monotonic()))
                        stream.connect(endpoint)
                        stream.settimeout(None)
                    except BaseException:
                        stream.close()
                        raise
                    self._socket = stream
                return
            except (FileNotFoundError, ConnectionRefusedError, TimeoutError) as error:
                if time.monotonic() >= deadline:
                    raise TimeoutError(f"control endpoint not ready: {endpoint}") from error
                time.sleep(min(0.05, max(0, deadline - time.monotonic())))

    def send(self, command):
        if "\n" in command or "\r" in command:
            raise ValueError("send exactly one command without a newline")
        data = (command + "\n").encode("utf-8")
        if len(data) > self.MAX_LINE:
            raise ValueError("control command exceeds 16 KiB including newline")
        if self._socket is not None:
            self._socket.sendall(data)
        elif self._handle is not None:
            from ctypes import wintypes

            offset = 0
            while offset < len(data):
                chunk = data[offset:]
                written = wintypes.DWORD()
                if not self._kernel.WriteFile(self._handle, chunk, len(chunk),
                                               ctypes.byref(written), None):
                    raise ctypes.WinError(ctypes.get_last_error())
                if written.value == 0:
                    raise BrokenPipeError("control pipe accepted no bytes")
                offset += written.value
        else:
            raise ValueError("control connection is closed")

    def close(self):
        if self._socket is not None:
            self._socket.close()
            self._socket = None
        if self._handle is not None:
            self._kernel.CloseHandle(self._handle)
            self._handle = None

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("endpoint", help="Unix socket path or Windows \\\\.\\pipe\\iris-... name")
    parser.add_argument("commands", nargs="*", help="each argument is one complete command")
    parser.add_argument("--timeout", type=float, default=10, help="connection deadline in seconds")
    args = parser.parse_args()
    with ControlClient(args.endpoint, timeout=args.timeout) as client:
        for command in args.commands or (line.rstrip("\r\n") for line in sys.stdin):
            client.send(command)


if __name__ == "__main__":
    main()
