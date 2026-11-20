#!/usr/bin/env python3
"""Check RawPtr against native scalars and records, LLVM IR and core hooks."""
import os
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile

COMPILER = Path(sys.argv[1] if len(sys.argv) > 1 else './sh.real').resolve()
HOSTED = 'x86_64-unknown-linux-gnu'
CORE = 'x86_64-unknown-none'
KINDS = [
    ('I8', 'int8_t', 8, -128, 127),
    ('U8', 'uint8_t', 8, 0, 255),
    ('I16', 'int16_t', 16, -32768, 32767),
    ('U16', 'uint16_t', 16, 0, 65535),
    ('I32', 'int32_t', 32, -2147483648, 2147483647),
    ('U32', 'uint32_t', 32, 0, 4294967295),
    ('Int', 'int64_t', 64, -(2**63), 2**63 - 1),
    ('U64', 'uint64_t', 64, 0, 2**64 - 1),
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
_Noreturn void plum_kernel_halt(void) { exit(live == 0 ? 0 : 98); }
int main(void) { plum_kernel_start(); }
'''

RECORDS = '''
pub extern "C" struct SampleFrame {
    pub tag: U8,
    pub sequence: U64,
    pub reading: Reading,
    pub count: U32,
    pub flags: U8,
}

pub extern "C" struct Reading {
    pub code: U16,
    pub source: U8,
}
'''

C_RECORDS = r'''
#include <stddef.h>
struct Reading { uint16_t code; uint8_t source; };
struct SampleFrame {
    uint8_t tag;
    uint64_t sequence;
    struct Reading reading;
    uint32_t count;
    uint8_t flags;
};
#define WIDTH(T, N) _Static_assert(sizeof(T) == N && _Alignof(T) == N, #T)
#define RECORD(T, S, A) _Static_assert(sizeof(struct T) == S && _Alignof(struct T) == A, #T)
#define FIELD(T, F, N) _Static_assert(offsetof(struct T, F) == N, #T "." #F)
RECORD(Reading, 4, 2);
FIELD(Reading, code, 0);
FIELD(Reading, source, 2);
RECORD(SampleFrame, 32, 8);
FIELD(SampleFrame, tag, 0);
FIELD(SampleFrame, sequence, 8);
FIELD(SampleFrame, reading, 16);
FIELD(SampleFrame, count, 20);
FIELD(SampleFrame, flags, 24);

static struct SampleFrame frames[2] = {
    {3, 4, {9, 4}, 50, 6},
    {13, UINT64_C(18364758544493064720), {513, 17}, 101, 200},
};
struct SampleFrame *frame_source(void) {
    unsigned char *bytes = (unsigned char *)&frames[1];
    bytes[1] = 0xAB;
    bytes[19] = 0xCD;
    bytes[31] = 0xEF;
    return frames;
}
struct SampleFrame *echo_frame(struct SampleFrame *p) { return p; }
int frame_neighbors_intact(void) {
    const unsigned char *bytes = (const unsigned char *)&frames[1];
    return frames[0].flags == 6 && frames[1].tag == 13
        && frames[1].sequence == UINT64_C(18364758544493064720)
        && frames[1].reading.code == 513 && frames[1].reading.source == 17
        && frames[1].count == 101 && frames[1].flags == 5
        && bytes[1] == 0xAB && bytes[19] == 0xCD && bytes[31] == 0xEF;
}
uint16_t volatile_frame(volatile struct SampleFrame *p, uint8_t value) {
    p[1].flags = value;
    return p[1].reading.code;
}
'''

RECORD_OPERATIONS = '''
let frame_stride = NativeLayout.size_of[layout.SampleFrame]()

let check_layout (): Unit = {
    assert_eq(frame_stride, 32);
    assert_eq(NativeLayout.align_of[layout.SampleFrame](), 8);
    assert_eq(NativeLayout.size_of[layout.Reading](), 4);
    assert_eq(NativeLayout.align_of[layout.Reading](), 2);
    assert_eq(NativeLayout.offset_of[layout.Reading]("code"), 0);
    assert_eq(NativeLayout.offset_of[layout.Reading]("source"), 2);
    assert_eq(NativeLayout.offset_of[layout.SampleFrame]("tag"), 0);
    assert_eq(NativeLayout.offset_of[layout.SampleFrame]("sequence"), 8);
    assert_eq(NativeLayout.offset_of[layout.SampleFrame]("reading"), 16);
    assert_eq(NativeLayout.offset_of[layout.SampleFrame]("count"), 20);
    assert_eq(NativeLayout.offset_of[layout.SampleFrame]("flags"), 24);
}

let volatile_frame (p: RawPtr[layout.SampleFrame]) (value: U8): U16 = unsafe {
    let second = p.offset_bytes(NativeLayout.size_of[layout.SampleFrame]());
    second.field_ptr("flags").store_volatile(value);
    second.field_ptr("reading").field_ptr("code").load_volatile()
}
'''


def fixtures():
    native = ['#include <stdint.h>']
    decls = []
    functions = ['struct Box { pointer: RawPtr[U8] }',
                 'let identity[T] (value: T): T = value',
                 'let equal[T] (value: T): Bool = value == value',
                 'let equal_int: (Int) -> Bool = equal',
                 'let less[T] (a: T) (b: T): Bool = { let cmp = |x, y| x < y; cmp(a, b) }',
                 'let show_value[T] (a: T): String = { let show = |x| x.to_string(); show(a) }',
                 'let hash_value[T] (a: T): Int = { let hash = |x| x.hash(); hash(a) }',
                 'let null (p: RawPtr[U8]): Bool = p.is_null()',
                 'let alias (p: RawPtr[I64]): RawPtr[Int] = identity(p)',
                 'let make_reader (p: RawPtr[U8]): () -> U8 = '
                 '{ let box = Box { pointer: p }; || unsafe { box.pointer.load() } }']
    body = []
    expected = []
    for kind, c_type, bits, low, high in KINDS:
        width = bits // 8
        name = kind.lower()
        limits = ('INT' if kind.startswith('I') else 'UINT') + str(bits)
        c_low = limits + '_MIN' if low < 0 else '0'
        c_high = limits + '_MAX'
        native += [
            f'static {c_type} storage_{name}[] = {{11, {c_low}, 19}};',
            f'{c_type} *ptr_{name}(void) {{ return &storage_{name}[1]; }}',
            f'{c_type} *echo_{name}({c_type} *p) {{ return p; }}',
            f'int valid_{name}(void) {{ return storage_{name}[0] == 11 && '
            f'storage_{name}[1] == {c_high} && storage_{name}[2] == 19; }}',
            f'{c_type} volatile_{name}(volatile {c_type} *p, {c_type} v) '
            '{ *p = v; *p = v; (void)*p; return *p; }',
        ]
        decls += [
            f'fn ptr_{name}() -> RawPtr[{kind}];',
            f'fn echo_{name}(p: RawPtr[{kind}]) -> RawPtr[{kind}];',
            f'fn valid_{name}() -> Bool;',
        ]
        functions.append(
            f'let volatile_{name} (p: RawPtr[{kind}]) (value: {kind}): {kind} = unsafe {{ '
            'p.store_volatile(value); p.store_volatile(value); p.load_volatile(); p.load_volatile() }'
        )
        body += [
            f'let p = identity(echo_{name}(ptr_{name}()));',
            f'let same = RawPtr.from_address[{kind}](p.to_address());',
            'assert(!same.is_null());',
            f'assert_eq(same.offset_bytes({width}).offset_bytes(-{width}).load(), {low});',
            'same.store(42);',
            'assert_eq(p.load(), 42);',
            f'assert_eq(volatile_{name}(p, {high}), {high});',
            f'assert(valid_{name}());',
        ]
        expected.append((name, bits))
    native += ['uint8_t *native_null(void) { return (uint8_t *)0; }',
               'static int calls;',
               'uint8_t *ordered_pointer(void) { calls = 1; return ptr_u8(); }',
               'uint8_t ordered_value(void) { return calls++ == 1 ? 9 : 0; }']
    decls += ['fn native_null() -> RawPtr[U8];',
              'fn ordered_pointer() -> RawPtr[U8];', 'fn ordered_value() -> U8;']
    body += [
        'let compare_int = equal_int;',
        'assert(compare_int(5));',
        'let low: U16 = 41;',
        'let high: U16 = 42;',
        'assert(less(low, high));',
        'assert_eq(show_value(high), "42");',
        'assert_eq(hash_value(high), high.hash());',
        'assert(null(native_null()));',
        'let is_null_pointer: (RawPtr[U8]) -> Bool = |p| p.is_null();',
        'assert(is_null_pointer(native_null()));',
        'let cell = ref(native_null());',
        'assert(equal(cell));',
        'let show_int = |n| n.to_string();',
        'assert_eq(show_int(42), "42");',
        'let show_string = |s| s.to_string();',
        'assert_eq(show_string("hello"), "hello");',
        'assert(echo_u8(RawPtr.from_address[U8](0)).is_null());',
        'assert_eq(native_null().to_address(), 0);',
        'let reader = make_reader(ptr_u8());',
        'assert_eq(reader(), 255);',
        'assert_eq(alias(ptr_int()).load(), 9223372036854775807);',
        'let refs = [ptr_u8(), ptr_u8()];',
        'ordered_pointer().store(ordered_value());',
        'assert_eq(refs[1].load(), 9);',
        'println("raw pointers ok");',
    ]
    native += [C_RECORDS] + [f'WIDTH({c_type}, {bits // 8});' for _, c_type, bits, _, _ in KINDS]
    decls += ['fn frame_source() -> RawPtr[layout.SampleFrame];',
              'fn echo_frame(p: RawPtr[layout.SampleFrame]) -> RawPtr[layout.SampleFrame];',
              'fn frame_neighbors_intact() -> Bool;']
    body[-1:-1] = [
        'check_layout();',
        'let frame = identity(echo_frame(frame_source()));',
        'assert(!frame.is_null());',
        'let roundtrip = RawPtr.from_address[layout.SampleFrame](frame.to_address());',
        'let inspect = || volatile_frame(roundtrip, 5);',
        'assert_eq(inspect(), 513);',
        'let reading = frame.field_ptr("reading");',
        'assert_eq(reading.field_ptr("code").load(), 9);',
        'assert(frame_neighbors_intact());',
    ]
    source = ('use wire as layout;\nextern "C" {\n' + '\n'.join(decls) + '\n}\n'
              + '\n'.join(functions) + RECORD_OPERATIONS + '\nlet main (): Unit = unsafe {\n'
              + '\n'.join(body) + '\n}\n')
    return source, '\n'.join(native) + '\n', expected


def llvm_function(ir, name):
    symbol = re.escape(name)
    function = re.search(r'(?m)^define [^\n]*@' + symbol + r'[.(][^\n]*\{\n(.*?)^}', ir, re.S)
    assert function, name
    return function[1]


def check_volatile(ir, shapes, prefix='plum.f..'):
    for name, bits in shapes:
        body = llvm_function(ir, prefix + 'volatile_' + name)
        accesses = re.findall(r'\b(load|store) volatile i(\d+)[^\n]*align (\d+)', body)
        assert accesses == [(op, str(bits), str(bits // 8))
                            for op in ['store', 'store', 'load', 'load']], (name, body)
        assert not re.search(r'\b(?:fence|atomicrmw|cmpxchg)\b|\b(?:load|store) atomic\b', body), body


def check_frame_accesses(ir, oracle=False, optimized=False):
    body = llvm_function(ir, 'volatile_frame' if oracle else 'plum.f..volatile_frame')
    assert not re.search(r'\b(?:fence|atomicrmw|cmpxchg)\b|\b(?:load|store) atomic\b', body), body
    accesses = re.findall(r'\b(load|store) volatile i(\d+)[^\n]*align (\d+)', body)
    assert [(op, width) for op, width, _ in accesses] == [('store', '8'), ('load', '16')], body
    alignment = [int(align) for _, _, align in accesses]
    if oracle:
        assert alignment[0] >= 1 and alignment[1] >= 2, body
    else:
        assert alignment == [1, 2], body
    if not oracle and not optimized:
        offsets = [int(offset) for offset in re.findall(r'getelementptr i8[^\n]*i64 (\d+)\b', body)]
        assert offsets == [32, 24, 16, 0], body


def main():
    with tempfile.TemporaryDirectory(prefix='plum-raw-pointer-') as scratch:
        work = Path(scratch)
        app = work / 'app'
        (app / 'native').mkdir(parents=True)
        (app / 'wire').mkdir()
        records = app / 'wire/records.plum'
        records.write_text(RECORDS)
        source, c_source, shapes = fixtures()
        (app / 'main.plum').write_text(source)
        (app / 'native/scalars.c').write_text(c_source)
        env = dict(os.environ, PLUM_CACHE=str(work / 'cache'))
        for name in ('PLUM_CC', 'PLUM_NO_BUILD_CACHE', 'PLUM_NO_OBJECT_CACHE'):
            env.pop(name, None)

        def run(*args, ok=True):
            result = subprocess.run(list(map(str, args)), env=env, capture_output=True, text=True, timeout=300)
            assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
            return result

        fmt = work / 'format.plum'
        fmt.write_text('pub extern "C" struct Fixture { pub first:U8, second:U16, third:U64 }\n')
        ast = run(COMPILER, 'ast', fmt).stdout
        formatted = run(COMPILER, 'fmt', fmt).stdout
        assert '(extern-struct pub Fixture' in ast and '\n    second: U16,' in formatted
        fmt.write_text(formatted)
        assert run(COMPILER, 'ast', fmt).stdout == ast
        assert run(COMPILER, 'fmt', fmt).stdout == formatted

        declaration = RECORDS.index('Reading {')
        line = RECORDS.count('\n', 0, declaration) + 1
        column = declaration - RECORDS.rfind('\n', 0, declaration)
        refs = json.loads(run(COMPILER, 'references', app, records, line, column, 'true').stdout)
        assert {ref['uri'] for ref in refs} == {records.as_uri(), (app / 'main.plum').as_uri()}, refs

        hooks = work / 'hooks.c'
        hooks.write_text(HOOKS)
        for target in (HOSTED, CORE):
            oracle = run('clang', '--target=' + target, '-ffreestanding', '-O2', '-S',
                         '-emit-llvm', app / 'native/scalars.c', '-o', '-').stdout
            check_volatile(oracle, shapes, prefix='')
            check_frame_accesses(oracle, oracle=True)
            assert re.search(r'define[^\n]*\bptr @echo_frame\(ptr\b', oracle)
            for mode in ([], ['--release']):
                ir = run(COMPILER, 'emit-llvm', app, '--target', target, *mode).stdout
                for name, _ in shapes:
                    assert f'declare ptr @ptr_{name}()' in ir, name
                    assert f'declare ptr @echo_{name}(ptr)' in ir, name
                    assert re.search(r'define[^\n]*\bptr @echo_' + re.escape(name) + r'\(ptr\b', oracle), name
                assert 'inttoptr i64' in ir and 'ptrtoint ptr' in ir
                assert 'declare ptr @echo_frame(ptr)' in ir
                assert 'declare ptr @frame_source()' in ir
                check_volatile(ir, shapes)
                check_frame_accesses(ir)
                if mode:
                    ll = work / 'program.ll'
                    ll.write_text(ir)
                    optimized = run('clang', '-Wno-override-module', '--target=' + target,
                                    '-O2', '-S', '-emit-llvm', ll, '-o', '-').stdout
                    check_volatile(optimized, shapes)
                    check_frame_accesses(optimized, optimized=True)
                exe = work / 'program'
                if target == CORE:
                    obj = work / 'program.o'
                    run(COMPILER, 'build', app, '--target', target, '--emit-object', *mode, '-o', obj)
                    undefined = {line.split()[-1] for line in run('nm', '-u', obj).stdout.splitlines()}
                    assert undefined <= {
                        'plum_kernel_alloc', 'plum_kernel_free', 'plum_kernel_write',
                        'plum_kernel_panic', 'plum_kernel_halt', 'memcpy', 'memmove', 'memset', 'memcmp',
                    }, undefined
                    run('clang', '-no-pie', obj, hooks, '-o', exe)
                else:
                    run(COMPILER, 'build', app, *mode, '-o', exe)
                assert run(exe).stdout == 'raw pointers ok\n'

        neg = work / 'negative.plum'
        bound = 'let p = unsafe { RawPtr.from_address[U8](0) }; '
        cases = [
            ('let bad (p: RawPtr[Bool]): Unit = ()', 'requires a concrete integer pointee'),
            ('let bad[T] (p: RawPtr[T]): Unit = ()', 'requires a concrete integer pointee'),
            ('let bad (p: RawPtr): Unit = ()', 'RawPtr'),
            ('let bad (p: RawPtr[U8, U16]): Unit = ()', 'RawPtr'),
            ('let main (): Unit = { RawPtr.from_address[U8](0); () }', "requires an 'unsafe' block"),
            ('let main (): Unit = unsafe { RawPtr.from_address(0); () }', 'one explicit integer type argument'),
            ('let main (): Unit = unsafe { RawPtr.from_address[U8, U16](0); () }', 'one explicit integer type argument'),
            ('let main (): Unit = unsafe { RawPtr.from_address[U8](); () }', 'expects 1 argument'),
            ('let main (): Unit = unsafe { let n = 1; RawPtr.from_address[U8](n); () }', 'U64'),
            ('let main (): Unit = unsafe { let p = RawPtr.from_address[U8]; () }', 'cannot be used as a function value'),
            ('struct RawPtr { field: Int }', 'reserved built-in type'),
            ('extern "C" { fn source() -> RawCStr; }\n'
             'let main (): Unit = { let p: RawPtr[U8] = unsafe { source() }; () }', 'RawCStr'),
            ('extern "C" { fn source(p: RawPtr[U16]); }\n'
             'let main (): Unit = { ' + bound + 'unsafe { source(p) }; () }', 'RawPtr[U16]'),
            ('extern "C" { fn callback(f: (RawPtr[U8]) -> Unit); }', 'callbacks containing RawPtr'),
            ('struct Box { field: RawPtr[U8] }\nextern "C" { fn f() -> Box; }', 'aggregates and callbacks containing RawPtr'),
            ('enum Choice { Value(RawPtr[U8]), Empty }\nextern "C" { fn f(x: Choice); }', 'aggregates and callbacks containing RawPtr'),
            ('let eq[T] (a: T): Bool = a == a\nlet main (): Unit = { ' + bound + 'eq(p); () }', 'requires T'),
            ('let eq[T] (a: T): Bool = a == a\n'
             'let result = eq(unsafe { RawPtr.from_address[U8](0) })', 'requires T'),
            ('let eq[T] (xs: Array[T]): Bool = xs == xs\n'
             'let main (): Unit = { ' + bound + 'eq([p]); () }', 'requires T'),
            ('let eq[T] (a: T): Bool = { let same = |x| x == x; same(a) }\n'
             'let main (): Unit = { ' + bound + 'eq(p); () }', 'requires T'),
            ('let eq[T] (a: T): Bool = a == a\nlet nested[T] (a: T): Bool = eq([a])\n'
             'let main (): Unit = { ' + bound + 'nested(p); () }', 'requires T'),
            ('let hash_value[T] (a: T): Int = a.hash()\nlet nested[T] (a: T): Int = hash_value([a])\n'
             'let main (): Unit = { ' + bound + 'nested(p); () }', 'requires T to support structural hashing'),
            ('let less[T] (a: T): Bool = { let cmp = |x| x < x; cmp(a) }\n'
             'let main (): Unit = { ' + bound + 'less(p); () }', 'requires T to be ordered'),
            ('let show_value[T] (a: T): String = { let show = |x| x.to_string(); show(a) }\n'
             'let main (): Unit = { ' + bound + 'show_value(p); () }', 'requires T to have a text form'),
            ('let hash_value[T] (a: T): Int = { let hash = |x| x.hash(); hash(a) }\n'
             'let main (): Unit = { ' + bound + 'hash_value(p); () }', 'requires T to support structural hashing'),
            ('let add[T] (a: T): T = a + a\nlet main (): Unit = { ' + bound + 'add(p); () }', 'requires T'),
            ('let eq[T] (a: T): Bool = a == a\nlet alias = eq\n'
             'let main (): Unit = { ' + bound + 'let f = alias; f(p); () }',
             'add a concrete type annotation'),
            ('let add[T] (a: T): T = a + a\nlet alias = add\n'
             'let main (): Unit = { ' + bound + 'let f = alias; f(p); () }',
             'add a concrete type annotation'),
            ('extern "Rust" struct Wrong { value: U8 }', 'native records require the C ABI'),
            ('extern "C" struct Empty {}', 'at least one field'),
            ('extern "C" struct Generic[T] { value: T }', 'cannot have type parameters'),
            ('extern "C" struct Duplicate { value: U8, value: U16 }', 'duplicate native field'),
            ('extern "C" struct Cycle { value: Cycle }', 'cycle through inline native field'),
            ('extern "C" struct A { value: B }\nextern "C" struct B { value: A }',
             'cycle through inline native field'),
            ('extern "C" struct A { value: Int }\nstruct A { value: U8 }', 'conflicts with an existing type'),
            ('extern "C" struct A { value: U8 }\nextern "C" struct A { value: U8 }', 'already declared'),
            ('struct NativeLayout { size: Int }', 'reserved built-in type'),
            ('let NativeLayout.size_of (): Int = 1', 'reserved built-in namespace'),
        ]
        for field_type in ['Bool', 'Float', 'String', 'Array[U8]', 'RawPtr[U8]', 'Managed']:
            cases.append(('struct Managed { value: U8 }\n'
                          f'extern "C" struct A {{ value: {field_type} }}',
                          'requires a concrete integer or native record'))
        oversized = ['extern "C" struct R0 { value: U64 }']
        for n in range(1, 61):
            oversized.append(f'extern "C" struct R{n} {{ left: R{n - 1}, right: R{n - 1} }}')
        cases.append(('\n'.join(oversized), 'native record layout exceeds Int size'))

        record = 'extern "C" struct A { value: U16 }\n'
        cases += [
            (record + 'let bad (value: A): Unit = ()', 'only admitted as a RawPtr pointee'),
            (record + 'struct Managed { value: A }', 'only admitted as a RawPtr pointee'),
            (record + 'extern "C" { fn f() -> A; }', 'only admitted as a RawPtr pointee'),
            (record + 'extern "C" { fn f(value: A); }', 'only admitted as a RawPtr pointee'),
            (record + 'extern "C" { fn f(cb: (RawPtr[A]) -> Unit); }', 'callbacks containing RawPtr'),
            (record + 'let main (): Unit = { A { value: 0 }; () }', 'cannot be built with a struct literal'),
            (record + 'let x = NativeLayout.size_of[A](1)', 'expects 0 argument'),
            (record + 'let x = NativeLayout.align_of()', 'one explicit native record type argument'),
            (record + 'let x = NativeLayout.offset_of[A]("missing")', 'no native field A.missing'),
            (record + 'let x = NativeLayout.size_of[U8]()', 'requires a concrete native record type'),
            (record + 'let x = NativeLayout.align_of[A]', 'cannot be used as function values'),
            (record + 'let bad (field: String): Int = NativeLayout.offset_of[A](field)', 'string literal'),
            (record + 'let bad (p: RawPtr[A]): RawPtr[U16] = p.field_ptr("value")', "requires an 'unsafe' block"),
            (record + 'let bad (p: RawPtr[A]): RawPtr[U16] = unsafe { p.field_ptr("missing") }',
             'no native field A.missing'),
            (record + 'let bad (p: RawPtr[A]) (field: String): RawPtr[U16] = unsafe { p.field_ptr(field) }',
             'string literal'),
            (record + 'let bad (p: RawPtr[U16]): RawPtr[U16] = unsafe { p.field_ptr("value") }',
             'requires a native record pointee'),
            (record + 'extern "C" struct B { value: U16 }\nlet bad (p: RawPtr[A]): RawPtr[B] = p', 'RawPtr[B]'),
        ]
        for access in ['load()', 'load_volatile()', 'store(0)', 'store_volatile(0)']:
            cases.append((record + 'let bad (p: RawPtr[A]): Unit = unsafe { p.' + access + '; () }',
                          'cannot access all of native record'))
        for operation in ['load()', 'load_volatile()', 'to_address()', 'offset_bytes(1)',
                          'store(1)', 'store_volatile(1)']:
            cases.append(('let main (): Unit = { ' + bound + 'p.' + operation + '; () }',
                          "requires an 'unsafe' block"))
        for body, message in [
            ('unsafe { p.store() }', 'expects 1 argument'),
            ('unsafe { p.load(1) }', 'expects 0 argument'),
            ('let func = p.load', 'methods require a direct call'),
            ('let n: U16 = 1; unsafe { p.store(n) }', 'U8'),
            ('p == p', 'no structural equality'),
            ('p < p', 'needs an ordered type'),
            ('p.hash()', 'not supported'),
            ('p + p', 'does not support arithmetic'),
            ('-p', 'does not support arithmetic'),
            ('[p] == [p]', 'no structural equality'),
            ('let eq = |x| x == x; eq(p)', 'no structural equality'),
            ('let show = |x| x.to_string(); show(p)', ".to_string() on a RawPtr isn't supported"),
        ]:
            cases.append(('let main (): Unit = { ' + bound + body + '; () }', message))
        for text, message in cases:
            neg.write_text(text + '\n' + ('' if 'let main' in text else 'let main (): Unit = ()\n'))
            for command in ['check', 'emit-llvm']:
                failed = run(COMPILER, command, neg, ok=False)
                assert message in failed.stdout + failed.stderr, (text, message, failed)
        neg.write_text('struct Box { field: RawPtr[U8] }\nlet main (): Unit = ()\n')
        run(COMPILER, 'check', neg)
        for target in ['aarch64-unknown-linux-gnu', 'x86_64-w64-windows-gnu', 'x86_64-unknown-linux-gnux32']:
            failed = run(COMPILER, 'emit-llvm', neg, '--target', target, ok=False)
            assert 'RawPtr is not supported for target' in failed.stdout + failed.stderr, failed
        neg.write_text(record + 'let main (): Unit = ()\n')
        run(COMPILER, 'check', neg)
        for target in ['aarch64-unknown-linux-gnu', 'x86_64-w64-windows-gnu', 'x86_64-unknown-linux-gnux32']:
            failed = run(COMPILER, 'emit-llvm', neg, '--target', target, ok=False)
            assert 'native aggregate layout is not supported for target' in failed.stdout + failed.stderr, failed

        private = work / 'privacy'
        (private / 'wire').mkdir(parents=True)
        (private / 'wire/records.plum').write_text(
            'pub extern "C" struct Visible { hidden: U8 }\nextern "C" struct Hidden { value: U8 }\n')
        for text, message in [
            ('let bad (p: RawPtr[layout.Hidden]): Unit = ()', 'native record wire.Hidden is private'),
            ('let bad (p: RawPtr[layout.Visible]): RawPtr[U8] = unsafe { p.field_ptr("hidden") }',
             'native field wire.Visible.hidden is private'),
            ('let x = NativeLayout.offset_of[layout.Visible]("hidden")', 'native field wire.Visible.hidden is private'),
        ]:
            (private / 'main.plum').write_text('use wire as layout;\n' + text + '\nlet main (): Unit = ()\n')
            for command in ['check', 'emit-llvm']:
                failed = run(COMPILER, command, private, ok=False)
                assert message in failed.stdout + failed.stderr, (text, message, failed)
    print('raw-pointer-check: scalar and record layout, native and core debug/release, volatile IR and rejections passed')


if __name__ == '__main__':
    main()
