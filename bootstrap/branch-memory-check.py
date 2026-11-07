#!/usr/bin/env python3
"""Bound compiler text-copy traffic for branch bodies and match-arm lists."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def source(kind, width):
    prefix = 'let identity(s: String): String = s\n'
    payload = 'x' * 128
    if kind == 'if':
        values = ',\n'.join(f'identity("{payload}{i}")' for i in range(width))
        body = '[\n' + values + '\n]'
        # Fixed depth amplifies template-prefix copying without requiring
        # input-sized recursive nesting. This does not prove linearity in depth.
        for _ in range(32):
            body = 'if flag { ' + body + ' } else { ["fallback"] }'
        return (prefix + 'let choose(flag: Bool): Array[String] = ' + body + '\n'
                + f'let main(): Unit = {{ assert_eq(choose(true).len(), {width}); '
                + 'assert_eq(choose(false)[0], "fallback"); println("ok") }\n')
    arms = ''.join(f'{i} ' + ('if n >= 0 ' if i % 2 else '')
                   + f'=> identity("{payload}{i}"),\n' for i in range(width))
    return (prefix + 'let choose(n: Int): String = match n {\n' + arms
            + ' _ => "fallback", }\nlet main(): Unit = {\n'
            + f'for i in 0..{width} {{ assert_eq(choose(i), "{payload}".concat(i.to_string())); }};\n'
            + 'assert_eq(choose(-1), "fallback"); println("ok") }\n')


def main():
    compiler = str(Path(sys.argv[1] if len(sys.argv) > 1 else './sh.real').resolve())
    env = dict(os.environ, PLUM_RT_STATS='1', PLUM_NO_BUILD_CACHE='1')
    env.pop('PLUM_PROFILE', None)
    failed = False
    with tempfile.TemporaryDirectory(prefix='plum-branch-check-') as work:
        for kind in ('if', 'match'):
            for width in (128, 256, 512):
                project = Path(work) / f'{kind}-{width}'
                project.mkdir()
                (project / 'main.plum').write_text(source(kind, width))
                for mode, flags in (('debug', []), ('release', ['--release'])):
                    result = subprocess.run([compiler, 'emit-llvm', str(project), *flags],
                                            capture_output=True, env=env, timeout=180)
                    if result.returncode:
                        sys.stderr.buffer.write(result.stdout + result.stderr)
                        raise RuntimeError(f'{kind} {width} {mode}: emission failed')
                    body = re.search(rb'^define[^\n]*@plum\.f\.\.choose\.0\.\([^\n]*\{(.*?)^\}',
                                     result.stdout, re.M | re.S)
                    branches = len(re.findall(rb'br i1 ', body[1])) if body else 0
                    if branches < (32 if kind == 'if' else width):
                        raise RuntimeError(f'{kind} {width} {mode}: missing branch bodies')
                    stats = re.findall(rb'\[plum-rt\] alloc n=\d+ bytes=(\d+)', result.stderr)
                    if len(stats) != 1:
                        raise RuntimeError('missing or ambiguous compiler allocation counters')
                    allocated = int(stats[0])
                    # At width 512, main requests 74/72 MB for branches and
                    # 110/106 MB for matches; fragments use 50/48 and 48/45 MB.
                    # Other passes still allocate; these bound the selected
                    # range, not the complexity of the whole compiler.
                    if kind == 'if':
                        ceiling = 26_000_000 + width * (70_000 if not flags else 65_000)
                    else:
                        ceiling = 20_000_000 + width * (80_000 if not flags else 70_000)
                    print(f'  {kind} width={width} {mode}: {allocated} bytes (ceiling {ceiling})', flush=True)
                    if allocated > ceiling:
                        failed = True
        if failed:
            raise RuntimeError('branch emission exceeded its allocation budget')
    print('branch-memory-check: branch bodies and match arms stay within allocation budgets')


if __name__ == '__main__':
    main()
