#!/usr/bin/env python3
"""Bound copied liveness continuations through public compiler emission."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def source(width):
    # Every later operand reads the same parameter. The final operand may
    # transfer it, while earlier operands must preserve it for their siblings.
    return ('let identity(s: String): String = s\n'
            + 'let collect(s: String): Array[String] = [\n'
            + ',\n'.join('identity(s)' for _ in range(width)) + '\n]\n'
            + f'let main(): Unit = {{ let xs = collect("shared"); assert_eq(xs.len(), {width}); '
            + 'for x in xs { assert_eq(x,"shared"); }; println("ok") }\n')


def main():
    compiler = str(Path(sys.argv[1] if len(sys.argv) > 1 else './sh.real').resolve())
    env = dict(os.environ, PLUM_RT_STATS='1', PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_PROFILE', None)
    failed = False
    with tempfile.TemporaryDirectory(prefix='plum-liveness-check-') as work:
        for width in (512, 1024, 2048):
            project = Path(work) / str(width)
            project.mkdir()
            (project / 'main.plum').write_text(source(width))
            for mode, flags in (('debug', []), ('release', ['--release'])):
                result = subprocess.run([compiler, 'emit-llvm', str(project), *flags],
                                        capture_output=True, env=env, timeout=180)
                if result.returncode:
                    sys.stderr.buffer.write(result.stdout + result.stderr)
                    raise RuntimeError(f'continuation {width} {mode}: emission failed')
                body = re.search(rb'^define[^\n]*@plum\.f\.\.collect\.0\.\([^\n]*\{(.*?)^\}',
                                 result.stdout, re.M | re.S)
                calls = len(re.findall(rb'call ptr @plum\.f\.\.identity\.0\.\(', body[1])) if body else 0
                if calls != width:
                    raise RuntimeError(f'continuation {width} {mode}: missing sibling expressions')
                stats = re.findall(rb'\[plum-rt\] alloc n=\d+ bytes=(\d+)', result.stderr)
                if len(stats) != 1:
                    raise RuntimeError('missing or ambiguous compiler allocation counters')
                allocated = int(stats[0])
                # Main requests 133/129 MB at width 2048; source ranges use
                # 39/35 MB. Other passes retain their existing costs; this
                # bounds the selected input range, not all compiler complexity.
                ceiling = 24_000_000 + width * (12_000 if not flags else 10_000)
                print(f'  continuation width={width} {mode}: {allocated} bytes (ceiling {ceiling})', flush=True)
                if allocated > ceiling:
                    failed = True
        if failed:
            raise RuntimeError('liveness continuations exceeded their allocation budget')
    print('liveness-memory-check: future operand ranges stay within allocation budgets')


if __name__ == '__main__':
    main()
