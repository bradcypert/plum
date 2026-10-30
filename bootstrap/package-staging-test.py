#!/usr/bin/env python3
"""Concurrent package operations own their staging directories and cleanup."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from lsp_test_support import native_path

compiler = str(Path(sys.argv[1]).resolve())

with tempfile.TemporaryDirectory(prefix="plum-package-stage-") as temporary:
    root = Path(temporary)
    remote, project, cache = (root / name for name in ("remote", "project", "cache"))
    remote.mkdir()
    project.mkdir()
    (remote / "hello").mkdir()
    (remote / "hello" / "main.plum").write_text('pub let greeting (): String = "hello"\n')
    def git(*args):
        return subprocess.run(["git", "-C", native_path(remote), *args],
                              capture_output=True, check=True, text=True).stdout.strip()
    git("init", "--quiet")
    git("add", ".")
    git("-c", "user.name=Plum test", "-c", "user.email=test@example.invalid",
        "commit", "--quiet", "-m", "fixture")
    rev = git("rev-parse", "HEAD")
    url = native_path(remote).replace("\\", "/")
    def identity(revision):
        framed = f"{len(url.encode())}:{url}{len(revision)}:{revision}"
        return hashlib.sha256(framed.encode()).hexdigest()
    def manifest(revision=rev):
        (project / "plum.pkg").write_text(
            'Package { name: "consumer", deps: [Dep { name: "hello", git: '
            + json.dumps(url) + ', rev: ' + json.dumps(revision) + ' }] }')
    manifest()
    env = {**os.environ, "PLUM_CACHE": native_path(cache)}
    def command(action):
        return [compiler, action, native_path(project)]
    def concurrent(action):
        jobs = [subprocess.Popen(command(action), env=env, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE) for _ in range(4)]
        for job in jobs:
            out, err = job.communicate(timeout=90)
            assert job.returncode == 0, (action, out, err)
    destinations = [cache / "pkg" / "v2" / identity(rev) / rev,
                    project / "vendor" / "v2" / identity(rev) / rev]
    markers = []
    for destination in destinations:
        old_stage = destination.with_name(destination.name + ".partial")
        old_stage.mkdir(parents=True)
        marker = old_stage / "user-owned"
        marker.write_text("leave this alone")
        markers.append(marker)
    concurrent("fetch")
    concurrent("vendor")
    for destination in destinations:
        assert (destination / "hello" / "main.plum").read_text() == (remote / "hello" / "main.plum").read_text()
        assert not (destination / ".owner").exists()
        assert not (destination / ".git").exists()
    for marker in markers:
        assert marker.read_text() == "leave this alone"
    assert not list(cache.rglob(".partial-*"))
    assert not list(project.rglob(".partial-*"))
    # Checkout failure also owns and removes exactly its unique container.
    missing = "f" * 40
    manifest(missing)
    failed = subprocess.run(command("fetch"), env=env, capture_output=True, timeout=90)
    assert failed.returncode != 0 and b"could not check out" in failed.stdout + failed.stderr, (
        "missing commit fetch", failed.returncode, failed.stdout, failed.stderr)
    assert not (cache / "pkg" / "v2" / identity(missing) / missing).exists()
    assert not list(cache.rglob(".partial-*"))
    for marker in markers:
        assert marker.read_text() == "leave this alone"

print("package-staging: concurrent fetch/vendor and owned cleanup passed")
