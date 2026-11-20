#!/usr/bin/env python3
"""Check C-callable Plum symbols, scalar adapters, and entry lifetime."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

COMPILER = Path(sys.argv[1] if len(sys.argv) > 1 else './sh.real').resolve()
HOSTED = 'x86_64-unknown-linux-gnu'
CORE = 'x86_64-unknown-none'
KINDS = [
    ('I8', 'int8_t', '-128', '127', 'INT8_MIN', 'INT8_MAX'),
    ('I16', 'int16_t', '-32768', '32767', 'INT16_MIN', 'INT16_MAX'),
    ('I32', 'int32_t', '-2147483648', '2147483647', 'INT32_MIN', 'INT32_MAX'),
    ('Int', 'int64_t', '-9223372036854775808', '9223372036854775807', 'INT64_MIN', 'INT64_MAX'),
    ('I64', 'int64_t', '-9223372036854775808', '9223372036854775807', 'INT64_MIN', 'INT64_MAX'),
    ('U8', 'uint8_t', '0', '255', '0', 'UINT8_MAX'),
    ('U16', 'uint16_t', '0', '65535', '0', 'UINT16_MAX'),
    ('U32', 'uint32_t', '0', '4294967295', '0', 'UINT32_MAX'),
    ('U64', 'uint64_t', '0', '18446744073709551615', '0', 'UINT64_MAX'),
]
HOOKS = r'''
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
static long live;
extern _Noreturn void plum_kernel_start(void);
void *plum_kernel_alloc(uint64_t n) {
    void *p = malloc((size_t)n);
    if (p) ++live;
    return p;
}
void plum_kernel_free(void *p) { if (p) { --live; free(p); } }
void plum_kernel_write(const uint8_t *p, uint64_t n) { fwrite(p, 1, n, stdout); }
_Noreturn void plum_kernel_panic(const uint8_t *p, uint64_t n) {
    fwrite(p, 1, n, stderr);
    exit(97);
}
_Noreturn void plum_kernel_halt(void) {
#ifdef CHECK_AFTER_MAIN
    extern int64_t early_target(void);
    (void)early_target();
#endif
    exit(live == 0 ? 0 : 98);
}
int main(void) { plum_kernel_start(); }
'''

PLUM = '''
use adapter;
extern "C" struct Sample { reading: I16, flags: U8 }
extern "C" {
    fn drive_exports() -> Bool;
    fn initial_seed() -> Int;
    fn record_unit();
}
let ordinary_before_entry = sample_i8(-128)
let seed = unsafe { initial_seed() }

extern "C" let sample_bool (value: Bool): Bool = value
extern "C" let sample_double (value: Float): Float = value + 0.5
extern "C" let sample_unit (): Unit = unsafe { record_unit() }
extern "C" let sample_seed (): Int = seed
extern "C" let sample_pointer (p: RawPtr[Sample]): RawPtr[Sample] = p
extern "C" let sample_adjust (p: RawPtr[Sample]) (delta: I8): RawPtr[Sample] = unsafe {
    let reading = p.field_ptr("reading");
    reading.store(reading.load() + I16.from(delta));
    p.field_ptr("flags").store_volatile(231);
    p
}
extern "C" let sample_mixed
    (a: I8) (b: U8) (c: I16) (d: U16) (e: I32) (f: U32) (g: Int) (h: U64): U64 = {
    assert_eq(a, -120);
    assert_eq(b, 223);
    assert_eq(c, -32000);
    assert_eq(d, 64000);
    assert_eq(e, -2000000000);
    assert_eq(f, 4000000000);
    assert_eq(g, -7000000000000000000);
    assert_eq(h, 17000000000000000000);
    h
}
let main (): Unit = {
    assert_eq(ordinary_before_entry, -128);
    assert_eq(adapter.sample_nested(), 42);
    assert(unsafe { drive_exports() });
    println("C exports ok");
}
'''

ADAPTER = '''
extern "C" { fn nested_roundtrip() -> Int; }
pub extern "C" let sample_nested (): Int = unsafe { nested_roundtrip() }
'''

C_DECLS = '''
struct Sample { int16_t reading; uint8_t flags; };
int sample_bool(int value);
double sample_double(double value);
void sample_unit(void);
int64_t sample_seed(void);
int64_t sample_nested(void);
struct Sample *sample_pointer(struct Sample *p);
struct Sample *sample_adjust(struct Sample *p, int8_t delta);
uint64_t sample_mixed(int8_t a, uint8_t b, int16_t c, uint16_t d, int32_t e,
                      uint32_t f, int64_t g, uint64_t h);
'''

C_ADAPTER = '''
static int init_count, unit_count;
static void (*volatile unit_callback)(void) = sample_unit;
int64_t initial_seed(void) { return ++init_count == 1 ? 42 : 0; }
void record_unit(void) { ++unit_count; }
int64_t nested_roundtrip(void) { return sample_seed(); }
int drive_exports(void) {
    struct Sample row = { -513, 19 };
    unit_callback();
    if (init_count != 1 || unit_count != 1 || sample_nested() != 42 ||
        sample_bool(-8) != 1 || sample_bool(42) != 1 || sample_bool(0) != 0 ||
        sample_double(1.25) != 1.75 || sample_pointer(0) != 0 ||
        sample_pointer(&row) != &row || sample_adjust(&row, -7) != &row ||
        row.reading != -520 || row.flags != 231 ||
        sample_mixed(-120, 223, -32000, 64000, -2000000000, UINT32_C(4000000000),
                     -INT64_C(7000000000000000000), UINT64_C(17000000000000000000)) !=
                     UINT64_C(17000000000000000000)) return 0;
'''

C_REFERENCE = '''
int sample_bool(int value) { return value != 0; }
double sample_double(double value) { return value + 0.5; }
void sample_unit(void) {}
int64_t sample_seed(void) { return 42; }
int64_t sample_nested(void) { return 42; }
struct Sample *sample_pointer(struct Sample *p) { return p; }
struct Sample *sample_adjust(struct Sample *p, int8_t delta) { p->reading += delta; return p; }
uint64_t sample_mixed(int8_t a, uint8_t b, int16_t c, uint16_t d, int32_t e,
                      uint32_t f, int64_t g, uint64_t h) { return a + b + c + d + e + f + g + h; }
'''


def abi_signature(ir, name):
    match = re.search(r'^define ([^\n]+) @' + name + r'\(([^\n]*)\) [^\n]*\{', ir, re.M)
    assert match, name
    def abi_types(text):
        text = re.sub(r'\([^()]*\)', '', text)
        return tuple(re.findall(r'\b(?:void|ptr|double|i\d+|signext|zeroext)\b', text))
    params = re.sub(r'\([^()]*\)', '', match[2])
    return abi_types(match[1]), tuple(map(abi_types, params.split(',')))


def fixture():
    c_decls = ['#include <stdint.h>', C_DECLS]
    definitions = [PLUM]
    reference = [C_REFERENCE]
    checks = [C_ADAPTER]
    names = re.findall(r'\b(sample_\w+)\(', C_DECLS)
    for kind, c_type, low, high, c_low, c_high in KINDS:
        name = 'sample_' + kind.lower()
        names.append(name)
        c_decls.append(f'{c_type} {name}({c_type} value);')
        reference.append(f'{c_type} {name}({c_type} value) {{ return value; }}')
        definitions.append(
            f'extern "C" let {name} (value: {kind}): {kind} = {{ '
            f'assert(value == {low} || value == {high}); value }}')
        checks.append(f'    if ({name}({c_low}) != {c_low} || {name}({c_high}) != {c_high}) return 0;')
    checks.append('    return 1;\n}')
    return '\n'.join(definitions), '\n'.join(c_decls + checks), '\n'.join(c_decls + reference), names


def main():
    with tempfile.TemporaryDirectory(prefix='plum-c-export-') as tmp:
        work = Path(tmp)
        app = work / 'app'
        (app / 'native').mkdir(parents=True)
        (app / 'adapter').mkdir()
        env = dict(os.environ, PLUM_CACHE=str(work / 'cache'))
        for key in ('PLUM_CC', 'PLUM_NO_BUILD_CACHE', 'PLUM_NO_OBJECT_CACHE'):
            env.pop(key, None)

        def run(*args, ok=True):
            result = subprocess.run(list(map(str, args)), capture_output=True, text=True,
                                    env=env, timeout=300)
            assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
            return result

        hooks = work / 'hooks.c'
        hooks.write_text(HOOKS)
        obj, exe = work / 'probe.o', work / 'probe'

        def build(project, target, *flags, after_main=False):
            if target == CORE:
                run(COMPILER, 'build', project, '--target', target, '--emit-object', *flags, '-o', obj)
                opt = ['-DCHECK_AFTER_MAIN'] if after_main else []
                run('clang', '-no-pie', *opt, obj, hooks, '-o', exe)
            else:
                run(COMPILER, 'build', project, '--target', target, *flags, '-o', exe)

        plum, native, reference, names = fixture()
        source = app / 'main.plum'
        source.write_text(plum)
        (app / 'adapter/exports.plum').write_text(ADAPTER)
        (app / 'native/adapter.c').write_text(native)
        oracle_source = work / 'oracle.c'
        oracle_source.write_text(reference)
        run(COMPILER, 'check', app)
        for target in (HOSTED, CORE):
            oracle = run('clang', '--target=' + target, '-ffreestanding', '-O2',
                         '-S', '-emit-llvm', oracle_source, '-o', '-').stdout
            for mode in ([], ['--release']):
                ir = run(COMPILER, 'emit-llvm', app, '--target', target, *mode).stdout
                for name in names:
                    actual, expected = abi_signature(ir, name), abi_signature(oracle, name)
                    assert actual == expected, (name, actual, expected)
                assert 'store volatile i8' in ir and 'load i16' in ir
                build(app, target, *mode)
                symbols = run('nm', '-g', '--defined-only', obj if target == CORE else exe).stdout
                assert set(names) <= {line.split()[-1] for line in symbols.splitlines()}
                assert run(exe).stdout == 'C exports ok\n'

        # An export reached before entry and one reached from the halt hook
        # must both fail before the body executes.
        lifetime = work / 'lifetime'
        (lifetime / 'native').mkdir(parents=True)
        (lifetime / 'native/adapter.c').write_text(
            '#include <stdint.h>\nint64_t early_target(void);\n'
            'int64_t call_export(void) { return early_target(); }\n')
        prefix = ('extern "C" { fn call_export() -> Int; }\n'
                  'extern "C" let early_target (): Int = { panic_raw("body ran"); 1 }\n')
        lifetime_main = lifetime / 'main.plum'
        for target in (HOSTED, CORE):
            for mode in ([], ['--release']):
                for trigger in ['let early = unsafe { call_export() }\nlet main (): Unit = ()',
                                'let main (): Unit = { unsafe { call_export() }; () }']:
                    lifetime_main.write_text(prefix + trigger + '\n')
                    build(lifetime, target, *mode)
                    failed = run(exe, ok=False)
                    expected = 'outside Plum main' if trigger.startswith('let early') else 'body ran'
                    assert expected in failed.stdout + failed.stderr, failed
                    assert failed.returncode == (97 if target == CORE else 1), failed
                if target == CORE:
                    lifetime_main.write_text(prefix + 'let main (): Unit = ()\n')
                    build(lifetime, target, *mode, after_main=True)
                    failed = run(exe, ok=False)
                    assert failed.returncode == 97 and 'outside Plum main' in failed.stderr, failed

        negative = work / 'negative.plum'
        cases = [
            ('extern "Other" let sample (): Int = 0', 'require the C ABI'),
            ('extern "C" let value: Int = 0', 'must be a function'),
            ('extern "C" let missing () = 0', 'explicit return type'),
            ('extern "C" let generic[T] (x: T): T = x', 'cannot have type parameters'),
            ('extern "C" let bare n: Int = n', 'named, annotated parameters'),
            ('extern "C" let unnamed ((x, y): (Int, Int)): Int = x', 'named, annotated parameters'),
            ('extern "C" let unit_arg (x: Unit): Unit = ()', 'unsupported C export parameter type'),
            ('extern "C" let s (x: CStr): Int = 0', 'unsupported C export parameter type'),
            ('extern "C" let s (x: RawCStr): Int = 0', 'unsupported C export parameter type'),
            ('extern "C" let s (x: (Int) -> Int): Int = 0', 'unsupported C export parameter type'),
            ('extern "C" let s (): String = "managed"', 'unsupported C export result type'),
            ('struct R { x: Int }\nextern "C" let record (x: R): Int = x.x', 'unsupported C export parameter type'),
            ('extern "C" struct R { x: Int }\nextern "C" let record (x: R): Int = 0', 'RawPtr pointee'),
            ('extern "C" let dotted.call (): Int = 0', 'plain C identifier'),
            ('extern "C" let return (): Int = 0', 'plain C identifier'),
            ('extern "C" let main (): Unit = ()', 'reserved'),
            ('extern "C" let println (n: Int): Int = n', 'C export name println is reserved for the builtin'),
            ('extern "C" let plum_kernel_start (): Unit = ()', 'reserved'),
            ('extern "C" let collide (): Int = 0\nextern "C" { fn collide() -> Int; }',
             'conflicts with another declaration'),
            ('let collide (): Int = 0\nextern "C" let collide (): Int = 1',
             'conflicts with another declaration'),
        ]
        for input_text, message in cases:
            main_def = '' if 'let main' in input_text else '\nlet main (): Unit = ()'
            negative.write_text(input_text + main_def + '\n')
            for command in ('check', 'emit-llvm'):
                failed = run(COMPILER, command, negative, ok=False)
                assert message in failed.stdout + failed.stderr, (input_text, failed)
        source.write_text('use adapter;\nextern "C" let sample_nested (): Int = 0\nlet main (): Unit = ()\n')
        for command in ('check', 'emit-llvm'):
            failed = run(COMPILER, command, app, ok=False)
            assert 'conflicts with another declaration' in failed.stdout + failed.stderr, failed
        negative.write_text('extern "C" let malloc (): Int = 0\nlet main (): Unit = ()\n')
        failed = run(COMPILER, 'emit-llvm', negative, ok=False)
        assert 'conflicts with a runtime symbol' in failed.stdout + failed.stderr, failed
        negative.write_text('extern "C" let sample (): Int = 0\nlet main (): Unit = ()\n')
        for target in ('aarch64-unknown-linux-gnu', 'x86_64-w64-windows-gnu', 'x86_64-unknown-linux-gnux32'):
            failed = run(COMPILER, 'emit-llvm', negative, '--target', target, ok=False)
            assert 'C export ABI is not supported for target' in failed.stdout + failed.stderr, failed
        negative.write_text('let main (): Unit = ()\n')
        assert 'plum_exports_ready' not in run(COMPILER, 'emit-llvm', negative).stdout
    print('c-export-check: C ABI, symbols, native and core lifetime, and rejections passed')


if __name__ == '__main__':
    main()
