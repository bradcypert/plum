#!/usr/bin/env python3
"""Verify real generated/native sanitizer failures, leak detection and caches."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
compiler = str(Path(sys.argv[1] if len(sys.argv) > 1 else './sh.real').resolve())
fixtures = root / 'bootstrap/sanitizer_corpus'


def main():
    with tempfile.TemporaryDirectory(prefix='plum-sanitizer-check-') as work:
        env = dict(os.environ, ASAN_OPTIONS='detect_leaks=1:halt_on_error=1:symbolize=1',
                   LSAN_OPTIONS='exitcode=23', PLUM_CACHE=str(Path(work) / 'cache'), PLUM_CACHE_TRACE='1')
        env.pop('PLUM_CC', None)

        def run(args, *, fails=False, contains=None, environment=None):
            result = subprocess.run([str(a) for a in args], cwd=root, env=environment or env,
                                    capture_output=True, text=True, timeout=300)
            output = result.stdout + result.stderr
            if (result.returncode != 0) != fails or (contains and contains not in output):
                raise RuntimeError(f'{args}: exit {result.returncode}\n{output[-12000:]}')
            return result

        def build(name, *flags):
            binary = Path(work) / (name + str(build.count))
            build.count += 1
            run([compiler, 'build', fixtures / name, '--sanitize', 'address', *flags, '-o', binary])
            return binary
        build.count = 0

        # The original missing attribute must be observable in generated IR,
        # rather than only testing native malloc/free interceptors.
        for flags in [[], ['--release']]:
            result = run([build('invalid_plum', *flags)], fails=True, contains='heap-use-after-free')
            if not re.search(r'\bin plum_main\b[^\n]*\bmain\.plum:\d+', result.stderr):
                raise RuntimeError('generated Plum failure has no symbolized source line:\n' + result.stderr)
        run([build('invalid_native')], fails=True, contains='heap-buffer-overflow')
        run([build('leak')], fails=True, contains='LeakSanitizer: detected memory leaks')
        run([build('clean')], contains='native=1')
        run([compiler, 'test', fixtures / 'invalid_plum', '--sanitize=address'], fails=True, contains='heap-use-after-free')
        run([compiler, 'test', fixtures / 'clean', '--sanitize', 'address'], contains='1 passed; 0 failed')

        # IR and native-object cache identity includes instrumentation. Use
        # both mode transitions, then a real warm hit in each mode.
        for address, hit in [(False, False), (True, False), (True, True), (False, True)]:
            flags = ['--sanitize=address'] if address else []
            result = run([compiler, 'run', fixtures / 'clean', *flags, '--', '--sanitize', 'child'],
                         contains=f'native={int(address)}\n--sanitize|child\n')
            if ('IR hit' in result.stderr) != hit:
                raise RuntimeError('wrong instrumentation cache identity:\n' + result.stderr)
            if sys.platform.startswith('linux') and hit and 'object hit' not in result.stderr:
                raise RuntimeError('no native object reuse in warmed supported configuration:\n' + result.stderr)

        for source in [fixtures / 'clean', fixtures / 'clean/main.plum']:
            result = run([compiler, 'emit-llvm', source, '--release', '--sanitize=address'])
            definitions = [line for line in result.stdout.splitlines() if line.startswith('define ')]
            if not definitions or any('sanitize_address' not in line or '"frame-pointer"="all"' not in line for line in definitions):
                raise RuntimeError('a generated function bypasses sanitizer instrumentation')
        for bad, message in [(['--sanitize'], 'requires a value'),
                             (['--sanitize=thread'], 'unsupported sanitizer'),
                             (['--sanitize', 'address', '--target', 'aarch64-windows-gnu'], 'requires a native target')]:
            run([compiler, 'build', fixtures / 'clean', *bad], fails=True, contains=message)

        runner = root / 'bootstrap/sanitize-run'
        run([runner, '--plum', compiler, fixtures / 'clean'], contains='native=1')
        run([runner, '--plum', compiler, fixtures / 'leak'], fails=True, contains='LeakSanitizer: detected memory leaks')
        run([runner, '--plum', compiler, fixtures / 'invalid_plum'], fails=True, contains='heap-use-after-free')
        # An ambient disabled detector/zero exit status cannot make findings pass.
        hostile = dict(env, ASAN_OPTIONS='detect_leaks=0:exitcode=0', LSAN_OPTIONS='exitcode=0')
        run([runner, '--plum', compiler, fixtures / 'leak'], fails=True,
            contains='LeakSanitizer: detected memory leaks', environment=hostile)
        # Simulate a driver/runtime that accepts the build but emits no leak
        # diagnostics. The runner must fail its preflight, not run the project.
        broken = Path(work) / 'broken-compiler'
        broken.write_text(os.linesep.join([
            '#!/usr/bin/env python3', 'from pathlib import Path', 'import sys',
            'p=Path(sys.argv[sys.argv.index("-o")+1])',
            'p.write_text(chr(10).join(["#!/bin/sh", "exit 0", ""]))',
            'p.chmod(0o755)', '',
        ]))
        broken.chmod(0o755)
        run([runner, '--plum', broken, fixtures / 'clean'], fails=True, contains='leak detection preflight failed')
    print('sanitizer-check: generated/native access failures, leaks, clean ownership, symbols, cache isolation and fail-closed runner passed')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        sys.exit(str(error))
