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


def make_symlink(path, target, is_directory=False):
    """Create native Windows links, including from MSYS Python fixtures."""
    import os
    if os.name == "nt":
        path.symlink_to(str(target).replace("/", "\\"), target_is_directory=is_directory)
    elif shutil.which("cygpath"):
        # MSYS may encode links using its own reparse representation. Exercise
        # the native relative symlink contract used by ordinary Windows tools.
        args = ["cmd.exe", "/d", "/c", "mklink"]
        if is_directory:
            args.append("/D")
        target_text = native_path(target) if Path(target).is_absolute() else str(target)
        args.extend([native_path(path).replace("/", "\\"), target_text.replace("/", "\\")])
        result = subprocess.run(args, capture_output=True,
                                env={**os.environ, "MSYS2_ARG_CONV_EXCL": "*"})
        if result.returncode:
            raise OSError(result.stdout + result.stderr)
    else:
        path.symlink_to(target, target_is_directory=is_directory)
