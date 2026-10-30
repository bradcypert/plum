#!/usr/bin/env python3
"""Cache inventory counts link records without traversing their targets."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from lsp_test_support import make_symlink, native_path

compiler = str(Path(sys.argv[1]).resolve())

with tempfile.TemporaryDirectory(prefix="plum-cache-tree-") as temporary:
    root = Path(temporary)
    cache = root / "cache"
    package = cache / "pkg" / "v2" / ("a" * 64) / ("b" * 40)
    package.mkdir(parents=True)
    (package / "data").write_bytes(b"data")
    (package / "empty").mkdir()
    outside = root / "outside"
    outside.mkdir()
    (outside / "keep").write_bytes(b"external bytes must not count")
    (cache / "keep").write_bytes(b"other cache kind")
    count, size = 1, 4
    links = {"loop": ".", "dangling": "absent", "external": "../../../../../outside"}
    for name, target in links.items():
        try:
            make_symlink(package / name, target, is_directory=name != "dangling")
        except (OSError, NotImplementedError):
            print("cache-tree: symlinks unavailable")
            break
        else:
            count += 1
            size += len(target.encode("utf-8"))
    # A link before the commit directory must not be followed either.
    try:
        make_symlink(cache / "pkg" / "cycle", ".", is_directory=True)
        make_symlink(cache / "pkg" / ("c" * 40), "../../outside", is_directory=True)
    except (OSError, NotImplementedError):
        pass
    # In-progress and abandoned staging namespaces are not packages.
    for stage in [".partial-reserved", ("e" * 40) + ".partial"]:
        (cache / "pkg" / stage / ("d" * 40)).mkdir(parents=True)
    env = {**os.environ, "PLUM_CACHE": native_path(cache)}
    def run(command, ok=True):
        result = subprocess.run([compiler, "cache", command], env=env,
                                capture_output=True, text=True, encoding="utf-8",
                                errors="surrogateescape", timeout=30)
        assert (result.returncode == 0) == ok, result.stdout + result.stderr
        return result.stdout + result.stderr
    expected = f"1 package(s), {count} file(s), {size} bytes"
    listed = run("list")
    assert expected in listed, (expected, listed)
    cleaned = run("clean")
    assert expected in cleaned, (expected, cleaned)
    assert not (cache / "pkg").exists()
    assert (cache / "keep").read_bytes() == b"other cache kind"
    assert (outside / "keep").read_bytes() == b"external bytes must not count"
    assert "0 package(s), 0 file(s), 0 bytes" in run("list")
    # Malformed directory layouts fail at a bounded depth, without recursion
    # through a package tree or silently returning partial totals.
    nested = cache / "pkg"
    nested.mkdir()
    for _ in range(130):
        nested = nested / "d"
        nested.mkdir()
    assert "depth exceeds 128" in run("list", ok=False)

print("cache-tree: bounded inventory, link sizes and cleanup passed")
