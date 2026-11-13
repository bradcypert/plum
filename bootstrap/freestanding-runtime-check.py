#!/usr/bin/env python3
"""Audit the core-only kernel object, then execute it with native test hooks.
The hosted hook harness is only a test adapter; no bootable OS is claimed."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

COMPILER = Path(sys.argv[1] if len(sys.argv) > 1 else './sh').resolve()
TARGET = 'x86_64-unknown-none'
HOOKS = {'plum_kernel_alloc', 'plum_kernel_free', 'plum_kernel_write', 'plum_kernel_panic'}
MEMORY = {'memcpy', 'memmove', 'memcmp', 'memset'}


def run(args, *, ok=True, env=None):
    result = subprocess.run(list(map(str, args)), capture_output=True, text=True,
                            env=env, timeout=180)
    assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
    return result


HARNESS = r'''
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
static long live, baseline;
void begin(void) { baseline = live; }
void done(void) {
    if (live != baseline) { fprintf(stderr,"live cells: %ld != %ld\n",live,baseline); exit(88); }
}
void *plum_kernel_alloc(uint64_t size) {
    if (getenv("PLUM_TEST_OOM")) return NULL;
    if (size > SIZE_MAX - 16) return NULL;
    void *p = malloc((size_t)size);
    if (p) ++live;
    return p;
}
void plum_kernel_free(void *p) { if (p) { --live; free(p); } }
void plum_kernel_write(const uint8_t *p, uint64_t len) { fwrite(p,1,len,stdout); }
_Noreturn void plum_kernel_panic(const uint8_t *p, uint64_t len) {
    fwrite(p,1,len,stderr); fputc('\n',stderr); exit(97);
}
'''

SOURCE = '''extern "C" { fn begin(); fn done(); }
struct Pair { text: String, values: Array[Int] }
enum Choice { Value(Pair), Empty }
let unused_hosted (): Unit = { Os.cwd(); () }
use Os;
use Time;
let apply (f: (Int) -> Int) (n: Int): Int = f(n)
let exercise (): Unit = {
    let left = "hello";
    let right = left.concat(" world");
    println(left);
    println(right);
    println("@malloc( @free( @malloc_usable_size( \\\"@printf");
    let pair = Pair { text: right, values: [1, 2, 3] };
    let other = pair.values.push(4);
    println(pair.values[1].to_string());
    println(other[3].to_string());
    println(pair == Pair { text: "hello world", values: [1, 2, 3] });
    let choice = Value(pair);
    println(match choice { Value(p) => p.text, Empty => "empty" });
    let bytes = Bytes.from_string("abc");
    let extra = bytes.concat(Bytes.from_string("def"));
    println(extra.to_hex());
    let whole = extra.slice(0, extra.len());
    println(whole.to_hex());
    println(extra.slice(2, 2).len().to_string());
    let state = ref(1);
    state.set(state.get() + 1);
    println(state.get().to_string());
    println((1.5 + 2.5) == 4.0);
    let sum = Array.concat(["a", "b"], ["c"]);
    println(sum[2]);
    let capture = "captured";
    let closure = |n: Int| { println(capture); n + 1 };
    println(apply(closure, 41).to_string());
    println(0.to_string());
    println((-9223372036854775807 - 1).to_string());
    println(9223372036854775807.to_string());
}
let main (): Unit = { unsafe { begin() }; exercise(); unsafe { done() }; () }
'''

with tempfile.TemporaryDirectory(prefix='plum-core-runtime-') as tmp:
    work = Path(tmp)
    project = work / 'app'
    project.mkdir()
    source = project / 'main.plum'
    source.write_text(SOURCE)
    hooks = work / 'hooks.c'
    hooks.write_text(HARNESS)
    env = dict(os.environ, PLUM_CACHE=str(work / 'cache'), PLUM_CACHE_TRACE='1')
    env.pop('PLUM_CC', None)

    def plum(*args, ok=True):
        return run([COMPILER, *args], ok=ok, env=env)

    for mode in ([], ['--release']):
        obj = work / ('release.o' if mode else 'debug.o')
        args = ['build', project, '--target', TARGET, '--emit-object', '-o', obj, *mode]
        first = plum(*args)
        assert 'IR miss' in first.stderr
        assert 'IR hit' in plum(*args).stderr
        names = {line.split()[-1] for line in run(['nm', '-u', obj]).stdout.splitlines()}
        assert names <= HOOKS | MEMORY | {'begin', 'done'}, names
        assert HOOKS <= names, names
        text = run(['nm', obj]).stdout
        for forbidden in ('plum_dump_stats', 'plum_args', 'plum_locale_init', 'plum_process', 'plum_thread'):
            assert forbidden not in text, forbidden
        exe = work / 'run'
        run(['clang', '-no-pie', obj, hooks, '-o', exe])
        result = run([exe])
        assert result.stdout == ('hello\nhello world\n@malloc( @free( @malloc_usable_size( "@printf\n'
            '2\n4\ntrue\nhello world\n616263646566\n616263646566\n0\n2\ntrue\nc\ncaptured\n42\n0\n'
            '-9223372036854775808\n9223372036854775807\n'), result.stdout
        oom = run([exe], ok=False, env=env | {'PLUM_TEST_OOM': '1'})
        assert oom.returncode == 97 and any(message in oom.stderr for message in ('out of memory', 'cannot allocate storage')), oom

    # Runtime failure helpers must reach the panic hook, not printf/exit.
    for body, message in [
        ('let zero = ref(0); println((10 / zero.get()).to_string())', 'division by zero'),
        ('let max = ref(9223372036854775807); println((max.get() + 1).to_string())', 'integer overflow'),
        ('let at = ref(3); println([1][at.get()].to_string())', 'array index out of bounds'),
        ('let shift = ref(-1); println((1 << shift.get()).to_string())', 'negative shift count'),
        ('panic_raw("explicit panic")', 'explicit panic'),
    ]:
        source.write_text('let main (): Unit = { ' + body + '; () }\n')
        obj = work / 'panic.o'
        plum('build', project, '--target', TARGET, '--emit-object', '-o', obj)
        exe = work / 'panic'
        run(['clang', '-no-pie', obj, hooks, '-o', exe])
        result = run([exe], ok=False)
        assert result.returncode == 97 and message in result.stderr, result

    # Unused hosted functions/imports above compile. Reached hosted calls,
    # including function values and global initialization, are diagnosed.
    for program in [
        'use Os; let inner (): Unit = { Os.cwd(); () }\nlet main (): Unit = inner()',
        'use Time; let main (): Unit = { let f = |n: Int| { Time.sleep(Time.millis(n)); () }; f(1) }',
        'let main (): Unit = { args(); () }',
        'use Time;\nlet BOOT: Int = Time.now_millis()\nlet main (): Unit = ()',
        'let main (): Unit = println(1.5.to_string())',
    ]:
        source.write_text(program + '\n')
        result = plum('build', project, '--target', TARGET, '--emit-object', '-o', work / 'bad.o', ok=False)
        diagnostic = result.stdout + result.stderr
        assert 'freestanding runtime: unsupported hosted operation' in diagnostic, diagnostic
        assert 'via main ->' in diagnostic, diagnostic

    # Close the entire link with freestanding test hooks: there must be no
    # libc, host startup files, unresolved symbols or GC-based suppression.
    # This intentionally tiny adapter is a link fixture, not the #509 allocator.
    adapter = work / 'kernel-hooks.c'
    adapter.write_text(r"""
static unsigned char arena[1048576] __attribute__((aligned(16)));
static unsigned long offset;
void *plum_kernel_alloc(unsigned long n) {
    if (n > sizeof(arena)-offset || n > (~0ul)-15) return (void*)0;
    unsigned long rounded=(n+15)&~15ul;
    if (rounded > sizeof(arena)-offset) return (void*)0;
    void *p=arena+offset; offset+=rounded; return p;
}
void plum_kernel_free(void *p) { (void)p; }
void plum_kernel_write(const unsigned char *p, unsigned long n) { (void)p; (void)n; }
_Noreturn void plum_kernel_panic(const unsigned char *p, unsigned long n) {
    (void)p; (void)n; for (;;) { __asm__ volatile ("cli; hlt"); }
}
void *memcpy(void *d, const void *s, unsigned long n) {
    unsigned char *a=d; const unsigned char *b=s;
    for (unsigned long i=0;i<n;i++) a[i]=b[i]; return d;
}
void *memmove(void *d, const void *s, unsigned long n) {
    unsigned char *a=d; const unsigned char *b=s;
    if ((unsigned long)a < (unsigned long)b) { for(unsigned long i=0;i<n;i++) a[i]=b[i]; }
    else { for(unsigned long i=n;i>0;i--) a[i-1]=b[i-1]; } return d;
}
void *memset(void *d, int value, unsigned long n) {
    unsigned char *a=d; for(unsigned long i=0;i<n;i++) a[i]=(unsigned char)value; return d;
}
int memcmp(const void *a, const void *b, unsigned long n) {
    const unsigned char *x=a,*y=b;
    for(unsigned long i=0;i<n;i++) { if(x[i]!=y[i]) return x[i]<y[i]?-1:1; } return 0;
}
""")
    script = work / 'kernel.ld'
    script.write_text('ENTRY(main)\nPHDRS { text PT_LOAD FLAGS(5); ro PT_LOAD FLAGS(4); data PT_LOAD FLAGS(6); }\n'
        'SECTIONS { . = 0xffffffff80000000; .text : { *(.text*) } :text '
        '. = ALIGN(4096); .rodata : { *(.rodata*) } :ro '
        '. = ALIGN(4096); .data : { *(.data*) } :data .bss : { *(.bss*) *(COMMON) } :data }\n')
    source.write_text('let main (): Unit = println("core".concat(" runtime"))\n')
    elf = work / 'kernel.elf'
    plum('build', project, '--target', TARGET, '--linker-script', script,
         '--link-c', adapter, '-o', elf)
    assert not run(['nm', '-u', elf]).stdout.strip()
    assert 'NEEDED' not in run(['readelf', '-d', elf]).stdout
    assert 'INTERP' not in run(['readelf', '-l', elf]).stdout

    print('freestanding-runtime-check: symbol audit, RC lifetimes, formatting, '
          'checked failures, OOM, cache reuse and transitive diagnostics passed')
