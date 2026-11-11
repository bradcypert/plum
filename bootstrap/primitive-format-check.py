#!/usr/bin/env python3
"""Check emitted primitive formatting, compact storage, scratch and ownership."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


def extract(ir):
    names = ['plum_int_to_string', 'plum_float_to_string', 'plum_float_fixed_raw',
             'plum_fmt_double', 'plum_bool_to_string', 'plum_str_new', 'plum_alloc',
             'plum_free', 'plum_rel_str', 'plum_rc_inc']
    if re.search(r'^define ptr @plum_alloc_origin\(', ir, re.M):
        names.append('plum_alloc_origin')
    definitions = []
    for name in names:
        match = re.search(r'^define [^\n]*@' + name + r'\([^\n]*\).*?^}', ir, re.M | re.S)
        if not match:
            raise RuntimeError('missing emitted formatter dependency: ' + name)
        definitions.append(match[0])
    constants = re.findall(r'^@(?:\.plum_(?:fmt_(?:int|fixed|s)|nan_text|true|false)|plum_[nb]_alloc|plum_n_str) = .*$', ir, re.M)
    declarations = '''declare ptr @malloc(i64)
declare void @free(ptr)
declare ptr @memcpy(ptr, ptr, i64)
declare i32 @snprintf(ptr, i64, ptr, ...)
declare i32 @plum_format_g(ptr, i64, i32, double)
declare double @strtod(ptr, ptr)
declare ptr @plum_heap_alloc(i64)
declare ptr @plum_heap_alloc_origin(i64, i64)
declare void @plum_heap_free(ptr)
'''
    # LLVM's private i1 interface is not a C ABI. Cross through i64 explicitly.
    bridge = '''define ptr @format_test_bool(i64 %b) {
entry:
  %value = icmp ne i64 %b, 0
  %text = call ptr @plum_bool_to_string(i1 %value)
  ret ptr %text
}
; Obtain the runtime's exact external symbol, bypassing C header redirects.
define ptr @format_test_runtime_snprintf() {
entry:
  ret ptr @snprintf
}
'''
    return declarations + '\n'.join(constants + definitions) + '\n' + bridge


def direct_result_prototype(source):
    # Test-only alternative: format directly into a maximum-sized result cell.
    # It removes the copy but retains 32/64/512 bytes for even tiny values.
    for name, cap in [('plum_int_to_string', 32), ('plum_float_to_string', 64), ('plum_float_fixed_raw', 512)]:
        match = re.search(r'^define [^\n]*@' + name + r'\([^\n]*\).*?^}', source, re.M | re.S)
        body = match[0]
        allocation = '  %cell = call ptr @plum_alloc' + ('_origin' if '@plum_alloc_origin(' in source else '')
        allocation += f'(i64 {cap + 16}' + (', i64 1' if '@plum_alloc_origin(' in source else '') + ')\n'
        replacement = body.replace(f'  %buf = alloca [{cap} x i8], align 16\n', allocation + '  %buf = getelementptr i8, ptr %cell, i64 16\n')
        replacement = replacement.replace('  %s = call ptr @plum_str_new(ptr %buf, i64 %len)\n  ret ptr %s',
                                          '  %lp = getelementptr i8, ptr %cell, i64 8\n  store i64 %len, ptr %lp\n  ret ptr %cell')
        if replacement == body or '%s = call ptr @plum_str_new' in replacement:
            raise RuntimeError('direct-result prototype no longer matches emitted formatter')
        source = source.replace(body, replacement)
    return source


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('compiler', nargs='?', default='./sh.real')
    ap.add_argument('--sanitize', action='store_true')
    ap.add_argument('--before', help='also measure the previous heap-scratch compiler and the direct-result alternative')
    ap.add_argument('--out', help='write measurement JSON (also validates every result)')
    args = ap.parse_args()
    root = Path(__file__).resolve().parent.parent
    compiler = str(Path(args.compiler).resolve())
    env = dict(os.environ, PLUM_NO_BUILD_CACHE='1', ASAN_OPTIONS='detect_leaks=1:halt_on_error=1', LSAN_OPTIONS='exitcode=23')
    env.pop('PLUM_CC', None)

    def run(command):
        result = subprocess.run([str(x) for x in command], cwd=root, env=env,
                                capture_output=True, text=True, encoding='utf-8', timeout=300)
        if result.returncode:
            raise RuntimeError(f'{command}: exit {result.returncode}\n{result.stdout}\n{result.stderr}')
        return result.stdout

    rows = []
    with tempfile.TemporaryDirectory(prefix='plum-primitive-format-') as directory:
        work = Path(directory)
        fixture = root / 'bootstrap/exec_corpus/primitive_format_storage'
        for tracked in (False, True):
            flags = ['--memory-stats'] if tracked else []
            if args.sanitize:
                flags += ['--sanitize=address']
            for release in (False, True):
                binary = work / 'public.exe'
                run([compiler, 'build', fixture, *flags, *(['--release'] if release else []), '-o', binary])
                if run([binary]) != (fixture / 'expected.txt').read_text(encoding='utf-8'):
                    raise RuntimeError('incorrect primitive-format fixture output')
            actual = extract(run([compiler, 'emit-llvm', fixture, *flags]))
            designs = [('after', actual, 0)]
            if args.before:
                before = str(Path(args.before).resolve())
                designs = [('before', extract(run([before, 'emit-llvm', fixture, *flags])), 1),
                           ('after', actual, 0), ('direct-result-prototype', direct_result_prototype(actual), 2)]
            for label, source, design in designs:
                # Count emitted runtime requests, not registry indexing or libc's
                # internal work. Do not substitute any formatting algorithm.
                for name in ['malloc', 'free', 'plum_heap_alloc_origin', 'plum_heap_free']:
                    replacement = 'format_test_' + name.removeprefix('plum_')
                    source = source.replace('@' + name + '(', '@' + replacement + '(')
                ir = work / 'runtime.ll'
                ir.write_text(source, encoding='utf-8')
                for optimization in ('-O0', '-O2'):
                    binary = work / 'native.exe'
                    command = ['clang', optimization, '-Wno-override-module', '-pthread', f'-DFORMAT_DESIGN={design}']
                    if tracked:
                        command += ['-DPLUM_HEAP_STATS']
                    if args.sanitize:
                        command += ['-fsanitize=address', '-fno-omit-frame-pointer']
                    run(command + [ir, root / 'bootstrap/primitive-format-test.c',
                                   root / 'native_stdlib/memory_shim.c', root / 'native_stdlib/compat_shim.c', '-o', binary])
                    output = run([binary, '--measure'])
                    records = [dict(json.loads(line), design=label, optimization=optimization, tracked=tracked) for line in output.splitlines()]
                    if len(records) != 6:
                        raise RuntimeError('missing primitive-format measurement')
                    rows.extend(records)
                    print(f'primitive-format: {label} {optimization} tracked={tracked} passed', flush=True)
    if args.out:
        digest = lambda path: hashlib.sha256(Path(path).read_bytes()).hexdigest()
        compilers = {'after': compiler}
        if args.before:
            compilers['before'] = str(Path(args.before).resolve())
        metadata = {'compilers': {name: {'path': path, 'sha256': digest(path)} for name, path in compilers.items()},
                    'harness_sha256': digest(__file__), 'probe_sha256': digest(root / 'bootstrap/primitive-format-test.c'),
                    'sanitized': args.sanitize}
        Path(args.out).write_text(json.dumps({'method': 'Actual emitted formatters; test-only request/release seams include heap scratch omitted by Plum counters. Excludes registry-index allocations and libc internals. Requested traffic, result live bytes and isolated conversion heap peak; not RSS or total process heap. Direct-result is a throwaway LLVM prototype, not production.', 'metadata': metadata, 'records': rows}, indent=2) + '\n', encoding='utf-8')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        raise SystemExit(str(error))
