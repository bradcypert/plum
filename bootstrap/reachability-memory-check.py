#!/usr/bin/env python3
"""Bound pending-work copying through the compiler's public emission command."""
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

    def emit(project, flags):
        return subprocess.run([compiler, 'emit-llvm', str(project), *flags],
                              capture_output=True, env=env, timeout=180)

    with tempfile.TemporaryDirectory(prefix='plum-reachability-check-') as work:
        for width in (512, 1024, 2048):
            project = Path(work) / str(width)
            project.mkdir()
            source = 'let sink (): Unit = ()\n'
            source += ''.join(f'let f{i} (): Unit = sink()\n' for i in range(width))
            source += 'let main (): Unit = {\n' + ''.join(f'f{i}();\n' for i in range(width)) + '}\n'
            (project / 'main.plum').write_text(source)
            for mode, flags in (('debug', []), ('release', ['--release'])):
                result = emit(project, flags)
                if result.returncode:
                    sys.stderr.buffer.write(result.stdout + result.stderr)
                    raise RuntimeError(f'wide graph {width} {mode}: emission failed')
                names = re.findall(rb'^define[^\n]*@plum\.f\.\.([a-z0-9_]+)\.0\.\(', result.stdout, re.M)
                expected = [b'main'] + [f'f{i}'.encode() for i in range(width)] + [b'sink']
                if names != expected:
                    raise RuntimeError(f'wide graph {width} {mode}: FIFO discovery or duplicate handling changed')
                stats = re.findall(rb'\[plum-rt\] alloc n=\d+ bytes=(\d+)', result.stderr)
                if len(stats) != 1:
                    raise RuntimeError('missing or ambiguous compiler allocation counters')
                allocated = int(stats[0])
                # Main uses 638/513 MB at width 2048; batching uses 454/329 MB.
                # Other analyses still allocate, so this bounds the selected
                # input range rather than proving the full pipeline linear.
                ceiling = (80_000_000 + 230_000 * width if not flags
                           else 64_000_000 + 180_000 * width)
                print(f'  shared-callee graph width={width} {mode}: {allocated} bytes (ceiling {ceiling})', flush=True)
                if allocated > ceiling:
                    failed = True

        # The historical limit counts popped occurrences, not distinct
        # specializations. Array elements avoid a deeply recursive block AST.
        for calls in (10_000, 10_001):
            project = Path(work) / f'fuel-{calls}'
            project.mkdir()
            source = 'let sink (): Int = 0\nlet main (): Unit = { let xs = [\n'
            source += ',\n'.join('sink()' for _ in range(calls))
            source += '\n]; println(xs.len().to_string()) }\n'
            (project / 'main.plum').write_text(source)
            result = emit(project, ['--release'])
            if calls == 10_000:
                if result.returncode:
                    sys.stderr.buffer.write(result.stdout + result.stderr)
                    raise RuntimeError('final permitted occurrence failed before empty-queue return')
            elif result.returncode != 1 or b'monomorphization did not terminate after 10000 instantiations' not in result.stdout + result.stderr:
                raise RuntimeError('over-limit duplicate occurrences lost the existing monomorphization diagnostic')
        if failed:
            raise RuntimeError('wide graph emission exceeded its allocation budget')
    print('reachability-memory-check: FIFO discovery, shared callees and fuel boundaries passed')


if __name__ == '__main__':
    main()
