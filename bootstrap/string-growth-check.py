#!/usr/bin/env python3
"""Check actual emitted String growth/repetition/hex/join/percent/base64/decode/case/reverse, failure paths, ownership and accounting."""
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
        # Plum text and emitted LLVM are UTF-8, including source comments;
        # Python's default codec on Windows can instead be a legacy code page.
        r = subprocess.run([str(x) for x in command], cwd=root, env=env,
                           capture_output=True, text=True, encoding='utf-8', timeout=300)
        if r.returncode:
            raise RuntimeError(f'{command}: exit {r.returncode}\n{r.stdout}\n{r.stderr}')
        return r.stdout
    with tempfile.TemporaryDirectory(prefix='plum-string-growth-') as work:
        work = Path(work)
        project = root / 'bootstrap/exec_corpus/string_growth'
        for project_name in ('string_growth', 'string_repeat', 'bytes_hex', 'string_join', 'percent_encode', 'base64_encode', 'base64_decode', 'percent_decode', 'hex_decode', 'string_hash', 'string_replace', 'string_case', 'string_reverse'):
            project = root / 'bootstrap/exec_corpus' / project_name
            for release in (False, True):
                for tracked in (False, True):
                    flags = (['--release'] if release else []) + (['--memory-stats'] if tracked else [])
                    if args.sanitize:
                        flags += ['--sanitize=address']
                    binary = work / 'public.exe'
                    run([compiler, 'build', project, *flags, '-o', binary])
                    if run([binary]) != (project / 'expected.txt').read_text(encoding='utf-8'):
                        raise RuntimeError('incorrect public String/Bytes/CStr output')

        for tracked in (False, True):
            flags = (['--memory-stats'] if tracked else []) + (['--sanitize=address'] if args.sanitize else [])
            ir = run([compiler, 'emit-llvm', project, *flags])
            # Extract definitions emitted by this compiler, not a copied IR
            # implementation. Inject failure only at the private malloc seam.
            names = ['plum_str_unit_end', 'plum_str_reverse', 'plum_utf8_len_at', 'plum_utf8_decode', 'plum_utf8_encode', 'plum_utf8_encoded_len', 'plum_str_case_map', 'plum_str_case', 'plum_str_to_upper', 'plum_str_to_lower', 'plum_str_replace_length', 'plum_str_count_matches', 'plum_str_replace', 'plum_str_find', 'plum_hex_pair', 'plum_hex_decoded_length', 'plum_hex_decode_validated', 'plum_percent_decoded_length', 'plum_percent_decode_validated', 'plum_alloc_preferred', 'plum_base64_decode_value', 'plum_base64_decoded_length', 'plum_base64_decode_validated', 'plum_base64_length', 'plum_base64_group', 'plum_bytes_base64_encode', 'plum_str_added_length', 'plum_percent_unreserved', 'plum_percent_length', 'plum_bytes_percent_encode', 'plum_str_join_length', 'plum_str_join', 'plum_str_concat_all', 'plum_str_scaled_length', 'plum_bytes_to_hex', 'plum_str_repeat_length', 'plum_str_repeat', 'plum_str_concat_length',
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
                if name == 'plum_str_case_map':
                    # Use the real platform shim normally; fault cases change
                    # its width between counting and filling deterministically.
                    body = body.replace('@plum_toupper_cp(', '@growth_test_toupper_cp(')
                    body = body.replace('@plum_tolower_cp(', '@growth_test_tolower_cp(')
                definitions.append(body)
            constants = re.findall(r'^@(?:plum_[nb]_alloc|plum_n_concat|plum_n_str|\.plum_(?:concat|repeat|str|hex|percent|base64|b64decode|pctdecode|hexdecode|replace|case|reverse)_[a-z_]+) = .*$', ir, re.M)
            declarations = '''declare i32 @plum_toupper_cp(i32)
declare i32 @plum_tolower_cp(i32)
declare i32 @growth_test_toupper_cp(i32)
declare i32 @growth_test_tolower_cp(i32)
declare ptr @growth_test_malloc(i64)
declare ptr @memcpy(ptr, ptr, i64)
declare i32 @memcmp(ptr, ptr, i64)
declare void @free(ptr)
declare i64 @malloc_usable_size(ptr)
declare void @plum_memory_fatal(ptr, i64) noreturn
declare { i64, i1 } @llvm.sadd.with.overflow.i64(i64, i64)
declare { i64, i1 } @llvm.smul.with.overflow.i64(i64, i64)
'''
            if tracked:
                declarations += 'declare void @plum_heap_adopt_origin(ptr, i64, i64)\ndeclare void @plum_heap_free(ptr)\n'
            # The private helper has an LLVM i8 interface, not C's unsigned-char
            # ABI. Darwin's C caller expects the callee to zero-extend its return;
            # LLVM only promises the low eight bits. Widen explicitly in LLVM
            # before crossing into C, without duplicating the conversion logic.
            bridge = '''define i64 @growth_test_hex_pair(i64 %hi, i64 %lo) {
entry:
  %a = trunc i64 %hi to i8
  %b = trunc i64 %lo to i8
  %byte = call i8 @plum_hex_pair(i8 %a, i8 %b)
  %wide = zext i8 %byte to i64
  ret i64 %wide
}
'''
            source = work / 'runtime.ll'
            source.write_text(declarations + '\n'.join(constants + definitions) + '\n' + bridge,
                              encoding='utf-8')
            for optimization in ('-O0', '-O2'):
                binary = work / 'native.exe'
                command = ['clang', optimization, '-g', '-Wno-override-module', '-pthread']
                if tracked:
                    command += ['-DPLUM_HEAP_STATS']
                if args.sanitize:
                    command += ['-fsanitize=address', '-fno-omit-frame-pointer']
                run(command + [source, root / 'bootstrap/string-growth-test.c',
                               root / 'native_stdlib/memory_shim.c', root / 'native_stdlib/compat_shim.c', '-o', binary])
                print(f'{optimization} tracked={tracked}: {run([binary]).strip()}')
                for case in ('reverse-negative', 'reverse-large', 'reverse-oom', 'case-negative', 'case-large', 'case-truncated', 'case-sum-overflow', 'case-header-overflow', 'case-oom', 'case-fill-growth', 'case-fill-short', 'negative', 'negative-b', 'overflow', 'header-overflow', 'concat-negative', 'reuse-overflow', 'oom', 'exact-oom', 'repeat-negative-length', 'repeat-overflow', 'repeat-header-overflow', 'repeat-oom', 'hex-negative-length', 'hex-overflow', 'hex-header-overflow', 'hex-oom', 'join-negative-count', 'join-large-count', 'join-negative-part', 'join-header-overflow', 'join-sum-overflow', 'join-negative-separator', 'join-product-overflow', 'join-separator-header-overflow', 'join-combined-overflow', 'join-oom', 'concat-all-oom', 'percent-negative-length', 'percent-large-length', 'percent-sum-overflow', 'percent-header-overflow', 'percent-oom', 'base64-negative-length', 'base64-large-length', 'base64-product-overflow', 'base64-header-overflow', 'base64-url-header-overflow', 'base64-oom', 'b64decode-negative-input', 'b64decode-large-input', 'b64decode-negative-count', 'b64decode-count-overflow', 'b64decode-short-group', 'b64decode-padding-shape', 'b64decode-extra-padding', 'b64decode-oom', 'pctdecode-negative-input', 'pctdecode-large-input', 'pctdecode-negative-count', 'pctdecode-count-overflow', 'pctdecode-short-group', 'pctdecode-oom', 'hexdecode-negative-input', 'hexdecode-large-input', 'hexdecode-header-overflow', 'hexdecode-odd-input', 'hexdecode-oom', 'replace-negative-input', 'replace-large-input', 'replace-negative-pattern', 'replace-negative-target', 'replace-large-pattern', 'replace-large-target', 'replace-negative-count', 'replace-large-count', 'replace-empty-pattern-count', 'replace-product-overflow', 'replace-header-overflow', 'replace-signed-overflow', 'replace-oom'):
                    r = subprocess.run([str(binary), case], cwd=root, env=env,
                                       capture_output=True, text=True, encoding='utf-8', timeout=30)
                    message = 'cannot allocate storage' if 'oom' in case else 'invalid reversal size' if case.startswith('reverse-') else 'invalid case-conversion size' if case.startswith('case-') else 'invalid replacement size' if case.startswith('replace-') else 'invalid validated hex size' if case.startswith('hexdecode-') else 'invalid validated percent size' if case.startswith('pctdecode-') else 'invalid validated base64 size' if case.startswith('b64decode-') else 'invalid base64 size' if case.startswith('base64-') else 'invalid percent-encoding size' if case.startswith('percent-') else 'invalid hexadecimal size' if case.startswith('hex-') else 'invalid repetition size' if case.startswith('repeat-') else 'invalid concatenation size'
                    prefix = 'Plum Bytes' if case.startswith(('b64decode-', 'pctdecode-', 'hexdecode-')) and 'oom' not in case else 'Plum String'
                    if r.returncode != 1 or r.stderr != f'{prefix}: {message}\n':
                        raise RuntimeError(f'{case}: wrong fatal path ({r.returncode})\n{r.stdout}{r.stderr}')
    print('string-growth-check: public aliases, emitted arithmetic, growth/repetition/hex/join/percent/base64/decode/case/reverse, failure fallback and accounting passed')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
