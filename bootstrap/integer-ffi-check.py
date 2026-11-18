#!/usr/bin/env python3
"""C compiler ABI oracle, scalar round trips and target-sensitive IR caching."""
import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
FIXTURE = ROOT / 'bootstrap/exec_corpus/integer_scalar_ffi'
KINDS = ('I8', 'I16', 'I32', 'Int', 'I64', 'U8', 'U16', 'U32', 'U64')
TARGETS = ('x86_64-unknown-linux-gnu', 'x86_64-apple-darwin',
           'x86_64-w64-windows-gnu', 'aarch64-unknown-linux-gnu',
           'arm64-apple-darwin', 'aarch64-pc-windows-msvc', 'x86_64-unknown-none')
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('compiler', nargs='?', default='./sh.real')
    parser.add_argument('--native-only', action='store_true')
    parser.add_argument('--core', action='store_true')
    parser.add_argument('--cross-run', action='store_true')
    options = parser.parse_args()
    compiler = Path(options.compiler).resolve()
    expected = (FIXTURE / 'expected.txt').read_text()
    with tempfile.TemporaryDirectory(prefix='plum-integer-ffi-') as tmp:
        work = Path(tmp)
        env = dict(os.environ, PLUM_CACHE=str(work / 'cache'))
        for name in ('PLUM_CC', 'PLUM_NO_BUILD_CACHE', 'PLUM_NO_OBJECT_CACHE'):
            env.pop(name, None)

        def run(*args, ok=True, overlay=None):
            result = subprocess.run(list(map(str, args)), cwd=ROOT,
                                    env=env | (overlay or {}), capture_output=True,
                                    text=True, timeout=300)
            assert (result.returncode == 0) == ok, (args, result.returncode, result.stdout, result.stderr)
            return result

        for mode in ([], ['--release']):
            exe = work / 'native.exe'
            run(compiler, 'build', FIXTURE, *mode, '-o', exe)
            assert run(exe).stdout == expected
        print('integer-ffi-check: native signed/unsigned boundaries, mixed stack arguments and legacy scalars pass debug/release', flush=True)
        if options.native_only:
            return

        # Header-free declarations let the installed Clang frontend describe
        # each target's ABI without an SDK. Compare both declarations and call
        # operands; LLVM integer widths alone are not the ABI contract.
        oracle = work / 'oracle.c'
        ctypes = {'Int': 'long long', 'I64': 'long long', 'U64': 'unsigned long long',
                  'I8': 'signed char', 'I16': 'short', 'I32': 'int',
                  'U8': 'unsigned char', 'U16': 'unsigned short', 'U32': 'unsigned int'}
        oracle.write_text('\n'.join(
            f'extern {ctypes[k]} ffi_echo_{k.lower()}({ctypes[k]}); '
            f'{ctypes[k]} call_{k.lower()}({ctypes[k]} x) {{ return ffi_echo_{k.lower()}(x); }}'
            for k in KINDS))

        def signature(ir, kind):
            match = re.search(r'^declare (?:dso_local )?(?:(signext|zeroext) )?i(\d+) '
                              r'@ffi_echo_' + kind.lower() + r'\(i\d+([^)]*)\)', ir, re.M)
            assert match, (kind, ir[:1000])
            ret_attr, bits, param = match.groups()
            param_attrs = re.findall(r'\b(?:signext|zeroext)\b', param)
            assert len(param_attrs) <= 1
            return ret_attr or '', bits, param_attrs[0] if param_attrs else ''

        for target in TARGETS:
            c_ir = run('clang', '--target=' + target, '-S', '-emit-llvm', '-O0', oracle, '-o', '-').stdout
            plum_ir = run(compiler, 'emit-llvm', FIXTURE, '--target', target).stdout
            for kind in KINDS:
                actual = signature(plum_ir, kind)
                assert actual == signature(c_ir, kind), (target, kind, actual, signature(c_ir, kind))
                ret_attr, bits, param_attr = actual
                ret = (ret_attr + ' ' if ret_attr else '') + 'i' + bits
                arg = 'i' + bits + ' ' + (param_attr + ' ' if param_attr else '')
                assert re.search(r'call ' + ret + r' @ffi_echo_' + kind.lower() + r'\(' + arg, plum_ir), (target, kind)
            ir_file = work / 'target.ll'
            ir_file.write_text(plum_ir)
            run('clang', '--target=' + target, '-Wno-override-module', '-c', ir_file, '-o', work / 'target.o')
            print(f'integer-ffi-check: C ABI declarations/calls and object emission match Clang for {target}', flush=True)

        project = work / 'invalid'
        project.mkdir()
        source = project / 'main.plum'

        def reject(text, message):
            source.write_text(text)
            for action in ('check', 'emit-llvm'):
                failed = run(compiler, action, project, ok=False)
                assert message in failed.stdout + failed.stderr, failed

        for ty, declarations in (
            ('(U8) -> U8', ''), ('(Int) -> U64', ''), ('Array[U8]', ''),
            ('Outer', 'struct Inner { value: U8 }\nstruct Outer { inner: Inner }\n'),
            ('Value', 'enum Value { Number(U64), Empty }\n'),
        ):
            for signature_text in (f'fn native(value: {ty});', f'fn native() -> {ty};'):
                reject(declarations + 'extern "C" { ' + signature_text + ' }\nlet main (): Unit = ()\n',
                       'extern aggregates and callbacks containing fixed-width integers are not supported')
        reject('extern "C" { fn native(x: U8) -> U8; }\nlet main (): Unit = { let f = native; () }\n',
               'cannot be used as a function value')
        reject('extern "C" { fn native(x: U8, y: U8) -> U8; }\nlet main (): Unit = unsafe { let f = native(1); () }\n',
               'expected 2 argument(s)')
        reject('extern "C" { fn native(x: U8) -> U8; }\nlet main (): Unit = { native(1); () }\n',
               "requires an 'unsafe' block")
        source.write_text('extern "C" { fn native(x: U8) -> U8; }\nlet main (): Unit = ()\n')
        failure = run(compiler, 'emit-llvm', project, '--target', 'riscv64-unknown-linux-gnu', ok=False)
        assert 'fixed-width scalar C ABI is not supported for target' in failure.stdout + failure.stderr
        missing_driver = {'PLUM_CC': str(work / 'missing-c-driver')}
        emitted = run(compiler, 'emit-llvm', FIXTURE / 'main.plum', '--target', TARGETS[0],
                      overlay=missing_driver).stdout
        assert signature(emitted, 'I8') == ('signext', '8', 'signext')
        failure = run(compiler, 'emit-llvm', project, ok=False, overlay=missing_driver)
        assert 'cannot determine the C compiler target' in failure.stdout + failure.stderr
        run(compiler, 'emit-llvm', ROOT / 'bootstrap/exec_corpus/ffi', overlay=missing_driver)

        # Stop the driver at IR handoff. run publishes IR before native
        # compilation, so switching the driver's native target exercises real
        # cache lookup without needing a foreign linker or SDK. build itself
        # bypasses IR caching; verify its explicit target separately.
        driver = work / 'driver.py'
        captured = work / 'captured.ll'
        driver.write_text('import os, pathlib, sys\n'
                          'if "-dumpmachine" in sys.argv:\n    print(os.environ["FFI_TARGET"]); sys.exit(0)\n'
                          'for arg in sys.argv[1:]:\n'
                          '    if arg.endswith(".ll"):\n'
                          '        pathlib.Path(os.environ["FFI_CAPTURE"]).write_bytes(pathlib.Path(arg).read_bytes())\n'
                          'sys.exit(1)\n')
        # The Linux oracle job uses paths without spaces: PLUM_CC currently
        # splits a driver command on spaces.
        overlay = {'PLUM_CC': f'{sys.executable} {driver}', 'FFI_CAPTURE': str(captured),
                   'PLUM_CACHE': str(work / 'handoff-cache'), 'PLUM_CACHE_TRACE': '1'}
        for target, attr, event in (
            ('x86_64-unknown-linux-gnu', 'signext', 'miss'),
            ('x86_64-w64-windows-gnu', '', 'miss'),
            ('aarch64-unknown-linux-gnu', '', 'miss'),
            ('x86_64-w64-windows-gnu', '', 'hit'),
        ):
            failed = run(compiler, 'run', FIXTURE, ok=False, overlay=overlay | {'FFI_TARGET': target})
            assert 'IR ' + event in failed.stderr, failed
            assert signature(captured.read_text(), 'I8')[0] == attr
        for target in ('x86_64-w64-windows-gnu', 'aarch64-unknown-linux-gnu'):
            run(compiler, 'build', FIXTURE, '--target', target, '-o', work / 'handoff', ok=False,
                overlay=overlay | {'FFI_TARGET': 'x86_64-unknown-linux-gnu'})
            assert signature(captured.read_text(), 'I8')[0] == ''
        print('integer-ffi-check: unsupported boundaries rejected and build IR cache isolated by target', flush=True)

        if options.core:
            hooks = work / 'hooks.c'
            hooks.write_text(HOOKS)
            for mode in ([], ['--release']):
                obj = work / 'core.o'
                run(compiler, 'build', FIXTURE, '--target', 'x86_64-unknown-none', '--emit-object', *mode, '-o', obj)
                symbols = {line.split()[-1] for line in run('nm', '-u', obj).stdout.splitlines()}
                assert symbols <= {'plum_kernel_alloc', 'plum_kernel_free', 'plum_kernel_write',
                                   'plum_kernel_panic', 'plum_kernel_halt', 'memcpy', 'memmove', 'memset', 'memcmp', 'strcmp'}, symbols
                exe = work / 'core'
                run('clang', '-no-pie', obj, hooks, '-o', exe)
                assert run(exe).stdout == expected
            print('integer-ffi-check: core debug/release round trips pass with no leaked hook allocations', flush=True)
        if options.cross_run:
            if not shutil.which('zig') or not shutil.which('qemu-aarch64'):
                raise AssertionError('--cross-run requires zig and qemu-aarch64')
            for mode in ([], ['--release']):
                exe = work / 'arm64'
                run(compiler, 'build', FIXTURE, '--target', 'aarch64-linux-musl', *mode, '-o', exe,
                    overlay={'PLUM_CC': 'zig cc', 'ZIG_GLOBAL_CACHE_DIR': str(work / 'zig-global'),
                             'ZIG_LOCAL_CACHE_DIR': str(work / 'zig-local')})
                assert run('qemu-aarch64', exe).stdout == expected
            print('integer-ffi-check: Linux arm64 debug/release round trips run under qemu', flush=True)


if __name__ == '__main__':
    main()
