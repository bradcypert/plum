#!/usr/bin/env python3
"""Check consuming padding, checked sizes, final releases and public ownership."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('compiler', nargs='?', default='./sh.real')
    ap.add_argument('--sanitize', action='store_true')
    args = ap.parse_args()
    root = Path(__file__).resolve().parent.parent
    compiler = str(Path(args.compiler).resolve())
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1', ASAN_OPTIONS='detect_leaks=1:halt_on_error=1', LSAN_OPTIONS='exitcode=23')
    env.pop('PLUM_CC', None)

    def run(command, *, error=None):
        r = subprocess.run([str(x) for x in command], cwd=root, env=env,
                           capture_output=True, text=True, encoding='utf-8', timeout=300)
        if r.returncode != (1 if error else 0) or (error and r.stderr != error):
            raise RuntimeError(f'{command}: exit {r.returncode}\n{r.stdout}\n{r.stderr}')
        return r.stdout

    with tempfile.TemporaryDirectory(prefix='plum-string-pad-') as directory:
        work = Path(directory)
        fixture = root / 'bootstrap/exec_corpus/pad_storage'
        for tracked in (False, True):
            flags = ['--memory-stats'] if tracked else []
            if args.sanitize:
                flags += ['--sanitize=address']
            for release in (False, True):
                binary = work / 'public.exe'
                run([compiler, 'build', fixture, *flags, *(['--release'] if release else []), '-o', binary])
                if run([binary]) != (fixture / 'expected.txt').read_text(encoding='utf-8'):
                    raise RuntimeError('incorrect padding fixture output')
            ir = run([compiler, 'emit-llvm', fixture, *flags])
            definitions = []
            for name in ['plum_str_pad_length', 'plum_str_pad', 'plum_str_repeat_into',
                         'plum_str_scaled_length', 'plum_str_added_length', 'plum_alloc_preferred',
                         'plum_alloc_preferred_or_fail', 'plum_rc_inc', 'plum_rel_str', 'plum_free']:
                match = re.search(r'^define [^\n]*@' + name + r'\([^\n]*\).*?^}', ir, re.M | re.S)
                if not match:
                    raise RuntimeError('missing emitted padding dependency: ' + name)
                body = match[0]
                if name == 'plum_alloc_preferred_or_fail':
                    if body.count('call ptr @malloc(') != 2:
                        raise RuntimeError('preferred-allocation seam changed')
                    body = body.replace('call ptr @malloc(', 'call ptr @pad_test_malloc(')
                definitions.append(body)
            constants = re.findall(r'^@(?:plum_[nb]_alloc|plum_n_str|\.plum_(?:pad_size_error|concat_alloc_error)) = .*$', ir, re.M)
            declarations = '''declare ptr @pad_test_malloc(i64)
declare ptr @memcpy(ptr, ptr, i64)
declare ptr @memmove(ptr, ptr, i64)
declare void @free(ptr)
declare i64 @malloc_usable_size(ptr)
declare void @plum_memory_fatal(ptr, i64) noreturn
declare { i64, i1 } @llvm.sadd.with.overflow.i64(i64, i64)
declare { i64, i1 } @llvm.smul.with.overflow.i64(i64, i64)
'''
            if tracked:
                declarations += 'declare void @plum_heap_adopt_origin(ptr, i64, i64)\ndeclare void @plum_heap_free(ptr)\n'
            source = work / 'runtime.ll'
            source.write_text(declarations + '\n'.join(constants + definitions), encoding='utf-8')
            for optimization in ('-O0', '-O2'):
                binary = work / 'native.exe'
                command = ['clang', optimization, '-pthread', '-Wno-override-module']
                if tracked:
                    command += ['-DPLUM_HEAP_STATS']
                if args.sanitize:
                    command += ['-fsanitize=address', '-fno-omit-frame-pointer']
                run(command + [source, root / 'bootstrap/string-pad-test.c', root / 'native_stdlib/memory_shim.c',
                               root / 'native_stdlib/compat_shim.c', '-o', binary])
                run([binary])
                for case in ['source-negative', 'source-large', 'fill-negative', 'fill-large', 'left-negative',
                             'right-negative', 'count-overflow', 'product-overflow', 'header-overflow',
                             'combined-overflow', 'combined-signed-overflow', 'empty-fill', 'oom']:
                    message = 'cannot allocate storage' if case == 'oom' else 'invalid padding size'
                    run([binary, case], error=f'Plum String: {message}\n')
                print(f'string-pad: {optimization} tracked={tracked} passed', flush=True)


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
