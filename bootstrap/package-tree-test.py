#!/usr/bin/env python3
"""Independent hash vectors and real Git fetch/verification regression tests."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

compiler = Path(sys.argv[1]).resolve()

def digest(root, version):
    records = []
    for parent, dirs, files in os.walk(root, followlinks=False):
        dirs[:] = [name for name in dirs if name != '.git']
        for name in dirs + files:
            if name == '.git':
                continue
            path = Path(parent) / name
            relative = path.relative_to(root).as_posix().encode()
            if path.is_symlink():
                if version == 1:
                    raise ValueError('legacy links unsupported')
                kind, payload = b'L', os.readlink(path).encode()
            elif path.is_dir():
                if version == 1:
                    continue
                kind, payload = b'D', b''
            else:
                data = path.read_bytes()
                if version == 1:
                    framed = str(len(relative)).encode() + b'\n' + relative + b'\n' + str(len(data)).encode() + b'\n' + data
                    kind, payload = b'F', hashlib.sha256(framed).digest()
                else:
                    kind, payload = b'F', hashlib.sha256(data).digest()
            records.append((relative, kind, payload))
    hash_ = hashlib.sha256()
    if version == 2:
        hash_.update(b'plum-tree-v2\n')
    for relative, kind, payload in sorted(records):
        if version == 2:
            hash_.update(kind + str(len(relative)).encode() + b'\n' + relative + b'\n' + str(len(payload)).encode() + b'\n')
        hash_.update(payload)
    return hash_.hexdigest()

with tempfile.TemporaryDirectory(prefix='plum-package-tree-') as scratch:
    root = Path(scratch)
    remote, project, cache = root / 'remote', root / 'consumer', root / 'cache'
    remote.mkdir(); project.mkdir()
    (remote / 'library').mkdir()
    (remote / 'library/value.plum').write_text('pub let value (): Int = 42\n')
    (remote / 'assets').mkdir()
    (remote / 'assets/data.bin').write_bytes(bytes(range(256)) * 1024)
    (project / 'main.plum').write_text('let main (): Unit = ()\n')
    env = dict(os.environ, PLUM_CACHE=str(cache),
               GIT_AUTHOR_NAME='Plum Test', GIT_AUTHOR_EMAIL='test@example.invalid',
               GIT_COMMITTER_NAME='Plum Test', GIT_COMMITTER_EMAIL='test@example.invalid')
    def command(args, expected=None):
        result = subprocess.run([str(a) for a in args], env=env, capture_output=True, text=True, timeout=120)
        output = result.stdout + result.stderr
        if expected is None:
            assert result.returncode == 0, output
        else:
            assert result.returncode != 0 and expected in output, (result.returncode, output, expected)
        return output
    def git(*args):
        return command(['git', '-C', remote, *args]).strip()
    git('init', '--quiet')
    def commit():
        git('add', '.')
        git('commit', '--quiet', '-m', 'fixture')
        return git('rev-parse', 'HEAD')
    revision = commit()
    def manifest(extra='', policy=''):
        (project / 'plum.pkg').write_text(
            'Package { name: "consumer"' + policy + ', deps: [Dep { name: "library", git: '
            + json.dumps(str(remote)) + ', rev: ' + json.dumps(revision) + extra + '}] }\n')
    def fetch():
        output = command([compiler, 'fetch', project])
        assert 'hash_version: 2' in output, output
        found = re.search(r'sha256: ([0-9a-f]{64})', output)
        assert found and found.group(1) == digest(remote, 2), output
        return found.group(1)
    manifest()
    modern = fetch()
    manifest(', hash_version: 2, sha256: ' + json.dumps(modern))
    command([compiler, 'check', project])
    manifest(', sha256: ' + json.dumps(digest(remote, 1)))
    command([compiler, 'check', project])
    manifest(', hash_version: 3')
    command([compiler, 'fetch', project], '`hash_version` must be 1 or 2')
    manifest('', ', symlinks: "unknown"')
    command([compiler, 'fetch', project], '`symlinks` must be `within-root` or `reject`')
    for policy in ('within-root', 'reject'):
        manifest('', ', symlinks: ' + json.dumps(policy))
        command([compiler, 'check', project])
    try:
        (remote / 'asset-link').symlink_to('assets/data.bin')
    except OSError:
        print('package-tree: symlink fixtures skipped (creation unavailable)')
    else:
        (remote / 'dangling').symlink_to('absent')
        revision = commit()
        manifest()
        modern = fetch()
        cached = next(p for p in cache.rglob(revision) if p.is_dir())
        assert os.readlink(cached / 'asset-link') == 'assets/data.bin'
        assert os.readlink(cached / 'dangling') == 'absent'
        manifest(', hash_version: 2, sha256: ' + json.dumps(modern))
        command([compiler, 'vendor', project])
        vendored = next(p for p in (project / 'vendor').rglob(revision) if p.is_dir())
        assert os.readlink(vendored / 'asset-link') == 'assets/data.bin'
        assert digest(vendored, 2) == modern
        (vendored / 'empty').mkdir()
        command([compiler, 'check', project], 'does not match its `sha256`')
        (vendored / 'empty').rmdir()
        manifest(', sha256: ' + json.dumps('0' * 64))
        command([compiler, 'fetch', project], 'legacy package hash refuses symlink')
        (remote / 'escape').symlink_to('../outside')
        revision = commit()
        manifest()
        command([compiler, 'fetch', project], 'refused package link escape')
        assert not list(cache.rglob('*.partial')), 'failed fetch retained staging output'
        (remote / 'escape').unlink()
        (remote / 'absolute').symlink_to(str(remote / 'assets/data.bin'))
        revision = commit()
        manifest()
        command([compiler, 'fetch', project], 'refused package link absolute')
        assert not list(cache.rglob('*.partial'))
print('package-tree: legacy/v2 hashes, manifest policies, links, and publication validation verified')
