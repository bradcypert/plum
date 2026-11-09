#!/usr/bin/env python3
"""Guard integer parsing values, raw-unit diagnostics and bounded allocations."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

SOURCE = '''extern "C" {
    fn parse_before() -> Unit;
    fn parse_after() -> Unit;
}
let main (): Unit = {
    let kind = args()[0].parse_int().unwrap_or(0);
    let n = args()[1].parse_int().unwrap_or(0);
    let reps = args()[2].parse_int().unwrap_or(1);
    let text = if kind == 0 { "123456789" }
        else if kind == 1 { "0".repeat(n) }
        else if kind == 2 { "-".concat("0".repeat(n)).concat("7") }
        else if kind == 3 { "0".repeat(n).concat("é") }
        else { "9".repeat(n) };
    unsafe { parse_before(); };
    for i in 0..reps {
        match String.parse_int(text) {
            Ok(value) => {
                assert(kind < 3);
                assert_eq(value, if kind == 0 { 123456789 } else if kind == 2 { -7 } else { 0 })
            },
            Err(message) => {
                assert(kind >= 3);
                assert_eq(message, if kind == 3 { "expected a digit, found é" } else { "integer out of range" })
            },
        };
    };
    unsafe { parse_after(); };
    assert_eq(text.len(), if kind == 0 { 9 } else if kind == 2 || kind == 3 { n + 2 } else { n })
}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('compiler', nargs='?', default='./sh.real')
    parser.add_argument('--sanitize', action='store_true')
    parser.add_argument('--measure-only', action='store_true', help='record validated workloads without budgets')
    parser.add_argument('--out', type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    compiler = str(Path(args.compiler).resolve())
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1',
               ASAN_OPTIONS='detect_leaks=1:halt_on_error=1', LSAN_OPTIONS='exitcode=23')
    env.pop('PLUM_CC', None)
    cases = [(0, 9, 1000), (1, 512, 10), (1, 4096, 10), (1, 65536, 1),
             (2, 4096, 10), (3, 4096, 10), (4, 65536, 1)]
    rows = []
    def run(command):
        result = subprocess.run([str(x) for x in command], cwd=root, env=env,
                                capture_output=True, text=True, timeout=300)
        if result.returncode:
            raise RuntimeError(f'{command}: exit {result.returncode}\n{result.stdout}\n{result.stderr}')
        return result.stdout
    with tempfile.TemporaryDirectory(prefix='plum-parse-int-') as temp:
        work = Path(temp)
        project = work / 'bench'
        project.mkdir()
        (project / 'main.plum').write_text(SOURCE, encoding='utf-8')
        for mode, mode_flags in [('debug', []), ('release', ['--release'])]:
            sanitizer = ['--sanitize=address'] if args.sanitize else []
            binary = work / 'app.exe'
            if not args.measure_only:
                fixture = root / 'bootstrap/exec_corpus/parse_int_bytes'
                for tracked in (False, True):
                    flags = mode_flags + sanitizer + (['--memory-stats'] if tracked else [])
                    run([compiler, 'build', fixture, *flags, '-o', binary])
                    if run([binary]) != (fixture / 'expected.txt').read_text():
                        raise RuntimeError('incorrect integer oracle/alias output')
            run([compiler, 'build', project, *mode_flags, *sanitizer, '--memory-stats',
                 '--link-c', root / 'bootstrap/parse-int-observer.c', '-o', binary])
            for kind, length, reps in cases:
                sample = json.loads(run([binary, kind, length, reps]))
                row = dict(mode=mode, kind=kind, length=length, reps=reps, **sample)
                rows.append(row)
                print(row, flush=True)
                if args.measure_only:
                    continue
                count_limit = (3 if kind == 3 else 1) * reps + 1
                byte_limit = (128 if kind == 3 else 40) * reps + 64
                if (sample['allocations'] > count_limit or sample['allocated_bytes'] > byte_limit
                        or sample['live_bytes'] != 0 or sample['peak_bytes'] > max(sample['previous_peak_bytes'], sample['previous_live_bytes'] + 512)):
                    raise RuntimeError('integer parser exceeded its allocation/live/peak budget')
    if args.out:
        args.out.write_text(json.dumps(dict(
            method='Validated public parse_int calls, allocation-free snapshots, input held live. '
                   'Traffic/live are deltas; peak is whole-process, including input construction; before snapshot precedes the first measured parse.',
            results=rows), indent=2) + '\n')
    if args.measure_only:
        print('parse-int-measure: workload values verified; budgets not applied')
    else:
        print('parse-int-check: values, diagnostics, aliases and bounded allocation passed')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
