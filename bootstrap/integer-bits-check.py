#!/usr/bin/env python3
"""Model integer bit operations and helper bindings, hosted and freestanding."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
COMPILER = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / 'sh.real').resolve()
KINDS = ('I8', 'I16', 'I32', 'Int', 'I64', 'U8', 'U16', 'U32', 'U64')
NEW_KINDS = ('I8', 'I16', 'I32', 'U8', 'U16', 'U32', 'U64')
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


def normalize(value, bits, signed):
    value %= 2**bits
    return value - 2**bits if signed and value >= 2**(bits - 1) else value


def main():
    cases = 0
    with tempfile.TemporaryDirectory(prefix='plum-integer-bits-') as tmp:
        work = Path(tmp)
        app = work / 'app'
        app.mkdir()
        source = app / 'main.plum'
        hooks = work / 'hooks.c'
        hooks.write_text(HOOKS)
        env = dict(os.environ, PLUM_CACHE=str(work / 'cache'))

        def run(*args, ok=True):
            result = subprocess.run(list(map(str, args)), cwd=ROOT, env=env,
                                    capture_output=True, text=True, timeout=300)
            assert (result.returncode == 0) == ok, (args, result.returncode, result.stdout, result.stderr)
            return result

        def build(project, mode, core=False):
            exe = work / 'test'
            if core:
                obj = work / 'test.o'
                run(COMPILER, 'build', project, '--target', 'x86_64-unknown-none',
                    '--emit-object', *mode, '-o', obj)
                symbols = {line.split()[-1] for line in run('nm', '-u', obj).stdout.splitlines()}
                assert symbols <= {'plum_kernel_alloc', 'plum_kernel_free', 'plum_kernel_write',
                                   'plum_kernel_panic', 'plum_kernel_halt',
                                   'memcpy', 'memmove', 'memset', 'memcmp'}, symbols
                run('clang', '-no-pie', obj, hooks, '-o', exe)
            else:
                run(COMPILER, 'build', project, *mode, '-o', exe)
            return exe

        def reject(text, message):
            source.write_text(text)
            for action in ('check', 'emit-llvm'):
                result = run(COMPILER, action, app, ok=False)
                assert message in result.stdout + result.stderr, result

        functions = ['let combine[T] (a: T) (b: T): T = (a & b) | (a ^ b)',
                     'let complement[T] (a: T): T = ~a',
                     'let shift[T] (a: T) (n: Int): T = a << n']
        calls = []
        for kind in KINDS:
            bits, signed, minimum, maximum = spec(kind)
            values = sorted({minimum, minimum + 1, maximum, maximum - 1, 0, 1, 2,
                             normalize(0x55, bits, signed), normalize(0xAA, bits, signed),
                             normalize(2**(bits - 1), bits, signed),
                             normalize(-1, bits, signed)})
            counts = sorted({0, 1, bits - 1, bits, bits + 1, 63, 64, 65, 2**63 - 1})
            body = []
            for a in values:
                body += [f'{{ let a: {kind} = {a};',
                         f'assert_eq(~a, {normalize(~a, bits, signed)});',
                         f'assert_eq({kind}.count_ones(a), {(a % 2**bits).bit_count()});']
                cases += 2
                for b in values:
                    for op, value in (('&', a & b), ('|', a | b), ('^', a ^ b)):
                        body.append(f'{{ let b: {kind} = {b}; assert_eq(a {op} b, {normalize(value, bits, signed)}); }};')
                        cases += 1
                for n in counts:
                    left = 0 if n >= bits else normalize(a << n, bits, signed)
                    right = (-1 if signed and a < 0 else 0) if n >= bits else a >> n
                    logical = a if n == 0 else (0 if n >= bits else (a % 2**bits) >> n)
                    body += [f'assert_eq(a << {n}, {left});', f'assert_eq(a >> {n}, {right});',
                             f'assert_eq({kind}.shr_logical(a, {n}), {logical});']
                    cases += 3
                for n in (-1, -(2**63)):
                    body.append(f'assert_eq({kind}.shr_logical(a, {n}), a);')
                    cases += 1
                body.append('};')
            body += [f'let a: {kind} = 1; let b: {kind} = 2;',
                     'let count: Int = 1; let values = [combine(a, b), shift(a, count)];',
                     'assert_eq(values[0], 3); assert_eq(values[1], 2);',
                     'assert_eq(complement(a), ~a);',
                     f'let flip: ({kind}) -> {kind} = |x| ~x; assert_eq(flip(a), ~a);',
                     f'let literal: {kind} = ~0; assert_eq(literal, {normalize(-1, bits, signed)});',
                     f'let shifted: {kind} = 1 << 1; assert_eq(shifted, 2);',
                     f'let nested: {kind} = (1 << 1) | (1 & 1); assert_eq(nested, 3);',
                     f'let reverse: {kind} = ~0 & a; assert_eq(reverse, 1);']
            body += [f'let later = |x| {{ let out = x << 1; let typed: {kind} = x; out }};',
                     'assert_eq(later(a), 2);',
                     'let deferred = |x| x >> 1;',
                     f'let high: {kind} = {maximum}; assert_eq(deferred(high), {maximum >> 1});']
            functions.append(f'let test_{kind} (): Unit = {{\n' + '\n'.join(body) + '\n()}')
            calls.append(f'test_{kind}();')
        matrix = '\n'.join(functions) + '\nlet main (): Unit = {\n' + '\n'.join(calls) + '\nprintln("matrix ok")\n}\n'

        # Already typed operands never widen or mix signedness implicitly.
        for src in KINDS:
            for dst in KINDS:
                if src == dst or {src, dst} <= {'Int', 'I64'}:
                    continue
                reject(f'let main (): Unit = {{ let a: {src} = 1; let b: {dst} = 1; a & b; () }}\n', "'&'")
        for kind in NEW_KINDS:
            for op in ('<<', '>>'):
                reject(f'let main (): Unit = {{ let a: {kind} = 1; let n: {kind} = 1; a {op} n; () }}\n',
                       'shift count must be Int')
        for value in ('true', '1.0', '"x"'):
            reject(f'let main (): Unit = {{ {value} & {value}; () }}\n', 'needs integer operands')
            reject(f'let main (): Unit = {{ ~{value}; () }}\n', 'Int')
        reject('let main (): Unit = { let a: U8 = 1; let n = true; a << n; () }\n', 'shift count must be Int')
        reject('let main (): Unit = { let f = |x| x & x; f(true); () }\n', 'needs integer operands')
        reject('let main (): Unit = { let f = |x| x << 1; f(1.0); () }\n', 'needs integer operands')
        reject('let bad (): (Float) -> Float = |x| x << 1\n'
               'let main (): Unit = ()\n', 'needs integer operands')
        for value in ('true', '1.0', '"x"'):
            reject('let bits[T] (x: T): T = x & x\n'
                   f'let main (): Unit = {{ bits({value}); () }}\n', 'to be an integer')
            reject('let flip[T] (x: T): T = ~x\n'
                   'let relay[T] (x: T): T = flip(x)\n'
                   f'let main (): Unit = {{ relay({value}); () }}\n', 'to be an integer')
        reject('let bits[T] (x: T): T = x & x\nlet BAD: Bool = bits(true)\n'
               'let main (): Unit = ()\n', 'to be an integer')
        reject('let bits[T] (x: T): T = x & x\n'
               'let main (): Unit = { let f: (Bool) -> Bool = bits; f(true); () }\n', 'to be an integer')
        # Globals have one initialized representation, not a specialization
        # for each reference. A later use cannot supply a global alias's type.
        for use in ('let f: (Bool) -> Bool = alias; f(true);',
                    'let f: (Int) -> Int = alias; f(1);',
                    'let f: (U8) -> U8 = alias; f(255);', ''):
            reject('let bits[T] (x: T): T = x & x\nlet alias = bits\n'
                   f'let main (): Unit = {{ {use} () }}\n', 'to be an integer')
        reject('let flip[T] (x: T): T = ~x\nlet first = flip\nlet second = first\n'
               'let main (): Unit = { let f: (Bool) -> Bool = second; f(true); () }\n', 'to be an integer')
        reject('let bits[T] (x: T): T = x & x\nlet alias: (Bool) -> Bool = bits\n'
               'let main (): Unit = ()\n', 'to be an integer')
        reject('let main (): Unit = { let a: U8 = 1; let n: U8 = a << 1; let bad: Int = n; () }\n', 'Int')

        behavior = work / 'behavior'
        behavior.mkdir()
        (behavior / 'main.plum').write_text(
            'let combine[T] (a: T) (b: T): T = a | b\n'
            'struct Owner { text: String, value: U8 }\n'
            'let take (r: Ref[Int]) (digit: Int): Owner = { r.set(r.get() * 10 + digit); '
            'Owner { text: "owned".concat("!"), value: 255 } }\n'
            'let count (r: Ref[Int]): Int = { r.set(r.get() * 10 + 2); 1 }\n'
            'let G: U8 = ~0\nlet H: I8 = 127 << 1\n'
            'let POP: Int = I16.count_ones(-1)\n'
            'let LOGICAL: I32 = I32.shr_logical(-1, 1)\n'
            'let MIX: U8 = combine(G, 1)\n'
            'let main (): Unit = { let r = ref(0); '
            'assert_eq(take(r, 1).value << count(r), 254); assert_eq(r.get(), 12); '
            'r.set(0); assert_eq(take(r, 1).value ^ take(r, 2).value, 0); assert_eq(r.get(), 12); '
            'assert_eq(G, 255); assert_eq(H, -2); assert_eq(POP, 16); '
            'assert_eq(LOGICAL, 2147483647); assert_eq(MIX, 255); println("behavior ok"); }\n')
        values = work / 'values'
        values.mkdir()
        (values / 'main.plum').write_text(
            'let main (): Unit = { let logical = I8.shr_logical; '
            'let ones = U64.count_ones; let x: I8 = -1; '
            'let high: U64 = 18446744073709551615; '
            'assert_eq(logical(x, 1), 127); assert_eq(ones(high), 64); println("values ok"); }\n')

        collision = work / 'collision'
        collision.mkdir()
        wrappers, checks = [], []
        for kind in NEW_KINDS:
            for helper in ('shr_logical', 'count_ones'):
                name = f'{kind.lower()}_{helper}_raw'
                ret = kind if helper == 'shr_logical' else 'Int'
                extra = ' (n: Int)' if helper == 'shr_logical' else ''
                args = f'x, 1' if helper == 'shr_logical' else 'x'
                exclusive = name == 'u64_count_ones_raw'
                owner = 'exclusive_user' if exclusive else f'user_{name}'
                wrappers += [f'let {owner} (): {ret} = 42',
                             f'let {name} (x: {kind}){extra}: {ret} = {owner}(())']
                if not exclusive:
                    checks.append(f'{{ let x: {kind} = 1; assert_eq({name}({args}), 42); }};')
                checks.append(f'{{ let x: {kind} = 1; let f = {name}; assert_eq(f({args}), 42); }};')
        (collision / 'main.plum').write_text('\n'.join(wrappers) + '\nlet main (): Unit = {\n' + '\n'.join(checks) + '\nprintln("collision ok")\n}\n')

        source.write_text(matrix)
        for project, expected in ((app, 'matrix ok\n'), (behavior, 'behavior ok\n'),
                                  (values, 'values ok\n'), (collision, 'collision ok\n'),
                                  (ROOT / 'bootstrap/exec_corpus/bitwise',
                                   (ROOT / 'bootstrap/exec_corpus/bitwise/expected.txt').read_text()),
                                  (ROOT / 'bootstrap/exec_corpus/integer_shift_inference',
                                   (ROOT / 'bootstrap/exec_corpus/integer_shift_inference/expected.txt').read_text())):
            for mode in ([], ['--release']):
                for core in (False, True):
                    assert run(build(project, mode, core)).stdout == expected

        # Global closures intentionally own storage for the process
        # lifetime, so test their monomorphic aliases in hosted builds.
        aliases = ROOT / 'bootstrap/exec_corpus/integer_global_aliases'
        for mode in ([], ['--release']):
            assert run(build(aliases, mode)).stdout == (aliases / 'expected.txt').read_text()

        # Negative operator counts panic at every width, in both backends and
        # modes. The method's intentionally different rule was checked above.
        for kind in KINDS:
            for op in ('<<', '>>'):
                for negative in ((-1, -(2**63)) if kind == 'Int' else (-1,)):
                    source.write_text(f'let shift (x: {kind}) (n: Int): {kind} = x {op} n\n'
                                      f'let main (): Unit = {{ shift(1, {negative}); () }}\n')
                    for mode in ([], ['--release']):
                        for core in (False, True):
                            failed = run(build(app, mode, core), ok=False)
                            assert 'negative shift count' in failed.stdout + failed.stderr, failed
                            if core:
                                assert failed.returncode == 97, failed
    print(f'integer-bits-check: {cases} model cases across {len(KINDS)} integer names; '
          'negative counts, types, collisions, function values and hosted/core debug/release pass')


if __name__ == '__main__':
    main()
