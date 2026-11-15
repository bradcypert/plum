#!/usr/bin/env python3
"""Compare modular integer arithmetic with Python, hosted and freestanding."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
COMPILER = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / 'sh.real').resolve()


def main():
    with tempfile.TemporaryDirectory(prefix='plum-wrapping-') as directory:
        work = Path(directory)
        project = work / 'app'
        project.mkdir()
        source = project / 'main.plum'
        env = dict(os.environ, PLUM_CACHE=str(work / 'cache'))

        def run(*args, ok=True):
            result = subprocess.run(list(map(str, args)), cwd=ROOT, env=env,
                                    capture_output=True, text=True, timeout=300)
            assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
            return result

        functions = ['let apply[T] (f: (T, T) -> T) (a: T) (b: T): T = f(a, b)']
        calls = []
        count = 0
        for kind in ('I8', 'I16', 'I32', 'Int', 'I64', 'U8', 'U16', 'U32', 'U64'):
            bits = 64 if kind == 'Int' else int(kind[1:])
            signed = kind[0] == 'I'
            modulus = 2 ** bits
            minimum = -(2 ** (bits - 1)) if signed else 0
            maximum = 2 ** (bits - 1) - 1 if signed else modulus - 1
            values = sorted({minimum, minimum + 1, maximum, maximum - 1, 0, 1, 2,
                             -1 if signed else 2 ** (bits - 1)})
            body = []
            for op in ('add', 'sub', 'mul'):
                for a in values:
                    for b in values:
                        result = {'add': a + b, 'sub': a - b, 'mul': a * b}[op] % modulus
                        if signed and result >= modulus // 2:
                            result -= modulus
                        # Keep the generated test split across functions to bound compiler memory.
                        body.append(f'assert_eq({kind}.wrapping_{op}({a}, {b}), {result});')
                        count += 1
            body += [f'let f = {kind}.wrapping_add;',
                     f'let a: {kind} = {maximum}; let b: {kind} = 1;',
                     f'let xs = [apply(f, a, b)]; assert_eq(xs[0], {minimum});']
            functions.append(f'let test_{kind} (): Unit = {{\n' + '\n'.join(body) + '\n()}')
            calls.append(f'test_{kind}();')
        # Each argument is evaluated exactly once, in normal call order.
        functions.append('let tick (r: Ref[Int]): U8 = { r.set(r.get() + 1); Result.unwrap_or(U8.try_from(r.get()), 0) }')
        source.write_text('\n'.join(functions) + '\nlet main (): Unit = {\n' + '\n'.join(calls) +
                          '\nlet r = ref(0); assert_eq(U8.wrapping_sub(tick(r), tick(r)), 255); assert_eq(r.get(), 2); ()\n}\n')
        for mode in ([], ['--release']):
            exe = work / 'hosted'
            run(COMPILER, 'build', project, *mode, '-o', exe)
            run(exe)
        # Reuse the same runtime hook ABI exercised by the fixed-integer harness.
        hooks = work / 'hooks.c'
        hooks.write_text(HOOKS)
        for mode in ([], ['--release']):
            obj = work / 'core.o'
            run(COMPILER, 'build', project, '--target', 'x86_64-unknown-none',
                '--emit-object', *mode, '-o', obj)
            exe = work / 'core'
            run('clang', '-no-pie', obj, hooks, '-o', exe)
            run(exe)
        for kind in ('I8', 'I16', 'I32', 'U8', 'U16', 'U32', 'U64'):
            source.write_text(f'let main (): Unit = {{ let n = 1; let x = {kind}.wrapping_add(n, 1); () }}')
            for action in ('check', 'emit-llvm'):
                result = run(COMPILER, action, project, ok=False)
                assert 'Int' in result.stdout + result.stderr and kind in result.stdout + result.stderr, result
        print(f'integer-wrapping-check: {count} model cases, function values, evaluation and type rejection pass')


HOOKS = r'''#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
static long live;
extern _Noreturn void plum_kernel_start(void);
void *plum_kernel_alloc(uint64_t size) {
    void *p = malloc((size_t)size);
    if (p) ++live;
    return p;
}
void plum_kernel_free(void *p) { if (p) { --live; free(p); } }
void plum_kernel_write(const uint8_t *p, uint64_t len) { fwrite(p, 1, len, stdout); }
_Noreturn void plum_kernel_panic(const uint8_t *p, uint64_t len) {
    fwrite(p, 1, len, stderr); exit(97);
}
_Noreturn void plum_kernel_halt(void) { exit(live == 0 ? 0 : 98); }
int main(void) { plum_kernel_start(); }
'''

if __name__ == '__main__':
    main()
