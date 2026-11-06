#!/usr/bin/env python3
"""Check opt-in cell accounting, active-worker snapshots and sanitizer coexistence."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
compiler = str(Path(sys.argv[1] if len(sys.argv) > 1 else './sh.real').resolve())
fixture = root / 'bootstrap/exec_corpus/memory_stats'
enabled = 'memory diagnostics enabled: bounded ownership, reuse, capacity and threaded snapshots pass\n'


def main():
    with tempfile.TemporaryDirectory(prefix='plum-memory-check-') as work:
        env = dict(os.environ, PLUM_CACHE=str(Path(work) / 'cache'), PLUM_CACHE_TRACE='1',
                   ASAN_OPTIONS='detect_leaks=1:halt_on_error=1', LSAN_OPTIONS='exitcode=23')
        env.pop('PLUM_CC', None)

        def run(args, *, fails=False, contains=None):
            r = subprocess.run([str(a) for a in args], cwd=root, env=env, capture_output=True, text=True, timeout=300)
            if (r.returncode != 0) != fails or (contains and contains not in r.stdout + r.stderr):
                raise RuntimeError(f'{args}: exit {r.returncode}\n{r.stdout[-6000:]}\n{r.stderr[-6000:]}')
            return r

        # The ordinary corpus independently asserts None/disabled behavior.
        for flags in [[], ['--release']]:
            binary = Path(work) / 'tracked.exe'
            run([compiler, 'build', fixture, '--memory-stats', *flags, '-o', binary])
            if run([binary]).stdout != enabled:
                raise RuntimeError('incorrect tracked fixture output')
        for tracking, hit in [(False, False), (True, False), (True, True), (False, True)]:
            flags = ['--memory-stats'] if tracking else []
            r = run([compiler, 'run', fixture, *flags])
            expected = enabled if tracking else 'memory diagnostics disabled\n'
            if r.stdout != expected or ('IR hit' in r.stderr) != hit:
                raise RuntimeError('memory mode reused the wrong IR/cache configuration:\n' + r.stdout + r.stderr)
        # No sanitizer fallback on unsupported platforms. Supported native
        # Linux/macOS must detect leaks even while the registry indexes cells.
        if sys.platform.startswith('linux') or sys.platform == 'darwin':
            for name, fails, message in [('memory_stats', False, enabled), ('tracked_leak', True, 'LeakSanitizer: detected memory leaks')]:
                source = fixture if name == 'memory_stats' else root / 'bootstrap/sanitizer_corpus/tracked_leak'
                binary = Path(work) / (name + ".exe")
                run([compiler, 'build', source, '--memory-stats', '--sanitize', 'address', '-o', binary])
                run([binary], fails=fails, contains=message)
        # The ordinary registry shim has no table, lock or TLS; emitted cell
        # allocation remains direct malloc, and its release wrapper is inline.
        plain = run([compiler, 'emit-llvm', fixture]).stdout
        tracked = run([compiler, 'emit-llvm', fixture, '--memory-stats']).stdout
        if 'call ptr @plum_heap_alloc' in plain or 'call ptr @plum_heap_alloc' not in tracked:
            raise RuntimeError('ordinary/tracked allocation boundaries were not separated')
    print('memory-stats-check: bounded ownership, aliases, capacity/reuse, table churn, frozen/threaded snapshots, cache isolation and sanitizer coexistence passed')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        sys.exit(str(error))
