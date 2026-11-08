#!/usr/bin/env python3
"""Guard actual compiler symbol atoms: lazy ASCII path and owned fragments."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile


def probe(kind, steps):
    if kind == 'dense':
        setup = f'''let mut name = "$.";
    let mut expected = "$24$2e";
    for i in 0..{steps} {{ name = name.concat(name); expected = expected.concat(expected); }};'''
    else:
        setup = f'''let mut prefix = "aZ_09";
    for i in 0..{steps} {{ prefix = prefix.concat(prefix); }};
    let name = ''' + ('prefix;' if kind == 'ascii' else 'prefix.concat(".").concat(prefix);') + '\n    let expected = ' + (
            'prefix;' if kind == 'ascii' else 'prefix.concat("$2e").concat(prefix);')
    return f'''
let main (): Unit = {{
    {setup}
    for i in 0..100 {{ assert_eq(cg_symbol_atom(name), expected); }};
    println("symbol atoms: ok")
}}
'''


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('compiler', nargs='?', default='./sh.real')
    ap.add_argument('--sanitize', action='store_true')
    args = ap.parse_args()
    root = Path(__file__).resolve().parent.parent
    compiler = str(Path(args.compiler).resolve())
    implementation = (root / 'bootstrap/self_host/codegen/symbol_atoms.plum').read_text(encoding='utf-8')
    # The encoder's defensive ASCII-run error is supplied by codegen in
    # production. Keep that same fail-closed behavior in this isolated build.
    implementation += '\nlet cg_fail (message: String): Unit = panic_raw(message)\n'
    env = dict(os.environ, PLUM_RT_STATS='1', PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_PROFILE', None)
    env.pop('PLUM_CC', None)
    if args.sanitize:
        env.update(ASAN_OPTIONS='detect_leaks=1:halt_on_error=1:symbolize=1', LSAN_OPTIONS='exitcode=23')
    with tempfile.TemporaryDirectory(prefix='plum-symbol-check-') as work:
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
            if result.returncode or result.stdout != 'symbol atoms: ok\n':
                raise RuntimeError(f'{stem}: failed ({result.returncode})\n{result.stdout}{result.stderr}')
            return result

        for mode, flags in [('debug', []), ('release', ['--release'])]:
            if args.sanitize:
                flags = flags + ['--sanitize=address']
                build_run('\nlet main (): Unit = { test_symbol_atom_bytes(); println("symbol atoms: ok") }\n',
                          'bytes-' + mode, flags)
            for kind in ('ascii', 'mixed', 'dense'):
                for steps in (5, 8, 11):
                    width = 1 << steps
                    result = build_run(probe(kind, steps), f'{kind}-{mode}-{width}', flags)
                    stats = re.findall(r'\[plum-rt\] alloc n=(\d+) bytes=(\d+)', result.stderr)
                    if len(stats) != 1:
                        raise RuntimeError('missing or ambiguous allocation counters')
                    allocations, allocated = map(int, stats[0])
                    if kind == 'ascii':
                        # Only geometric input setup; hex data is shared.
                        count_limit, ceiling = 32, 4_000 + 15 * width
                    elif kind == 'mixed':
                        count_limit = 2000
                        ceiling = 50_000 + 50 * width + 100 * (30 * width + 512)
                    else:
                        # Dense escapes must reuse the owned fragment array,
                        # rather than copying it on each successive push.
                        count_limit = 4000
                        ceiling = 20_000 + 100 * (400 * width + 2048)
                    print(f'  symbol {kind} width={width} {mode}: {allocations} allocations, {allocated} bytes (ceiling {ceiling})', flush=True)
                    if allocations > count_limit or allocated > ceiling:
                        raise RuntimeError('symbol encoding exceeded its allocation budget')
    print('symbol-encoding-check: byte identities, ownership and allocation budgets passed')


if __name__ == '__main__':
    main()
