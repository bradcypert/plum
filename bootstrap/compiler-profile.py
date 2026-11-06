#!/usr/bin/env python3
"""Compare source-built compiler binaries on identical real inputs (Linux)."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import time


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--before', required=True)
    ap.add_argument('--after', required=True)
    ap.add_argument('--before-heap')
    ap.add_argument('--after-heap')
    ap.add_argument('--samples', type=int, default=3)
    ap.add_argument('--out', required=True)
    ap.add_argument('--case', action='append', help='run only this named workload (repeatable)')
    args = ap.parse_args()
    if args.samples < 1:
        ap.error('--samples must be positive')
    if platform.system() != 'Linux':
        ap.error('this harness reports Linux wait4 RSS units and GNU linker heap instrumentation')
    root = Path(__file__).resolve().parent.parent
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    binaries = {label: str(Path(getattr(args, label)).resolve()) for label in ('before', 'after')}
    tracked = {label: str(Path(p).resolve()) for label in binaries if (p := getattr(args, label + '_heap'))}
    cases = {
        'check-compiler': ['check', 'bootstrap/self_host'],
        'emit-compiler': ['emit-llvm', 'bootstrap/self_host', '--release'],
        'check-properties': ['check', 'bootstrap/properties'],
        'build-compiler': ['build', 'bootstrap/self_host', '--release'],
        'build-collections': ['build', 'bootstrap/exec_corpus/collections', '--release'],
    }
    if args.case:
        unknown = set(args.case) - set(cases)
        if unknown:
            ap.error(f'unknown workloads: {sorted(unknown)}')
        cases = {k: v for k, v in cases.items() if k in args.case}
    inputs = sorted(p for folder in ['bootstrap/self_host', 'bootstrap/properties', 'bootstrap/exec_corpus/collections']
                    for p in (root / folder).rglob('*') if p.is_file() and p.suffix in ('.plum', '.pkg', '.c'))
    metadata = {'platform': platform.platform(), 'samples': args.samples, 'harness_sha256': digest(__file__),
                'revision': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip(),
                'binaries': {k: {'path': v, 'sha256': digest(v)} for k, v in binaries.items()},
                'heap_binaries': {k: {'path': v, 'sha256': digest(v)} for k, v in tracked.items()},
                'input_sha256': {str(p.relative_to(root)): digest(p) for p in inputs}, 'cases': cases}
    (out / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    rows = []

    def run(label, case, sample, heap=False):
        stem = f'{case}-{label}-' + ('heap' if heap else str(sample))
        stdout = out / (stem + '.stdout')
        stderr = out / (stem + '.stderr')
        cache = out / 'cache' / stem
        command = [tracked[label] if heap else binaries[label]] + cases[case]
        executable = out / (stem + '.bin')
        if case.startswith('build-'):
            command += ['-o', str(executable)]
        env = dict(os.environ, PLUM_PROFILE='1', PLUM_RT_STATS='1', PLUM_CACHE=str(cache))
        # Ambient tool wrappers would make the experiment compare different tools.
        env.pop('PLUM_CC', None)
        if shutil.which('timeout'):
            command = ['timeout', '600'] + command
        started = time.perf_counter()
        with stdout.open('wb') as so, stderr.open('wb') as se:
            proc = subprocess.Popen(command, cwd=root, env=env, stdout=so, stderr=se)
            _, status, usage = os.wait4(proc.pid, 0)
            proc.returncode = os.waitstatus_to_exitcode(status)
        elapsed = time.perf_counter() - started
        if proc.returncode:
            raise RuntimeError(f'{stem} exited {proc.returncode}: {stderr.read_text()[-2000:]} {stdout.read_text()[-2000:]}')
        text = stderr.read_text()
        stats = re.findall(r'alloc n=(\d+) bytes=(\d+)', text)
        if len(stats) != 1:
            raise RuntimeError(f'{stem}: expected one compiler allocation record: {text[-2000:]}')
        row = {'case': case, 'label': label, 'sample': sample, 'heap': heap,
               'wall_s': elapsed, 'peak_rss_kib': usage.ru_maxrss,
               'user_s': usage.ru_utime, 'system_s': usage.ru_stime,
               'allocations': int(stats[0][0]), 'allocated_bytes': int(stats[0][1]),
               'phases': re.findall(r'^profile: (.+)$', text, re.M),
               'stdout_bytes': stdout.stat().st_size, 'stdout_sha256': digest(stdout)}
        if heap:
            peak, live = re.search(r'compiler-heap: peak_bytes=(\d+) exit_bytes=(\d+)', text).groups()
            row.update(peak_heap_bytes=int(peak), exit_heap_bytes=int(live))
        if case == 'build-compiler':
            subprocess.run([str(executable), 'check', 'bootstrap/exec_corpus/collections'], cwd=root, check=True, capture_output=True, timeout=600)
        if case == 'build-collections':
            result = subprocess.check_output([str(executable)], cwd=root, text=True)
            expected = (root / 'bootstrap/exec_corpus/collections/expected.txt').read_text()
            if result != expected:
                raise RuntimeError(f'{stem}: executable output changed')
        rows.append(row)
        (out / 'results.json').write_text(json.dumps(rows, indent=2) + '\n')
        print(f'{stem}: {elapsed:.3f}s {usage.ru_maxrss / 1024:.1f} MiB', flush=True)

    # Unrecorded-for-summary sample zero warms each compiler/input combination.
    # Alternate order between rounds to reduce systematic ordering bias.
    for sample in range(args.samples + 1):
        for case in cases:
            for label in (('before', 'after') if sample % 2 == 0 else ('after', 'before')):
                run(label, case, sample)
    # Instrumented runs follow all timing measurements; never use their time/RSS.
    for case in cases:
        for label in tracked:
            run(label, case, 0, heap=True)


if __name__ == '__main__':
    main()
