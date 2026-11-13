#!/usr/bin/env python3
"""Exercise the actual kernel support through a UART/halt model, inspect the
privileged x86 object, and close a libc-free ELF link. Does not boot QEMU."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
SUPPORT = ROOT / 'examples/kernel-hello/support'
DRIVER = ROOT / 'bootstrap/kernel-hooks-test.c'
COMPILER = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / 'sh').resolve()
FLAGS = ['--target=x86_64-unknown-none', '-march=x86-64', '-mcmodel=kernel',
         '-mno-red-zone', '-ffreestanding', '-fno-builtin', '-fno-stack-protector',
         '-fno-pic', '-fno-pie', '-fno-unwind-tables', '-fno-asynchronous-unwind-tables',
         '-Wall', '-Wextra', '-Werror', '-I', str(SUPPORT)]


def run(args, *, ok=True, env=None):
    result = subprocess.run(list(map(str, args)), capture_output=True, text=True,
                            timeout=180, env=env)
    assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
    return result


with tempfile.TemporaryDirectory(prefix='plum-kernel-hooks-') as tmp:
    work = Path(tmp)
    for optimization in ('-O0', '-O2'):
        objects = []
        for name in ('kernel', 'memory', 'platform'):
            obj = work / f'{name}.o'
            run(['clang', *FLAGS, optimization, '-c', SUPPORT / f'{name}.c', '-o', obj])
            symbols = run(['nm', '-u', obj]).stdout
            allowed = {'plum_kernel_halt', 'plum_kernel_port_read8', 'plum_kernel_port_write8'} if name == 'kernel' else set()
            assert {line.split()[-1] for line in symbols.splitlines()} <= allowed, symbols
            if name != 'platform':
                objects.append(obj)
        disassembly = run(['objdump', '-d', work / 'platform.o']).stdout
        for instruction in ('in', 'out', 'cli', 'hlt'):
            assert re.search(r'\b' + instruction + r'\b', disassembly), disassembly
        unit = work / 'unit'
        run(['clang', optimization, '-fno-builtin', '-Wall', '-Wextra', '-Werror', '-I', SUPPORT,
             '-DPLUM_KERNEL_UNIT_TEST', DRIVER, *objects, '-no-pie', '-Wl,--wrap=plum_kernel_free', '-o', unit])
        assert 'allocator, UART, panic and memory primitives passed' in run([unit]).stdout
        run([unit, 'early-panic'])

    project = work / 'app'
    project.mkdir()
    source = project / 'main.plum'
    source.write_text('''struct Row { name: String, values: Array[Int] }
let TITLE: String = "kernel ".concat("hooks")
let main (): Unit = {
    println(TITLE);
    let first = Row { name: "first", values: [1, 2, 3] };
    let rows = [first];
    let more = rows.push(Row { name: "last", values: [4] });
    println(rows[0].name);
    println(more.len().to_string());
    println(more[1].name);
    println(first.values[1].to_string());
    println("A\\x00B");
}
''')
    normal = source.read_text()
    env = dict(os.environ, PLUM_CACHE=str(work / 'cache'))
    env.pop('PLUM_CC', None)

    def build_object(mode):
        obj = work / 'app.o'
        run([COMPILER, 'build', project, '--target', 'x86_64-unknown-none', '--emit-object',
             '-o', obj, *mode], env=env)
        undefined = {line.split()[-1] for line in run(['nm', '-u', obj]).stdout.splitlines()}
        assert undefined <= {'plum_kernel_alloc', 'plum_kernel_free', 'plum_kernel_write', 'plum_kernel_panic', 'plum_kernel_halt', 'memcpy', 'memmove', 'memset', 'memcmp'}, undefined
        exe = work / 'execute'
        run(['clang', '-fno-builtin', '-I', SUPPORT, DRIVER, obj, *objects, '-no-pie',
             '-Wl,--wrap=plum_kernel_free', '-o', exe])
        return exe

    for mode in ([], ['--release']):
        source.write_text(normal)
        exe = build_object(mode)
        result = run([exe], env=env | {'PLUM_TEST_EXPECT_FREE': '1'})
        assert result.stdout == 'kernel hooks\nfirst\n2\nlast\n2\nA\x00B\n', result.stdout
        failed = run([exe], ok=False, env=env | {'PLUM_TEST_OOM': '1'})
        assert failed.returncode == 97 and failed.stdout.startswith('panic: ') and 'allocate' in failed.stdout, failed
        for body, expected in [
            ('let n = ref(0); println((1 / n.get()).to_string())', 'division by zero'),
            ('let n = ref(9223372036854775807); println((n.get() + 1).to_string())', 'integer overflow'),
            ('let n = ref(4); println([1][n.get()].to_string())', 'array index out of bounds'),
            ('panic_raw("test panic")', 'test panic'),
        ]:
            source.write_text('let main (): Unit = { ' + body + '; () }\n')
            failed = run([build_object(mode)], ok=False)
            assert failed.returncode == 97 and failed.stdout == f'panic: {expected}\n', failed

    source.write_text(normal)
    script = work / 'kernel.ld'
    script.write_text('ENTRY(plum_kernel_start)\nPHDRS { text PT_LOAD FLAGS(5); ro PT_LOAD FLAGS(4); data PT_LOAD FLAGS(6); }\n'
        'SECTIONS { . = 0xffffffff80000000; .text : { *(.text*) } :text '
        '. = ALIGN(4096); .rodata : { *(.rodata*) } :ro '
        '. = ALIGN(4096); .data : { *(.data*) } :data .bss : { *(.bss*) *(COMMON) } :data }\n')
    elf = work / 'kernel.elf'
    run([COMPILER, 'build', project, '--target', 'x86_64-unknown-none', '--release',
         '--link-c', SUPPORT / 'kernel.c', '--link-c', SUPPORT / 'memory.c',
         '--link-c', SUPPORT / 'platform.c', '--linker-script', script, '-o', elf], env=env)
    assert not run(['nm', '-u', elf]).stdout.strip()
    assert 'NEEDED' not in run(['readelf', '-d', elf]).stdout
    assert 'INTERP' not in run(['readelf', '-l', elf]).stdout
    print('kernel-hooks-check: bounded allocation, UART/backpressure, memory primitives, '
          'real Plum output/releases/panics and static ELF closure passed')
