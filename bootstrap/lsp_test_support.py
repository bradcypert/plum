"""Paths shared by Python LSP clients and the native compiler under test."""
import shutil
import subprocess
from pathlib import Path


def native_path(path):
    # MSYS/Cygwin paths work in Python's filesystem calls, but paths
    # embedded in JSON are not translated when a native process reads them.
    # Match lsp-smoke's cygpath conversion; keep Python's Path for file I/O.
    cygpath = shutil.which("cygpath")
    if cygpath:
        return subprocess.check_output([cygpath, "-m", str(path)], text=True).strip()
    return Path(path).as_posix()


def file_uri(path):
    native = native_path(path)
    if len(native) >= 3 and native[1:3] == ":/":
        return "file:///" + native
    # Keep the current server's URI contract; percent encoding is #160.
    return "file://" + native
