#!/usr/bin/env python3
"""Bound allocations of the actual compiler qualified-name helper."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile


def probe(steps):
    return f'''
let main (): Unit = {{
    let mut prefix = "éa";
    for i in 0..{steps} {{ prefix = prefix.concat(prefix); }};
    let name = prefix.concat(".Leaf");
    for i in 0..100 {{ assert_eq(module_of_qualified(name), prefix); }};
    println("qualified names: ok")
}}
'''


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('compiler', nargs='?', default='./sh.real')
    ap.add_argument('--sanitize', action='store_true')
    args = ap.parse_args()
    root = Path(__file__).resolve().parent.parent
    compiler = str(Path(args.compiler).resolve())
    # Copy the production implementation, not a separately maintained model.
    implementation = (root / 'bootstrap/self_host/typecheck/qualified_names.plum').read_text(encoding='utf-8')
    env = dict(os.environ, PLUM_RT_STATS='1', PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_PROFILE', None)
    env.pop('PLUM_CC', None)
    if args.sanitize:
        env.update(ASAN_OPTIONS='detect_leaks=1:halt_on_error=1:symbolize=1', LSAN_OPTIONS='exitcode=23')
    with tempfile.TemporaryDirectory(prefix='plum-qualified-check-') as work:
        def build_run(source, stem, flags):
            project = Path(work) / stem
            project.mkdir()
            (project / 'main.plum').write_text(implementation + source, encoding='utf-8')
            binary = Path(work) / (stem + '.bin')
            built = subprocess.run([compiler, 'build', project, *flags, '-o', binary],
                                   cwd=root, env=env, capture_output=True, text=True, timeout=180)
            if built.returncode:
                raise RuntimeError(f'{stem}: build failed\n{built.stdout}{built.stderr}')
            result = subprocess.run([binary], env=env, capture_output=True, text=True, timeout=180)
            if result.returncode or result.stdout != 'qualified names: ok\n':
                raise RuntimeError(f'{stem}: failed ({result.returncode})\n{result.stdout}{result.stderr}')
            return result

        for mode, flags in [('debug', []), ('release', ['--release'])]:
            if args.sanitize:
                flags = flags + ['--sanitize=address']
                build_run('\nlet main (): Unit = { test_qualified_name_boundaries(); println("qualified names: ok") }\n',
                          'boundaries-' + mode, flags)
            for steps in (7, 8, 9):
                width = 1 << steps
                result = build_run(probe(steps), f'{mode}-{width}', flags)
                stats = re.findall(r'\[plum-rt\] alloc n=(\d+) bytes=(\d+)', result.stderr)
                if len(stats) != 1:
                    raise RuntimeError('missing or ambiguous allocation counters')
                allocations, allocated = map(int, stats[0])
                # One prefix result per call, plus geometric input construction.
                # The old per-character slices allocate >26k times at width 128.
                ceiling = 20_000 + 100 * (3 * width + 32)
                print(f'  qualified width={width} {mode}: {allocations} allocations, {allocated} bytes (ceiling {ceiling})', flush=True)
                if allocations > 128 or allocated > ceiling:
                    raise RuntimeError('qualified-name scan exceeded its allocation budget')
    print('qualified-name-check: boundaries, ownership and allocation budgets passed')


if __name__ == '__main__':
    main()
