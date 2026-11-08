#!/usr/bin/env python3
"""Bound compiler text copying for wide expression, binding and assignment blocks."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def source(kind, width):
    prefix = ('let identity(s: String): String = s\n'
              + 'let observe(s: String): Unit = assert_eq(s,"41-shared")\n')
    if kind == 'expr':
        body = 'observe(s);\n' * width
    elif kind == 'let':
        body = ''.join(f'let x{i} = identity(s);\n' for i in range(width))
        body += f'assert_eq(x0,s); assert_eq(x{width - 1},s);\n'
    else:
        body = 'let mut current=s;\n'
        body += 'current={ let temporary=identity(current); temporary };\n' * width
        body += 'assert_eq(current,s);\n'
    return (prefix + 'let visit(s: String): Unit = {\n' + body
            + '}\nlet main(): Unit = { visit(41.to_string().concat("-shared")); println("ok") }\n')


def main():
    compiler = str(Path(sys.argv[1] if len(sys.argv) > 1 else './sh.real').resolve())
    env = dict(os.environ, PLUM_RT_STATS='1', PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_PROFILE', None)
    failed = False
    with tempfile.TemporaryDirectory(prefix='plum-block-check-') as work:
        for kind in ('expr', 'let', 'assign'):
            for width in (256, 512, 1024):
                project = Path(work) / f'{kind}-{width}'
                project.mkdir()
                (project / 'main.plum').write_text(source(kind, width))
                for mode, flags in (('debug', []), ('release', ['--release'])):
                    result = subprocess.run([compiler, 'emit-llvm', str(project), *flags],
                                            capture_output=True, env=env, timeout=180)
                    if result.returncode:
                        sys.stderr.buffer.write(result.stdout + result.stderr)
                        raise RuntimeError(f'{kind} block {width} {mode}: emission failed')
                    body = re.search(rb'^define[^\n]*@plum\.f\.\.visit\.0\.\([^\n]*\{(.*?)^\}',
                                     result.stdout, re.M | re.S)
                    callee = b'observe' if kind == 'expr' else b'identity'
                    calls = len(re.findall(rb'call (?:i1|ptr) @plum\.f\.\.' + callee + rb'\.0\.\(', body[1])) if body else 0
                    if calls != width:
                        raise RuntimeError(f'{kind} block {width} {mode}: missing statements')
                    stats = re.findall(rb'\[plum-rt\] alloc n=\d+ bytes=(\d+)', result.stderr)
                    if len(stats) != 1:
                        raise RuntimeError('missing or ambiguous compiler allocation counters')
                    allocated = int(stats[0])
                    # At width 1024, corrected baseline requests 150-673 MB;
                    # collection uses 27-85 MB across these cases. Other
                    # analyses still grow: this bounds the selected range.
                    slopes = {'expr': (20_000, 10_000), 'let': (60_000, 50_000), 'assign': (80_000, 60_000)}
                    ceiling = 24_000_000 + width * slopes[kind][bool(flags)]
                    print(f'  {kind} block width={width} {mode}: {allocated} bytes (ceiling {ceiling})', flush=True)
                    if allocated > ceiling:
                        failed = True
        if failed:
            raise RuntimeError('block emission exceeded its allocation budget')
    print('block-memory-check: statement and function assembly stay within allocation budgets')


if __name__ == '__main__':
    main()
