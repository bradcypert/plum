#!/usr/bin/env python3
"""Exercise formatter traversal and publication against real linked trees."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from lsp_test_support import make_symlink

compiler = Path(sys.argv[1]).resolve()
WINDOWS_HOST = os.name == "nt" or sys.platform.startswith(("msys", "cygwin"))
original = "let main (): Unit = {\nprintln(\"hello\")\n}\n"
formatted = "let main (): Unit = {\n    println(\"hello\")\n}\n"


def run(path, *flags, ok=True):
    # Preserve arbitrary POSIX filename bytes in diagnostics while comparing
    # source text and ASCII diagnostic messages. Windows paths are UTF-8.
    result = subprocess.run([str(compiler), "fmt", *flags, str(path)],
                            capture_output=True, text=True, encoding="utf-8", errors="surrogateescape", timeout=30)
    assert (result.returncode == 0) == ok, result.stdout + result.stderr
    return result.stdout + result.stderr


with tempfile.TemporaryDirectory(prefix="plum-format-tree-") as temp:
    root = Path(temp)
    project = root / "project-üni"
    project.mkdir()
    source = project / "main.plum"
    source.write_text(original)
    hardlink = root / "hardlink"
    os.link(source, hardlink)
    if not WINDOWS_HOST:
        source.chmod(0o640)
    assert run(source) == formatted
    run(source, "--check", ok=False)
    run(source, "--write")
    assert source.read_text() == formatted
    assert hardlink.read_text() == original
    if not WINDOWS_HOST:
        assert source.stat().st_mode & 0o777 == 0o640
    run(source, "--check")
    for directory in [".hidden", "vendor"]:
        (project / directory).mkdir()
        (project / directory / "file.plum").write_text(original)
    run(project, "--write")
    for directory in [".hidden", "vendor"]:
        assert (project / directory / "file.plum").read_text() == formatted

    # A second visible path to the same file must fail before either write.
    source.write_text(original)
    duplicate = project / "duplicate.plum"
    os.link(source, duplicate)
    assert "duplicate filesystem identity" in run(project, "--write", ok=False)
    assert source.read_text() == original and duplicate.read_text() == original
    duplicate.unlink()

    target = project / "payload"
    target.write_text(original)
    alias = project / "alias.plum"
    try:
        make_symlink(alias, "payload")
    except OSError:
        print("formatter-tree: symlinks unavailable on this runner")
    else:
        run(alias, "--write")
        assert alias.is_symlink() and os.readlink(alias) == "payload"
        assert target.read_text() == formatted
        target.write_text(original)
        manifest = project / "plum.pkg"
        manifest.write_text('Package { name: "test", symlinks: "reject" }')
        run(alias, "--write", ok=False)
        assert target.read_text() == original
        subdir = project / "module"
        subdir.mkdir()
        nested_alias = subdir / "nested.plum"
        make_symlink(nested_alias, "../payload")
        run(nested_alias, "--write", ok=False)
        run(subdir, "--write", ok=False)
        assert target.read_text() == original
        manifest.write_text('Package { name: "test", symlinks: "within-root" }')
        run(nested_alias, "--write")
        assert target.read_text() == formatted and nested_alias.is_symlink()
        nested_alias.unlink()
        subdir.rmdir()
        manifest.unlink()
        alias.unlink()
        outside = root / "outside"
        outside.write_text(original)
        for link_target in ["../outside", str(outside), "missing"]:
            make_symlink(alias, link_target)
            run(alias, "--write", ok=False)
            assert outside.read_text() == original and alias.is_symlink()
            alias.unlink()
        loop = project / "loop"
        make_symlink(loop, ".", is_directory=True)
        assert "duplicate filesystem identity" in run(project, "--write", ok=False)
        assert source.read_text() == original
        loop.unlink()
    assert not list(project.rglob(".plum-write-*"))

print("formatter-tree: confinement, alias policy and target replacement passed")
