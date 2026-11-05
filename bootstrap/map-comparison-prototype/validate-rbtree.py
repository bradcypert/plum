#!/usr/bin/env python3
"""Run after run.py --sanitize; use its generated sources and ASan compiler wrapper."""
import os, pathlib, subprocess, sys
compiler, generated = sys.argv[1:]
root = pathlib.Path(__file__).resolve().parent
env = dict(os.environ, PLUM_CC=str(pathlib.Path(generated).resolve()/'sanitize-cc'), ASAN_OPTIONS='detect_leaks=1')
for project in sorted(pathlib.Path(generated).glob('rbtree-*')):
    if project.name.endswith('-invariants'): continue
    source = (project/'main.plum').read_text().split('extern "C"')[0]
    dest = project.parent/(project.name+'-invariants')
    dest.mkdir(exist_ok=True)
    (dest/'main.plum').write_text(source+(root/'rbtree_validation.plum').read_text().replace('VALUE', project.name.split('-')[2]))
    binary = dest/'check'
    subprocess.run([compiler, 'build', str(dest), '--release', '--native-deps', 'none', '-o', str(binary)], env=env, check=True, timeout=120)
    subprocess.run([str(binary)], env=env, check=True, timeout=60)
    print(project.name, 'invariants passed', flush=True)
