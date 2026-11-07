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
origin_enabled = 'memory origins: coherent provenance, reuse, growth, aliases, peaks and workers\n'


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
        for source, expected in [(fixture, enabled), (root / 'bootstrap/exec_corpus/memory_origins', origin_enabled)]:
            for flags in [[], ['--release']]:
                binary = Path(work) / 'tracked.exe'
                run([compiler, 'build', source, '--memory-stats', *flags, '-o', binary])
                if run([binary]).stdout != expected:
                    raise RuntimeError('incorrect tracked fixture output')
        # Run the generated-input law with diagnostics actually enabled,
        # alongside its disabled-mode check in the ordinary property gate.
        props = run([compiler, 'test', root / 'bootstrap/properties', '--memory-stats'],
                    contains='test_memory_origin_conservation ... ok')
        if '0 failed' not in props.stdout:
            raise RuntimeError('tracked properties failed:\n' + props.stdout + props.stderr)
        for tracking, hit in [(False, False), (True, False), (True, True), (False, True)]:
            flags = ['--memory-stats'] if tracking else []
            r = run([compiler, 'run', fixture, *flags])
            expected = enabled if tracking else 'memory diagnostics disabled\n'
            if r.stdout != expected or ('IR hit' in r.stderr) != hit:
                raise RuntimeError('memory mode reused the wrong IR/cache configuration:\n' + r.stdout + r.stderr)
        # No sanitizer fallback on unsupported platforms. Supported native
        # Linux/macOS must detect leaks even while the registry indexes cells.
        if sys.platform.startswith('linux') or sys.platform == 'darwin':
            for name, fails, message in [('memory_stats', False, enabled), ('memory_origins', False, origin_enabled), ('tracked_leak', True, 'LeakSanitizer: detected memory leaks')]:
                source = root / ('bootstrap/sanitizer_corpus' if fails else 'bootstrap/exec_corpus') / name
                binary = Path(work) / (name + ".exe")
                run([compiler, 'build', source, '--memory-stats', '--sanitize', 'address', '-o', binary])
                run([binary], fails=fails, contains=message)
        # The ordinary registry shim has no table, lock or TLS; emitted cell
        # allocation remains direct malloc, and its release wrapper is inline.
        plain = run([compiler, 'emit-llvm', fixture]).stdout
        tracked = run([compiler, 'emit-llvm', fixture, '--memory-stats']).stdout
        if 'call ptr @plum_heap_alloc' in plain or 'call ptr @plum_heap_alloc' not in tracked:
            raise RuntimeError('ordinary/tracked allocation boundaries were not separated')
        if 'call ptr @plum_alloc(i64' in tracked or '@plum_alloc_origin' in plain:
            raise RuntimeError('fresh generated cells bypass origin attribution or ordinary mode gained it')
    print('memory-stats-check: bounded ownership, origins, aliases, capacity/reuse, table churn, coherent/frozen/threaded snapshots, tracked properties, cache isolation and sanitizer coexistence passed')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        sys.exit(str(error))
