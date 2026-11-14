#!/usr/bin/env python3
"""Fixed integer typing, checked lowering, unsigned formatting and ABI boundaries."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
COMPILER = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / 'sh.real').resolve()
KINDS = {
    'I8': (-128, 127), 'I16': (-32768, 32767),
    'I32': (-(2**31), 2**31 - 1), 'U8': (0, 255),
    'U16': (0, 65535), 'U32': (0, 2**32 - 1), 'U64': (0, 2**64 - 1),
}


def main():
    with tempfile.TemporaryDirectory(prefix='plum-fixed-integer-') as tmp:
        work = Path(tmp)
        project = work / 'app'
        project.mkdir()
        source = project / 'main.plum'
        env = dict(os.environ, PLUM_CACHE=str(work / 'cache'))

        def run(*args, ok=True, case=None):
            result = subprocess.run([str(a) for a in args], capture_output=True,
                                    text=True, env=dict(env, PLUM_FIXED_CASE=case or ''),
                                    cwd=ROOT, timeout=180)
            assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
            return result

        def reject(body, *messages):
            source.write_text(body)
            for action in ('check', 'emit-llvm'):
                failed = run(COMPILER, action, project, ok=False)
                for message in messages:
                    assert message in failed.stdout + failed.stderr, (body, failed)

        fixture = ROOT / 'bootstrap/exec_corpus/fixed_integers'
        for mode in ([], ['--release']):
            exe = work / 'fixed-integers'
            run(COMPILER, 'build', fixture, *mode, '-o', exe)
            assert run(exe).stdout == (fixture / 'expected.txt').read_text()

        # Check all typed-value pairs; only identity is implicit. Bare literal
        # arguments are contextual; a let-bound default Int is a typed value.
        types = ['Int', *KINDS]
        for src in types:
            for dst in types:
                source.write_text(f'let take (value: {dst}): Unit = ()\n'
                                  f'let main (): Unit = {{ let value: {src} = 1; take(value) }}\n')
                result = run(COMPILER, 'check', project, ok=src == dst)
                if src != dst:
                    assert src in result.stderr + result.stdout and dst in result.stderr + result.stdout, result
        for kind, (minimum, maximum) in KINDS.items():
            for value in (minimum - 1, maximum + 1):
                reject(f'let main (): Unit = {{ let value: {kind} = {value}; () }}\n',
                       'integer literal out of range')
            reject(f'let main (): Unit = {{ let value = 1; let narrowed: {kind} = value; () }}\n',
                   kind, 'Int')
            reject(f'let main (): Unit = {{ let value: {kind}[Int] = 1; () }}\n',
                   'type argument')
            for op in ('&', '|', '^', '<<', '>>'):
                reject(f'let main (): Unit = {{ let value: {kind} = 1; let bits = value {op} value; () }}\n',
                       'needs Int operands')
            if minimum == 0:
                reject(f'let main (): Unit = {{ let value: {kind} = 1; let negative = -value; () }}\n',
                       'unsigned integer negation')
            reject(f'extern "C" {{ fn native(value: {kind}) -> Unit; }}\nlet main (): Unit = ()\n',
                   'extern fixed-width integer ABI is not supported yet', kind)
            reject(f'extern "C" {{ fn native() -> {kind}; }}\nlet main (): Unit = ()\n',
                   'extern fixed-width integer ABI is not supported yet', kind)
        reject('struct Inner { value: U8 }\nstruct Outer { inner: Inner }\n'
               'extern "C" { fn native(value: Outer) -> Unit; }\nlet main (): Unit = ()\n',
               'extern fixed-width integer ABI is not supported yet', 'U8')
        reject('enum Value { Number(U64), Empty }\n'
               'extern "C" { fn native(value: Value) -> Unit; }\nlet main (): Unit = ()\n',
               'extern fixed-width integer ABI is not supported yet', 'U64')
        # Pattern bounds use the scrutinee kind, including high-half U64.
        reject('let main (): Unit = { let value: U8 = 0; match value { 256 => (), _ => () } }\n',
               'integer literal out of range', 'U8')
        # Type identity also reaches editor queries; distinguish it from Int.
        text = 'let main (): Unit = { let value: U64 = 18446744073709551615; println(value.to_string()) }\n'
        source.write_text(text)
        offset = text.index('value.to_string')
        hover = json.loads(run(COMPILER, 'query', project, source, 1, offset + 1).stdout)
        assert 'U64' in hover['type'], hover

        # Contextual ECall inference must record the callee exactly once;
        # the ordinary Int fallback already records its own call reference.
        text = ('let identity[T] (value: T): T = value\n'
                'let main (): Unit = {\n'
                '    let byte: U8 = identity(255);\n'
                '    let ordinary = identity(1);\n'
                '    let piped: U8 = 255 |> identity;\n'
                '    println("${byte} ${ordinary} ${piped}")\n}\n')
        source.write_text(text)
        references = json.loads(run(COMPILER, 'references', project, source, 1, 5, 'false').stdout)
        offsets = [text.index('identity(255)'), text.index('identity(1)'), text.index('|> identity') + 3]
        expected_sites = {(text[:off].count('\n'), off - text.rfind('\n', 0, off) - 1) for off in offsets}
        sites = {(ref['range']['start']['line'], ref['range']['start']['character']) for ref in references}
        assert len(references) == 3 and sites == expected_sites, references
        assert all(ref['uri'] == source.as_uri() for ref in references), references

        # Global re-inference must restore the caller's error context before
        # checking the following statement, even after visiting another file.
        globals_dir = project / 'globals'
        globals_dir.mkdir()
        (globals_dir / 'globals.plum').write_text('pub let VALUE: U8 = 255\n')
        reject('use globals;\nlet main (): Unit = {\n'
               '    let value = globals.VALUE;\n    let wrong: U8 = true;\n}\n',
               f'{source}:4:', 'Bool')
        (globals_dir / 'globals.plum').unlink()
        globals_dir.rmdir()

        # One binary per mode, many independently selected failure paths. Keep
        # operands behind calls and check that execution stops before 'after'.
        functions, arms, failures = [], [], {}
        for kind, (minimum, maximum) in {**KINDS, 'Int': (-(2**63), 2**63 - 1)}.items():
            operations = [('+', maximum, 1, 'overflow'), ('-', minimum, 1, 'overflow'),
                          ('*', maximum, 2, 'overflow'), ('/', 1, 0, 'division by zero'),
                          ('%', 1, 0, 'division by zero')]
            if minimum < 0:
                operations += [('/', minimum, -1, 'overflow'), ('%', minimum, -1, 'overflow')]
            for index, (op, a, b, message) in enumerate(operations):
                name = f'fail_{kind}_{index}'
                functions.append(f'let {name} (a: {kind}) (b: {kind}): {kind} = a {op} b')
                arms.append(f'"{name}" => println({name}({a}, {b}).to_string())')
                failures[name] = message
            if minimum < 0:
                name = f'fail_{kind}_neg'
                functions.append(f'let {name} (a: {kind}): {kind} = -a')
                arms.append(f'"{name}" => println({name}({minimum}).to_string())')
                failures[name] = 'overflow'
        source.write_text('use Os;\n' + '\n'.join(functions) + '\nlet main (): Unit = { println("before"); '
                          'match Option.unwrap_or(Os.env_var("PLUM_FIXED_CASE"), "") { ' + ', '.join(arms) +
                          ', _ => () }; println("after") }\n')
        for mode in ([], ['--release']):
            exe = work / 'failures'
            run(COMPILER, 'build', project, *mode, '-o', exe)
            for case, message in failures.items():
                failed = run(exe, ok=False, case=case)
                assert failed.stdout.startswith('before\n') and 'after' not in failed.stdout, (case, failed)
                assert message in failed.stdout + failed.stderr, (case, failed)

        # Exercise the full language fixture through the core profile with
        # hosted test hooks. This is a runtime adapter, not a bootable OS.
        hooks = work / 'hooks.c'
        hooks.write_text(r'''#include <stdint.h>
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
''')
        for mode in ([], ['--release']):
            obj = work / 'fixed.o'
            run(COMPILER, 'build', fixture, '--target', 'x86_64-unknown-none',
                '--emit-object', *mode, '-o', obj)
            symbols = run('nm', '-u', obj).stdout
            allowed = {'plum_kernel_alloc', 'plum_kernel_free', 'plum_kernel_write',
                       'plum_kernel_panic', 'plum_kernel_halt', 'memcpy', 'memmove', 'memset', 'memcmp'}
            assert {line.split()[-1] for line in symbols.splitlines()} <= allowed, symbols
            exe = work / 'core'
            run('clang', '-no-pie', obj, hooks, '-o', exe)
            assert run(exe).stdout == (fixture / 'expected.txt').read_text()
    print(f'fixed-integer-check: type identities, contexts, ranges, ABI rejection and '
          f'{len(failures) * 2} checked failures pass')


if __name__ == '__main__':
    main()
