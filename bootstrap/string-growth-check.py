#!/usr/bin/env python3
"""Check actual emitted String growth/repetition/hex/join/percent/base64/decode, failure paths, ownership and accounting."""
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
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1',
               ASAN_OPTIONS='detect_leaks=1:halt_on_error=1', LSAN_OPTIONS='exitcode=23')
    env.pop('PLUM_CC', None)
    def run(command):
        r = subprocess.run([str(x) for x in command], cwd=root, env=env,
                           capture_output=True, text=True, timeout=300)
        if r.returncode:
            raise RuntimeError(f'{command}: exit {r.returncode}\n{r.stdout}\n{r.stderr}')
        return r.stdout
    with tempfile.TemporaryDirectory(prefix='plum-string-growth-') as work:
        work = Path(work)
        project = root / 'bootstrap/exec_corpus/string_growth'
        for project_name in ('string_growth', 'string_repeat', 'bytes_hex', 'string_join', 'percent_encode', 'base64_encode', 'base64_decode', 'percent_decode'):
            project = root / 'bootstrap/exec_corpus' / project_name
            for release in (False, True):
                for tracked in (False, True):
                    flags = (['--release'] if release else []) + (['--memory-stats'] if tracked else [])
                    if args.sanitize:
                        flags += ['--sanitize=address']
                    binary = work / 'public.exe'
                    run([compiler, 'build', project, *flags, '-o', binary])
                    if run([binary]) != (project / 'expected.txt').read_text():
                        raise RuntimeError('incorrect public String/Bytes/CStr output')

        for tracked in (False, True):
            flags = (['--memory-stats'] if tracked else []) + (['--sanitize=address'] if args.sanitize else [])
            ir = run([compiler, 'emit-llvm', project, *flags])
            # Extract definitions emitted by this compiler, not a copied IR
            # implementation. Inject failure only at the private malloc seam.
            names = ['plum_percent_decoded_length', 'plum_percent_decode_validated', 'plum_alloc_preferred', 'plum_base64_decode_value', 'plum_base64_decoded_length', 'plum_base64_decode_validated', 'plum_base64_length', 'plum_base64_group', 'plum_bytes_base64_encode', 'plum_str_added_length', 'plum_percent_unreserved', 'plum_percent_length', 'plum_bytes_percent_encode', 'plum_str_join_length', 'plum_str_join', 'plum_str_concat_all', 'plum_str_scaled_length', 'plum_bytes_to_hex', 'plum_str_repeat_length', 'plum_str_repeat', 'plum_str_concat_length',
                     'plum_str_growth_bytes', 'plum_str_concat_copy', 'plum_str_concat',
                     'plum_str_concat_reuse', 'plum_rel_str', 'plum_rc_inc', 'plum_free']
            definitions = []
            for name in names:
                m = re.search(r'^define [^\n]*@' + name + r'\([^\n]*\).*?^}', ir, re.M | re.S)
                if not m:
                    raise RuntimeError('missing runtime function: ' + name)
                body = m[0]
                if name == 'plum_alloc_preferred':
                    if body.count('call ptr @malloc(') != 2:
                        raise RuntimeError('allocation seam changed; update fault injection')
                    body = body.replace('call ptr @malloc(', 'call ptr @growth_test_malloc(')
                definitions.append(body)
            constants = re.findall(r'^@(?:plum_[nb]_alloc|plum_n_concat|plum_n_str|\.plum_(?:concat|repeat|str|hex|percent|base64|b64decode|pctdecode)_[a-z_]+) = .*$', ir, re.M)
            declarations = '''declare ptr @growth_test_malloc(i64)
declare ptr @memcpy(ptr, ptr, i64)
declare void @free(ptr)
declare i64 @malloc_usable_size(ptr)
declare void @plum_memory_fatal(ptr, i64) noreturn
declare { i64, i1 } @llvm.sadd.with.overflow.i64(i64, i64)
declare { i64, i1 } @llvm.smul.with.overflow.i64(i64, i64)
'''
            if tracked:
                declarations += 'declare void @plum_heap_adopt_origin(ptr, i64, i64)\ndeclare void @plum_heap_free(ptr)\n'
            source = work / 'runtime.ll'
            source.write_text(declarations + '\n'.join(constants + definitions) + '\n')
            for optimization in ('-O0', '-O2'):
                binary = work / 'native.exe'
                command = ['clang', optimization, '-g', '-Wno-override-module', '-pthread']
                if tracked:
                    command += ['-DPLUM_HEAP_STATS']
                if args.sanitize:
                    command += ['-fsanitize=address', '-fno-omit-frame-pointer']
                run(command + [source, root / 'bootstrap/string-growth-test.c',
                               root / 'native_stdlib/memory_shim.c', '-o', binary])
                print(f'{optimization} tracked={tracked}: {run([binary]).strip()}')
                for case in ('negative', 'negative-b', 'overflow', 'header-overflow', 'concat-negative', 'reuse-overflow', 'oom', 'exact-oom', 'repeat-negative-length', 'repeat-overflow', 'repeat-header-overflow', 'repeat-oom', 'hex-negative-length', 'hex-overflow', 'hex-header-overflow', 'hex-oom', 'join-negative-count', 'join-large-count', 'join-negative-part', 'join-header-overflow', 'join-sum-overflow', 'join-negative-separator', 'join-product-overflow', 'join-separator-header-overflow', 'join-combined-overflow', 'join-oom', 'concat-all-oom', 'percent-negative-length', 'percent-large-length', 'percent-sum-overflow', 'percent-header-overflow', 'percent-oom', 'base64-negative-length', 'base64-large-length', 'base64-product-overflow', 'base64-header-overflow', 'base64-url-header-overflow', 'base64-oom', 'b64decode-negative-input', 'b64decode-large-input', 'b64decode-negative-count', 'b64decode-count-overflow', 'b64decode-short-group', 'b64decode-padding-shape', 'b64decode-extra-padding', 'b64decode-oom', 'pctdecode-negative-input', 'pctdecode-large-input', 'pctdecode-negative-count', 'pctdecode-count-overflow', 'pctdecode-short-group', 'pctdecode-oom'):
                    r = subprocess.run([str(binary), case], cwd=root, env=env,
                                       capture_output=True, text=True, timeout=30)
                    message = 'cannot allocate storage' if 'oom' in case else 'invalid validated percent size' if case.startswith('pctdecode-') else 'invalid validated base64 size' if case.startswith('b64decode-') else 'invalid base64 size' if case.startswith('base64-') else 'invalid percent-encoding size' if case.startswith('percent-') else 'invalid hexadecimal size' if case.startswith('hex-') else 'invalid repetition size' if case.startswith('repeat-') else 'invalid concatenation size'
                    prefix = 'Plum Bytes' if case.startswith(('b64decode-', 'pctdecode-')) and 'oom' not in case else 'Plum String'
                    if r.returncode != 1 or r.stderr != f'{prefix}: {message}\n':
                        raise RuntimeError(f'{case}: wrong fatal path ({r.returncode})\n{r.stdout}{r.stderr}')
    print('string-growth-check: public aliases, emitted arithmetic, growth/repetition/hex/join/percent/base64/decode, failure fallback and accounting passed')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
