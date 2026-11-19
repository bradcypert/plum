#!/usr/bin/env python3
"""Inspect the freestanding IR, objects, final ELF and build cache."""
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

COMPILER = Path(sys.argv[1] if len(sys.argv) > 1 else './sh').resolve()
TARGET = 'x86_64-unknown-none'
for tool in ('clang', 'ld.lld', 'readelf', 'nm', 'objdump'):
    if not shutil.which(tool):
        raise SystemExit(f'freestanding-check: required tool missing: {tool}')


def run(args, *, env=None, ok=True):
    result = subprocess.run(list(map(str, args)), env=env, capture_output=True,
                            text=True, timeout=180)
    assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
    return result


def check_ir(ir, *, release):
    assert f'target triple = "{TARGET}"' in ir
    assert re.search(r'^target datalayout = "e-.*-i64:64-.*"$', ir, re.M)
    model = re.search(r'^!(\d+) = !\{i32 1, !"Code Model", i32 2\}$', ir, re.M)
    flags = re.search(r'^!llvm\.module\.flags = !\{([^}]+)\}$', ir, re.M)
    assert model and flags and '!' + model[1] in flags[1].split(', '), (model, flags)
    assert bool(re.search(r' !dbg !\d+', ir)) == (not release)
    assert re.search(r'define void @plum_kernel_start\(\).*noreturn', ir)
    assert not re.search(r'define .*@main\(', ir)
    assert 'call void @plum_kernel_halt()' in ir
    for forbidden in ('@plum_locale_init(', '@atexit(', '@srand('):
        assert forbidden not in ir, forbidden

    definitions = [line for line in ir.splitlines() if line.startswith('define ')]
    assert definitions
    for line in definitions:
        assert ' noredzone ' in line and '"target-cpu"="x86-64"' in line, line
        features = re.search(r'"target-features"="([^"]+)"', line)
        assert features and {'+sse2', '-avx', '-avx2', '-avx512f'} <= set(features[1].split(',')), line


with tempfile.TemporaryDirectory(prefix='plum-freestanding-') as tmp:
    work = Path(tmp)
    app = work / 'app with spaces'
    native = app / 'native'
    native.mkdir(parents=True)
    (native / 'posix').mkdir()
    # This must never be discovered for the freestanding target.
    (native / 'posix' / 'host.c').write_text('#error hosted POSIX source selected\n')
    (app / 'main.plum').write_text('extern "C" { fn helper() -> Int; }\n'
        'let rewrite (n: Int): Array[String] = { let mut xs = ["initial"];\n'
        'assert(xs.len() == 1); for i in 0..n { xs = [i.to_string()]; }; xs }\n'
        'let main (): Unit = { let chars = chars_of("aé\\x00");\n'
        'assert(rewrite(1)[0] == "0" && rewrite(1)[0] == "0");\n'
        'assert(chars.len() == 3 && chars[0] == "a" && chars[1] == "é" && chars[2] == "\\x00");\n'
        'assert(chars[0].concat("!") == "a!");\n'
        'let parts = "a::b".split("::");\n'
        'assert(parts.len() == 2 && parts[0] == "a" && parts[1] == "b");\n'
        'assert(",,,,,,,,".split(",").len() == 9);\n'
        'assert(" \\u{2003}\\t ".trim() == "");\n'
        'println((unsafe { helper() }).to_string()); }\n')
    (native / 'value.h').write_text('#define VALUE 7\n')
    (native / 'helper.c').write_text('#include "value.h"\nlong long helper(void) { return VALUE; }\n'
        'long leaf(long x) { volatile long pad[8]; pad[0] = x; return pad[0]; }\n')
    assembly = work / 'extra source.S'
    (work / 'asm_value.h').write_text('#define ASM_VALUE 23\n')
    assembly.write_text('#include "asm_value.h"\n.global asm_marker\n.set asm_marker, ASM_VALUE\n')
    obj = work / 'kernel.o'
    env = dict(os.environ, PLUM_CACHE=str(work / 'cache'), PLUM_CACHE_TRACE='1')
    for key in ('PLUM_NO_BUILD_CACHE', 'PLUM_NO_OBJECT_CACHE', 'PLUM_CC', 'CCC_OVERRIDE_OPTIONS'):
        env.pop(key, None)

    def plum(*args, ok=True, overlay=None):
        return run([COMPILER, *args], env=env | (overlay or {}), ok=ok)

    args = ('build', app, '--target', TARGET, '--emit-object', '--link-asm', assembly, '-o', obj)
    first = plum(*args)
    assert 'IR miss' in first.stderr
    header = run(['readelf', '-h', obj]).stdout
    assert 'REL (Relocatable file)' in header and 'X86-64' in header
    symbols = run(['nm', obj]).stdout
    assert re.search(r'\b[Tt] helper$', symbols, re.M)
    assert re.search(r'\bU plum_kernel_write$', symbols, re.M), symbols
    assert 'plum_set_utf8_locale' not in symbols
    assert re.search(r'0000000000000017 A asm_marker', symbols)
    leaf = run(['objdump', '-d', '--disassemble=leaf', obj]).stdout
    assert re.search(r'sub.*%rsp', leaf), leaf  # stack allocation, not the red zone
    explicit = work / 'explicit input.c'
    explicit.write_text('long explicit_helper(void) { return 11; }\n')
    plum(*args, '--link-c', explicit)
    assert re.search(r'\b[Tt] explicit_helper$', run(['nm', obj]).stdout, re.M)
    second = plum(*args)
    assert 'IR hit' in second.stderr
    # Object caching is conservative on unsupported hosts; on certified Linux
    # this also proves the effective freestanding command is reusable.
    if sys.platform.startswith('linux') and 'object miss' in first.stderr:
        assert 'object hit' in second.stderr, second.stderr
    (native / 'value.h').write_text('#define VALUE 9\n')
    changed = plum(*args)
    assert 'IR hit' in changed.stderr
    assert 'object miss 1' in changed.stderr or 'objects bypass' in changed.stderr
    (work / 'asm_value.h').write_text('#define ASM_VALUE 29\n')
    plum(*args)
    assert re.search(r'000000000000001d A asm_marker', run(['nm', obj]).stdout)
    assert 'IR miss' in plum(*args, '--release').stderr
    assert 'IR hit' in plum(*args, overlay={'PLUM_NO_OBJECT_CACHE': '1'}).stderr
    custom = plum(*args, overlay={'PLUM_CC': 'clang'})
    assert 'IR hit' in custom.stderr and 'objects bypass' in custom.stderr
    assert 'IR bypass' in plum(*args, overlay={'PLUM_NO_BUILD_CACHE': '1'}).stderr

    script = work / 'kernel.ld'
    script.write_text('ENTRY(_start)\nSECTIONS { . = 0xffffffff80000000; '
                      '.text : { *(.text*) } .rodata : { *(.rodata*) } '
                      '.data : { *(.data*) } .bss : { *(.bss*) } }\n')
    for mode in ([], ['--release']):
        # Release has no original DWARF module flags. Check both the direct
        # emitter and the IR actually retained after a missing-hook link fails.
        emitted = plum('emit-llvm', app, '--target', TARGET, *mode).stdout
        check_ir(emitted, release=bool(mode))
        out = work / ('release-link-failure' if mode else 'debug-link-failure')
        saved = out.with_suffix('.ll')
        failed = plum('build', app, '--target', TARGET, '--linker-script', script,
                      '-o', out, *mode, ok=False)
        assert 'undefined symbol: plum_kernel_write' in failed.stderr, failed.stderr
        assert f'IR left at {saved}' in failed.stderr, failed.stderr
        assert not out.exists(), out
        check_ir(saved.read_text(), release=bool(mode))

    empty = work / 'empty.plum'
    empty.write_text('let main (): Unit = ()\n')
    for flag in ('--memory-stats', '--trace'):
        failed = plum('emit-llvm', empty, '--target', TARGET, flag, ok=False)
        assert 'freestanding IR emission' in failed.stderr and flag in failed.stderr, failed

    start = work / 'start.S'
    start.write_text('.text\n.global _start\n_start:\ncli\nhlt\njmp _start\n')
    start_obj = work / 'start.o'
    run(['clang', '--target=' + TARGET, '-c', start, '-o', start_obj])
    elf = work / 'kernel.elf'
    link = ('link', '--target', TARGET, '--linker-script', script, '--link-object', start_obj, '-o', elf)
    # Object emission also incorporates explicitly supplied precompiled objects.
    plum(*args, '--link-object', start_obj)
    assert re.search(r'\b[Tt] _start$', run(['nm', obj]).stdout, re.M)
    plum(*link)
    header = run(['readelf', '-h', elf]).stdout
    assert 'EXEC (Executable file)' in header and '0xffffffff80000000' in header
    assert 'INTERP' not in run(['readelf', '-l', elf]).stdout
    assert not run(['nm', '-u', elf]).stdout.strip()
    assert 'NEEDED' not in run(['readelf', '-d', elf]).stdout
    script.write_text(script.read_text().replace('80000000', '80001000'))
    plum(*link)
    assert '0xffffffff80001000' in run(['readelf', '-h', elf]).stdout
    # Prebuilt objects are relinked from current contents rather than a cached ELF.
    start.write_text('.text\n.global _start\n_start:\ncall missing_symbol\n')
    run(['clang', '--target=' + TARGET, '-c', start, '-o', start_obj])
    assert 'missing_symbol' in plum(*link, ok=False).stderr

    bad_cases = [
        (('build', app, '--target', 'i686-unknown-none', '--emit-object'), 'require'),
        (('build', app, '--target', 'aarch64-unknown-none'), 'unsupported target'),
        (('build', app, '--emit-object'), 'require'),
        (('build', app, '--target', TARGET), '--linker-script'),
        (('build', app, '--target', TARGET, '--emit-object', '--trace'), 'unsupported freestanding'),
        (('build', app, '--target', TARGET, '--emit-object', '--sanitize', 'address'), 'unsupported freestanding'),
        (('build', app, '--target', TARGET, '--emit-object', '--memory-stats'), 'unsupported freestanding'),
        (('build', app, '--target', TARGET, '--emit-object', '--link-asm'), 'requires a value'),
        (('build', app, '--target', TARGET, '--emit-object', '--linker-script', script), 'cannot be combined'),
        (('build', app, '--target', TARGET, '--emit-object', '--link-lib', 'm'), 'unsupported freestanding'),
        (('link', '--target', TARGET, '--linker-script', script), 'at least one'),
        (('link', '--target', TARGET, '--linker-script', script, '--link-c', native / 'helper.c'), 'inputs only'),
        (('link', '--target', TARGET, '--linker-script', script, '--link-object'), 'requires a value'),
        (('link', '--target', TARGET, '--target', TARGET, '--linker-script', script), 'only once'),
    ]
    for argv, expected in bad_cases:
        assert expected in plum(*argv, ok=False).stderr, argv
    # An actual hosted executable still compiles and executes after the target
    # switch. POSIX exclusion applies only to the bare-metal build.
    (native / 'posix' / 'host.c').write_text('long posix_helper(void) { return 0; }\n')
    hosted = plum('run', app)
    assert hosted.stdout.strip() == '9' and 'IR miss' in hosted.stderr, hosted
    assert 'IR hit' in plum('run', app).stderr
    assert 'IR hit' in plum(*args).stderr
    print('freestanding-check: object/ELF inspection, IR attributes, native inputs, '
          'cache invalidation, diagnostics and hosted execution passed')
