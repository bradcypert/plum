#!/usr/bin/env python3
"""Bound compiler allocation traffic for wide fresh and reused array literals."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def main():
    compiler = str(Path(sys.argv[1] if len(sys.argv) > 1 else './sh.real').resolve())
    env = dict(os.environ, PLUM_RT_STATS='1', PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_PROFILE', None)
    failed = False
    with tempfile.TemporaryDirectory(prefix='plum-emission-check-') as work:
        for kind in ('fresh', 'reuse'):
            for width in (128, 256, 512):
                project = Path(work) / f'{kind}-{width}'
                project.mkdir()
                values = ',\n'.join('identity("' + 'x' * 256 + str(i) + '")' for i in range(width))
                source = 'let identity (s: String): String = s\n'
                if kind == 'fresh':
                    # A returned literal has no destination slot to offer for
                    # reuse. A let binding can take the reuse path even on its
                    # first assignment, with a null old cell.
                    source += 'let made (): Array[String] = [\n' + values + '\n]\n'
                    source += 'let main (): Unit = { let xs = made(); '
                    function = b'plum_made'
                    expected_call = b'call ptr @plum_array_new('
                else:
                    source += 'let main (): Unit = { let mut xs = [identity("seed")];\nxs = [\n' + values + '\n];\n'
                    function = b'plum_main'
                    expected_call = b'call ptr @plum_array_reuse('
                source += f'assert_eq(xs.len(), {width}); println("ok") }}\n'
                (project / 'main.plum').write_text(source)
                for mode, flags in (('debug', []), ('release', ['--release'])):
                    result = subprocess.run([compiler, 'emit-llvm', str(project), *flags],
                                            env=env, capture_output=True, timeout=120)
                    if result.returncode:
                        sys.stderr.buffer.write(result.stdout + result.stderr)
                        raise RuntimeError(f'{kind} {width} {mode}: emission failed')
                    body = re.search(rb'^define[^\n]*@' + function + rb'\([^\n]*\{(.*?)^\}',
                                     result.stdout, re.M | re.S)
                    if body is None or expected_call not in body[1]:
                        raise RuntimeError(f'{kind} {width} {mode}: fixture did not exercise its literal path')
                    stats = re.findall(rb'\[plum-rt\] alloc n=\d+ bytes=(\d+)', result.stderr)
                    if len(stats) != 1:
                        raise RuntimeError('missing or ambiguous compiler allocation counters')
                    allocated = int(stats[0])
                    # Allow fixed prelude work and generous per-element cost.
                    # The old prefix copies use 122-161 MB at width 512.
                    ceiling = 20_000_000 + width * 100_000
                    print(f'  literal {kind} width={width} {mode}: {allocated} bytes (ceiling {ceiling})', flush=True)
                    if allocated > ceiling:
                        failed = True
        if failed:
            raise RuntimeError('wide literal emission exceeded its allocation budget')
    print('emission-memory-check: fresh and reused literals stay within allocation budgets')


if __name__ == '__main__':
    main()
