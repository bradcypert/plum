#!/usr/bin/env python3
"""Check every integer conversion pair against Python's unbounded integer model."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
COMPILER = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / 'sh.real').resolve()
KINDS = {
    'I8': (-128, 127), 'I16': (-32768, 32767), 'I32': (-(2**31), 2**31 - 1),
    'Int': (-(2**63), 2**63 - 1), 'U8': (0, 255), 'U16': (0, 65535),
    'U32': (0, 2**32 - 1), 'U64': (0, 2**64 - 1),
}
HOOKS = r'''#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
static long live;
extern _Noreturn void plum_kernel_start(void);
void *plum_kernel_alloc(uint64_t size) { void *p = malloc((size_t)size); if (p) ++live; return p; }
void plum_kernel_free(void *p) { if (p) { --live; free(p); } }
void plum_kernel_write(const uint8_t *p, uint64_t n) { fwrite(p, 1, n, stdout); }
_Noreturn void plum_kernel_panic(const uint8_t *p, uint64_t n) { fwrite(p, 1, n, stderr); exit(97); }
_Noreturn void plum_kernel_halt(void) { exit(live == 0 ? 0 : 98); }
int main(void) { plum_kernel_start(); }
'''


def main():
    cases = 0
    with tempfile.TemporaryDirectory(prefix='plum-integer-conversions-') as tmp:
        work = Path(tmp)
        app = work / 'app'
        app.mkdir()
        source = app / 'main.plum'
        env = dict(os.environ, PLUM_CACHE=str(work / 'cache'))

        def run(*args, ok=True):
            result = subprocess.run(list(map(str, args)), cwd=ROOT, env=env,
                                    capture_output=True, text=True, timeout=180)
            assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
            return result

        functions, checks = [], []
        for src, (src_min, src_max) in KINDS.items():
            for dst, (dst_min, dst_max) in KINDS.items():
                safe = dst_min <= src_min and src_max <= dst_max
                source.write_text(f'let main (): Unit = {{ let source: {src} = 0; '
                                  f'let converted = {dst}.from(source); () }}\n')
                for action in ('check', 'emit-llvm'):
                    result = run(COMPILER, action, app, ok=safe)
                    if not safe:
                        assert 'not range-preserving' in result.stdout + result.stderr, result
                cast = f'cast_{src}_{dst}'
                functions.append(f'let {cast} (source: {src}): Result[{dst}, IntegerConversionError] = {dst}.try_from(source)')
                candidates = {src_min, src_max, -1, 0, 1, dst_min - 1, dst_min, dst_min + 1,
                              dst_max - 1, dst_max, dst_max + 1, 2**63 - 1, 2**63}
                for value in sorted(v for v in candidates if src_min <= v <= src_max):
                    cases += 1
                    expected_ok = dst_min <= value <= dst_max
                    checks.append(f'{{ let original: {src} = {value}; match {cast}(original) {{')
                    if expected_ok:
                        checks.append(f'Ok(converted) => {{ let expected: {dst} = {value}; assert_eq(converted, expected); }}, Err(_) => panic_raw("{src}->{dst}: unexpected error")')
                    else:
                        src_tag = 'Signed64' if src == 'Int' else ('Signed' + src[1:] if src.startswith('I') else 'Unsigned' + src[1:])
                        dst_tag = 'Signed64' if dst == 'Int' else ('Signed' + dst[1:] if dst.startswith('I') else 'Unsigned' + dst[1:])
                        checks.append(f'Ok(_) => panic_raw("{src}->{dst}: unexpected success"), Err(error) => {{ '
                                      f'assert_eq(error.source, IntegerKind.{src_tag}); '
                                      f'assert_eq(error.destination, IntegerKind.{dst_tag}); }}')
                    checks.append('}; };')
                if safe:
                    for value in (src_min, src_max):
                        checks.append(f'{{ let original: {src} = {value}; let expected: {dst} = {value}; assert_eq({dst}.from(original), expected); }};')
        matrix = '\n'.join(functions) + '\nlet main (): Unit = {\n' + '\n'.join(checks) + '\nprintln("matrix ok")\n}\n'

        for expression in ('U8.from(1)', 'U64.from(1)', 'I16.from(32767)'):
            source.write_text(f'let main (): Unit = {{ {expression}; () }}\n')
            result = run(COMPILER, 'check', app, ok=False)
            assert 'not range-preserving' in result.stdout + result.stderr, result
        for expression in ('U8.try_from(true)', 'Int.try_from("1")', 'I8.try_from(1.0)',
                           'U8.try_from(())', 'U8.try_from([1])', 'Int.from(|| 1)'):
            source.write_text(f'let main (): Unit = {{ {expression}; () }}\n')
            for action in ('check', 'emit-llvm'):
                result = run(COMPILER, action, app, ok=False)
                assert 'known integer source type' in result.stdout + result.stderr, result
        for expression in ('U8.from()', 'U8.try_from(1, 2)'):
            source.write_text(f'let main (): Unit = {{ {expression}; () }}\n')
            result = run(COMPILER, 'check', app, ok=False)
            assert 'expects 1 argument' in result.stdout + result.stderr, result
        # A value named U8 retains ordinary record-field dispatch precedence.
        source.write_text('struct Converter { try_from: (Int) -> Int }\n'
                          'let U8 = Converter { try_from: |x| x + 1 }\n'
                          'let main (): Unit = assert_eq(U8.try_from(1), 2)\n')
        run(COMPILER, 'build', app, '-o', work / 'shadow')
        run(work / 'shadow')

        text = 'let main (): Unit = { let byte: U8 = 1; let wide = I16.from(byte); () }\n'
        source.write_text(text)
        offset = text.index('from(byte)')
        hover = json.loads(run(COMPILER, 'query', app, source, 1, offset + 1).stdout)
        assert 'I16.from' in hover['type'], hover

        hooks = work / 'hooks.c'
        hooks.write_text(HOOKS)
        for project, expected in ((ROOT / 'bootstrap/exec_corpus/integer_conversions',
                                   (ROOT / 'bootstrap/exec_corpus/integer_conversions/expected.txt').read_text()),
                                  (app, 'matrix ok\n')):
            if project == app:
                source.write_text(matrix)
            for mode in ([], ['--release']):
                exe = work / 'test'
                run(COMPILER, 'build', project, *mode, '-o', exe)
                assert run(exe).stdout == expected
                obj = work / 'test.o'
                run(COMPILER, 'build', project, '--target', 'x86_64-unknown-none', '--emit-object', *mode, '-o', obj)
                symbols = {line.split()[-1] for line in run('nm', '-u', obj).stdout.splitlines()}
                assert symbols <= {'plum_kernel_alloc', 'plum_kernel_free', 'plum_kernel_write',
                                   'plum_kernel_panic', 'plum_kernel_halt', 'memcpy', 'memmove', 'memset', 'memcmp'}, symbols
                run('clang', '-no-pie', obj, hooks, '-o', exe)
                assert run(exe).stdout == expected
        completions = json.loads(run(COMPILER, 'complete', app).stdout)
        names = {item['name'] for item in completions}
        assert {'U8.from', 'U8.try_from', 'Int.from', 'I64.try_from'} <= names, names
    print(f'integer-conversion-check: all 64 type pairs, {cases} value boundaries, '
          'single evaluation, errors and hosted/core debug/release execution pass')


if __name__ == '__main__':
    main()
