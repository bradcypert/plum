#!/usr/bin/env python3
"""Verify real generated/native sanitizer failures, leak detection and caches."""
import os
from pathlib import Path
import re
import runpy
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
compiler = str(Path(sys.argv[1] if len(sys.argv) > 1 else './sh.real').resolve())
fixtures = root / 'bootstrap/sanitizer_corpus'


def main():
    with tempfile.TemporaryDirectory(prefix='plum-sanitizer-check-') as work:
        env = dict(os.environ, ASAN_OPTIONS='detect_leaks=1:halt_on_error=1:symbolize=1',
                   LSAN_OPTIONS='exitcode=23', PLUM_CACHE=str(Path(work) / 'cache'), PLUM_CACHE_TRACE='1')
        env.pop('PLUM_CC', None)

        def run(args, *, fails=False, contains=None, environment=None):
            result = subprocess.run([str(a) for a in args], cwd=root, env=environment or env,
                                    capture_output=True, text=True, timeout=300)
            output = result.stdout + result.stderr
            if (result.returncode != 0) != fails or (contains and contains not in output):
                raise RuntimeError(f'{args}: exit {result.returncode}\n{output[-12000:]}')
            return result

        def build(name, *flags):
            binary = Path(work) / (name + str(build.count))
            build.count += 1
            run([compiler, 'build', fixtures / name, '--sanitize', 'address', *flags, '-o', binary])
            return binary
        build.count = 0

        # A unique heap string must survive every earlier operand and be
        # transferable at its last use. Share the allocation guard's source
        # generator so its sibling-read case also executes under ASan/LSan.
        future = Path(work) / 'liveness-future'
        future.mkdir()
        future_source = runpy.run_path(str(root / 'bootstrap/liveness-memory-check.py'))['source']
        (future / 'main.plum').write_text(future_source(64))
        for flags in [[], ['--release']]:
            binary = Path(work) / ('liveness-future-' + str(len(flags)))
            run([compiler, 'build', future, '--sanitize=address', *flags, '-o', binary])
            run([binary], contains='ok\n')

        block_source = runpy.run_path(str(root / 'bootstrap/block-memory-check.py'))['source']
        for kind in ('expr', 'let', 'assign'):
            project = Path(work) / ('block-' + kind)
            project.mkdir()
            (project / 'main.plum').write_text(block_source(kind, 32))
            for flags in [[], ['--release']]:
                binary = Path(work) / ('block-' + kind + str(len(flags)))
                run([compiler, 'build', project, '--sanitize=address', *flags, '-o', binary])
                run([binary], contains='ok\n')

        # The original missing attribute must be observable in generated IR,
        # rather than only testing native malloc/free interceptors.
        for flags in [[], ['--release']]:
            result = run([build('invalid_plum', *flags)], fails=True, contains='heap-use-after-free')
            if not re.search(r'\bin plum\.f\.\.main\.0\.[^\n]*\bmain\.plum:\d+', result.stderr):
                raise RuntimeError('generated Plum failure has no symbolized source line:\n' + result.stderr)
        run([build('invalid_native')], fails=True, contains='heap-buffer-overflow')
        run([build('leak')], fails=True, contains='LeakSanitizer: detected memory leaks')
        run([build('clean')], contains='native=1')
        run([compiler, 'test', fixtures / 'invalid_plum', '--sanitize=address'], fails=True, contains='heap-use-after-free')
        run([compiler, 'test', fixtures / 'clean', '--sanitize', 'address'], contains='1 passed; 0 failed')

        # CStr is an owner, including through aggregates and tail calls. Track
        # real allocations as well as leaks: conversions must not allocate a
        # view, and repeated temporary borrows must restore live-byte baselines.
        owned = root / 'bootstrap/exec_corpus/cstr_ownership'
        for flags in [[], ['--release']]:
            binary = Path(work) / ('cstr-owned' + str(len(flags)))
            run([compiler, 'build', owned, '--sanitize=address', '--memory-stats', *flags, '-o', binary])
            run([binary], contains='owned CStr: storage, aliases, closures, tail calls and bounded memory')
            trim = Path(work) / ('trim-owned' + str(len(flags)))
            run([compiler, 'build', root / 'bootstrap/exec_corpus/trim_unicode', '--sanitize=address', '--memory-stats', *flags, '-o', trim])
            run([trim], contains='directional trims share Unicode whitespace, preserve UTF-8/NULs and immutable aliases')
            literal = Path(work) / ('literal-emission' + str(len(flags)))
            run([compiler, 'build', root / 'bootstrap/exec_corpus/literal_emission', '--sanitize=address', '--memory-stats', *flags, '-o', literal])
            run([literal], contains='literal emission preserves sections, order, aliases and reuse')
            for name, message in [('string_slice', 'slices preserve codepoint bounds, raw bytes, aliases and bounded stack'),
                                  ('array_set_ownership', 'Array.set: consuming reuse, persistent snapshots, operand aliases and scalar/heap ownership'),
                                  ('array_callback_ownership', 'Array callbacks: receiver captures, operand order, immutable aliases and consuming reuse'),
                                  ('string_char_len', 'character counts preserve UTF-8, NULs, aliases and bounded stack'),
                                  ('statement_cleanup_order', 'statement cleanup preserves LIFO groups, tail calls and escaped owners'),
                                  ('branch_emission', 'branch emission preserves guards, sections, effects and tail calls'),
                                  ('nested_resource_scope', 'nested resources close at scope exit'),
                                  ('ref_payload_release', 'reference payload destructors are available'),
                                  ('symbol_modules', 'punctuation-distinct modules retain separate'),
                                  ('symbol_types', 'nominal types and nested generic shapes'),
                                  ('symbol_functions', 'specializations, associated functions and runtime helpers'),
                                  ('symbol_native', 'C extern names retain their native spelling')]:
                binary = Path(work) / (name + str(len(flags)))
                run([compiler, 'build', root / 'bootstrap/exec_corpus' / name, '--sanitize=address', '--memory-stats', *flags, '-o', binary])
                run([binary], contains=message)
        ir = run([compiler, 'emit-llvm', owned]).stdout
        tail = re.search(r'define [^\n]*@plum\.f\.\.tail\.0\.\([^\n]*\).*?\n}', ir, re.S)
        if not tail or not re.search(r'musttail call [^\n]*@plum\.f\.\.tail\.0\.\(', tail.group()):
            raise RuntimeError('owned CStr forwarding lost its cyclic tail call')

        # IR and native-object cache identity includes instrumentation. Use
        # both mode transitions, then a real warm hit in each mode.
        for address, hit in [(False, False), (True, False), (True, True), (False, True)]:
            flags = ['--sanitize=address'] if address else []
            result = run([compiler, 'run', fixtures / 'clean', *flags, '--', '--sanitize', 'child'],
                         contains=f'native={int(address)}\n--sanitize|child\n')
            if ('IR hit' in result.stderr) != hit:
                raise RuntimeError('wrong instrumentation cache identity:\n' + result.stderr)
            if sys.platform.startswith('linux') and hit and 'object hit' not in result.stderr:
                raise RuntimeError('no native object reuse in warmed supported configuration:\n' + result.stderr)

        for source in [fixtures / 'clean', fixtures / 'clean/main.plum']:
            result = run([compiler, 'emit-llvm', source, '--release', '--sanitize=address'])
            definitions = [line for line in result.stdout.splitlines() if line.startswith('define ')]
            if not definitions or any('sanitize_address' not in line or '"frame-pointer"="all"' not in line for line in definitions):
                raise RuntimeError('a generated function bypasses sanitizer instrumentation')
        for bad, message in [(['--sanitize'], 'requires a value'),
                             (['--sanitize=thread'], 'unsupported sanitizer'),
                             (['--sanitize', 'address', '--target', 'aarch64-windows-gnu'], 'requires a native target')]:
            run([compiler, 'build', fixtures / 'clean', *bad], fails=True, contains=message)

        runner = root / 'bootstrap/sanitize-run'
        run([runner, '--plum', compiler, fixtures / 'clean'], contains='native=1')
        run([runner, '--plum', compiler, fixtures / 'leak'], fails=True, contains='LeakSanitizer: detected memory leaks')
        run([runner, '--plum', compiler, fixtures / 'invalid_plum'], fails=True, contains='heap-use-after-free')
        # An ambient disabled detector/zero exit status cannot make findings pass.
        hostile = dict(env, ASAN_OPTIONS='detect_leaks=0:exitcode=0', LSAN_OPTIONS='exitcode=0')
        run([runner, '--plum', compiler, fixtures / 'leak'], fails=True,
            contains='LeakSanitizer: detected memory leaks', environment=hostile)
        # Simulate a driver/runtime that accepts the build but emits no leak
        # diagnostics. The runner must fail its preflight, not run the project.
        broken = Path(work) / 'broken-compiler'
        broken.write_text(os.linesep.join([
            '#!/usr/bin/env python3', 'from pathlib import Path', 'import sys',
            'p=Path(sys.argv[sys.argv.index("-o")+1])',
            'p.write_text(chr(10).join(["#!/bin/sh", "exit 0", ""]))',
            'p.chmod(0o755)', '',
        ]))
        broken.chmod(0o755)
        run([runner, '--plum', broken, fixtures / 'clean'], fails=True, contains='leak detection preflight failed')
        run([sys.executable, root / 'bootstrap/native-buffer-check.py', compiler],
            contains='escape detection and panic cleanup passed')
        run([sys.executable, root / 'bootstrap/qualified-name-check.py', compiler, '--sanitize'],
            contains='qualified-name-check: boundaries, ownership and allocation budgets passed')
        run([sys.executable, root / 'bootstrap/parse-int-check.py', compiler, '--sanitize'],
            contains='parse-int-check: values, diagnostics, aliases and bounded allocation passed')
        run([sys.executable, root / 'bootstrap/symbol-encoding-check.py', compiler, '--sanitize'],
            contains='symbol-encoding-check: byte identities, ownership and allocation budgets passed')
    print('sanitizer-check: generated/native access failures, leaks, clean ownership, symbols, cache isolation and fail-closed runner passed')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        sys.exit(str(error))
