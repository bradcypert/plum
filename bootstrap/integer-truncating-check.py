#!/usr/bin/env python3
"""Compare integer truncation with an unbounded model in hosted/core builds."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
COMPILER = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / 'sh.real').resolve()
KINDS = ('I8', 'I16', 'I32', 'Int', 'I64', 'U8', 'U16', 'U32', 'U64')
HOOKS = r'''#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
static long live;
extern _Noreturn void plum_kernel_start(void);
void *plum_kernel_alloc(uint64_t n) { void *p = malloc((size_t)n); if (p) ++live; return p; }
void plum_kernel_free(void *p) { if (p) { --live; free(p); } }
void plum_kernel_write(const uint8_t *p, uint64_t n) { fwrite(p, 1, n, stdout); }
_Noreturn void plum_kernel_panic(const uint8_t *p, uint64_t n) { fwrite(p, 1, n, stderr); exit(97); }
_Noreturn void plum_kernel_halt(void) { exit(live == 0 ? 0 : 98); }
int main(void) { plum_kernel_start(); }
'''


def spec(kind):
    bits = 64 if kind == 'Int' else int(kind[1:])
    signed = kind.startswith('I')
    return bits, signed, -(2**(bits - 1)) if signed else 0, 2**(bits - signed) - 1


def main():
    count = 0
    with tempfile.TemporaryDirectory(prefix='plum-truncating-') as tmp:
        work = Path(tmp)
        app = work / 'app'
        app.mkdir()
        source = app / 'main.plum'
        env = dict(os.environ, PLUM_CACHE=str(work / 'cache'))

        def run(*args, ok=True):
            result = subprocess.run(list(map(str, args)), cwd=ROOT, env=env,
                                    capture_output=True, text=True, timeout=300)
            assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
            return result

        functions = ['let apply[S, D] (value: S) (f: (S) -> D): D = f(value)']
        calls = []
        for src in KINDS:
            _, _, minimum, maximum = spec(src)
            for dst in KINDS:
                bits, signed, dst_min, dst_max = spec(dst)
                modulus = 2**bits
                candidates = {minimum, minimum + 1, maximum - 1, maximum, -1, 0, 1,
                              dst_min - 1, dst_min, dst_min + 1,
                              dst_max - 1, dst_max, dst_max + 1,
                              modulus - 1, modulus, modulus + 1,
                              -modulus - 1, -modulus, -modulus + 1,
                              2**63 - 1, 2**63, 2**63 + 1}
                body = []
                for value in sorted(v for v in candidates if minimum <= v <= maximum):
                    expected = value % modulus
                    if signed and expected >= modulus // 2:
                        expected -= modulus
                    body.append(f'{{ let original: {src} = {value}; '
                                f'let expected: {dst} = {expected}; '
                                f'assert_eq({dst}.truncating_from(original), expected); }};')
                    count += 1
                # Route a boundary through a typed closure and a polymorphic
                # function, then retain the result in a container.
                cast = f'cast_{src}_{dst}'
                functions.append(f'let {cast} (x: {src}): {dst} = {dst}.truncating_from(x)')
                body.append(f'let convert: ({src}) -> {dst} = |x| {cast}(x);')
                body.append(f'let input: {src} = {maximum}; '
                            f'let values = [apply(input, convert)]; '
                            f'assert_eq(values[0], {dst}.truncating_from(input));')
                name = f'test_{src}_{dst}'
                functions.append(f'let {name} (): Unit = {{\n' + '\n'.join(body) + '\n()}')
                calls.append(f'{name}();')
        matrix = '\n'.join(functions) + '\nlet main (): Unit = {\n' + '\n'.join(calls) + '\nprintln("matrix ok")\n}\n'

        for expression in ('U8.truncating_from(true)', 'Int.truncating_from("1")',
                           'I8.truncating_from(1.0)', 'U8.truncating_from(())',
                           'U8.truncating_from([1])', 'U8.truncating_from(None)',
                           'Int.truncating_from(|| 1)'):
            source.write_text(f'let main (): Unit = {{ {expression}; () }}\n')
            for action in ('check', 'emit-llvm'):
                result = run(COMPILER, action, app, ok=False)
                assert 'known integer source type' in result.stdout + result.stderr, result
        for expression in ('U8.truncating_from()', 'U8.truncating_from(1, 2)'):
            source.write_text(f'let main (): Unit = {{ {expression}; () }}\n')
            result = run(COMPILER, 'check', app, ok=False)
            assert 'expects 1 argument' in result.stdout + result.stderr, result

        # No implicit source contextualization: the literal is an Int, and the
        # value above Int max must first be annotated as U64.
        source.write_text('let main (): Unit = { U8.truncating_from(18446744073709551615); () }\n')
        run(COMPILER, 'check', app, ok=False)
        source.write_text('let main (): Unit = { let x: U8 = 1; '
                          'let y: Int = U8.truncating_from(x); () }\n')
        run(COMPILER, 'check', app, ok=False)
        source.write_text('let convert[S] (x: S): U8 = U8.truncating_from(x)\n'
                          'let main (): Unit = ()\n')
        result = run(COMPILER, 'check', app, ok=False)
        assert 'known integer source type' in result.stdout + result.stderr, result

        for namespace in KINDS:
            text = f'let main (): Unit = {{ let result = {namespace}.truncating_from(1); () }}\n'
            source.write_text(text)
            column = text.index('.truncating_from') + 2
            members = json.loads(run(COMPILER, 'members', app, source, 1, column).stdout)
            details = {item['name']: item['detail'] for item in members}
            assert {'from', 'try_from', 'truncating_from'} <= details.keys(), details
            hover = json.loads(run(COMPILER, 'query', app, source, 1, column).stdout)
            canonical = 'Int' if namespace == 'I64' else namespace
            assert f'{canonical}.truncating_from' in hover['type'], hover
        names = {item['name'] for item in json.loads(run(COMPILER, 'complete', app).stdout)}
        assert {f'{kind}.truncating_from' for kind in KINDS} <= names, names
        text = 'let main (): Unit = { let n = "value"; n.len(); () }\n'
        source.write_text(text)
        members = json.loads(run(COMPILER, 'members', app, source, 1,
                                 text.index('.len') + 2).stdout)
        assert 'truncating_from' not in {item['name'] for item in members}, members

        # Globals supply their own reachability roots; owned temporaries
        # occur exclusively here to test cleanup without help from main.
        globals_app = work / 'globals'
        globals_app.mkdir()
        (globals_app / 'main.plum').write_text(
            'struct Owner { label: String, value: I8 }\n'
            'let owner (): Owner = Owner { label: "owned".concat("!"), value: -1 }\n'
            'let GLOBAL: U16 = U16.truncating_from(owner().value)\n'
            'let HIGH: U64 = U64.truncating_from(-1)\n'
            'let main (): Unit = { assert_eq(GLOBAL, 65535); '
            'assert_eq(HIGH, 18446744073709551615); println("globals ok"); }\n')
        behavior_app = work / 'behavior'
        behavior_app.mkdir()
        (behavior_app / 'main.plum').write_text(
            'struct Owner { label: String, value: I8 }\n'
            'let owner (calls: Ref[Int]): Owner = { calls.set(calls.get() + 1); '
            'Owner { label: "owned".concat("!"), value: -1 } }\n'
            'let main (): Unit = { let calls = ref(0); '
            'assert_eq(U16.truncating_from(owner(calls).value), 65535); '
            'assert_eq(calls.get(), 1); '
            'let next: () -> Int = || { calls.set(calls.get() + 1); 511 }; '
            'assert_eq(U8.truncating_from(next()), 255); assert_eq(calls.get(), 2); '
            'println("behavior ok"); }\n')
        shadow_app = work / 'shadow'
        shadow_app.mkdir()
        (shadow_app / 'main.plum').write_text(
            'struct Converter { truncating_from: (Int) -> Int }\n'
            'let U8 = Converter { truncating_from: |x| x + 1 }\n'
            'let main (): Unit = { assert_eq(U8.truncating_from(511), 512); println("shadow ok"); }\n')
        # As in integer-conversion-check, namespace shadowing uses a hosted
        # global record; its process-lifetime storage is not a core leak test.
        for mode in ([], ['--release']):
            run(COMPILER, 'build', shadow_app, *mode, '-o', work / 'shadow-test')
            assert run(work / 'shadow-test').stdout == 'shadow ok\n'

        hooks = work / 'hooks.c'
        hooks.write_text(HOOKS)
        source.write_text(matrix)
        for project, expected in ((app, 'matrix ok\n'), (globals_app, 'globals ok\n'),
                                  (behavior_app, 'behavior ok\n')):
            for mode in ([], ['--release']):
                exe = work / 'test'
                run(COMPILER, 'build', project, *mode, '-o', exe)
                assert run(exe).stdout == expected
                obj = work / 'test.o'
                run(COMPILER, 'build', project, '--target', 'x86_64-unknown-none',
                    '--emit-object', *mode, '-o', obj)
                symbols = {line.split()[-1] for line in run('nm', '-u', obj).stdout.splitlines()}
                assert symbols <= {'plum_kernel_alloc', 'plum_kernel_free', 'plum_kernel_write',
                                   'plum_kernel_panic', 'plum_kernel_halt',
                                   'memcpy', 'memmove', 'memset', 'memcmp'}, symbols
                run('clang', '-no-pie', obj, hooks, '-o', exe)
                assert run(exe).stdout == expected
    print(f'integer-truncating-check: {len(KINDS)**2} type/alias pairs, {count} model cases, '
          'diagnostics, tooling and hosted/core debug/release execution pass')


if __name__ == '__main__':
    main()
