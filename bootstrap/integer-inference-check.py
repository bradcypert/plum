#!/usr/bin/env python3
"""Integer alias and literal front-end compatibility checks (part of #527)."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
COMPILER = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / 'sh.real').resolve()


def main():
    with tempfile.TemporaryDirectory(prefix='plum-integer-inference-') as tmp:
        work = Path(tmp)
        env = dict(os.environ, PLUM_CACHE=str(work / 'cache'))

        def run(*args, ok=True):
            result = subprocess.run([str(a) for a in args], capture_output=True,
                                    text=True, env=env, timeout=180, cwd=ROOT)
            assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
            return result

        for name in ('i64_alias', 'integer_literal_bounds'):
            fixture = ROOT / 'bootstrap/exec_corpus' / name
            assert run(COMPILER, 'check', fixture).stdout.strip() == 'ok'
            for mode in ([], ['--release']):
                exe = work / 'program'
                run(COMPILER, 'build', fixture, *mode, '-o', exe)
                assert run(exe).stdout == (fixture / 'expected.txt').read_text()
        for name in ('i64_alias_arity', 'i64_alias_mismatch', 'i64_alias_duplicate'):
            fixture = ROOT / 'bootstrap/typecheck_corpus' / name
            for action in ('check', 'emit-llvm'):
                failed = run(COMPILER, action, fixture, ok=False)
                for line in (fixture / 'expected-error.txt').read_text().splitlines():
                    assert line in failed.stdout + failed.stderr, failed
        for mode in ([], ['--release']):
            exe = work / 'overflow'
            run(COMPILER, 'build', ROOT / 'bootstrap/abort_corpus/i64_alias_overflow', *mode, '-o', exe)
            failed = run(exe, ok=False)
            assert failed.stdout.startswith('before\n'), failed
            assert 'integer overflow' in failed.stdout + failed.stderr, failed
            assert 'unreachable' not in failed.stdout, failed

        project = work / 'app'
        project.mkdir()
        source = project / 'main.plum'
        # Preserve all magnitudes through U64::MAX in tokens/AST. They still
        # fail as default Int: this patch does not claim U64 type support.
        for value in ('9223372036854775808', '18446744073709551615'):
            source.write_text('let main (): Unit = println(' + value + '.to_string())\n')
            assert f'Int({value})' in run(COMPILER, 'tokens', source).stdout
            assert value in run(COMPILER, 'ast', source).stdout
            failed = run(COMPILER, 'check', project, ok=False)
            assert 'integer literal out of range' in failed.stdout + failed.stderr, failed
        for value in ('-9223372036854775809', '18446744073709551616'):
            source.write_text('let main (): Unit = println(' + value + '.to_string())\n')
            failed = run(COMPILER, 'check', project, ok=False)
            assert 'integer literal out of range' in failed.stdout + failed.stderr, failed
        # The type checker now owns signed range errors. Preserve original
        # character offsets, including Unicode, nested interpolation and CRLF.
        for ending in ('\n', '\r', '\r\n'):
            for literal in ('9223372036854775808', '"${"${9223372036854775808}"}"'):
                line = '    let prefix = "é😀 9223372036854775808"; println(' + literal + ');'
                source.write_bytes(ending.join(['let main (): Unit = {', line, '}']).encode())
                col = line.rindex('9223372036854775808') + 1
                for action in ('check', 'emit-llvm'):
                    failed = run(COMPILER, action, project, ok=False)
                    output = failed.stdout + failed.stderr
                    assert f'{source}:2:{col}' in output and 'integer literal out of range' in output, output
        # Qualified nominal names must not be reinterpreted as primitive aliases
        # by the C declaration emitter. Native record layout remains unsupported.
        types = project / 'types'
        types.mkdir()
        nominal = types / 'types.plum'
        nominal.write_text('pub struct I64 { pub value: Int }\n')
        source.write_text('use types;\nextern "C" { fn probe(value: types.I64) -> Unit; }\n'
                          'let main (): Unit = unsafe { probe(types.I64 { value: 1 }) }\n')
        failed = run(COMPILER, 'emit-llvm', project, ok=False)
        assert 'extern declarations can only use' in failed.stdout + failed.stderr, failed
        assert 'types.I64' in failed.stdout + failed.stderr, failed
        nominal.unlink()
        types.rmdir()

        # Alias-owned methods remain discoverable under the canonical Int type.
        text = ('let I64.alias_step (value: I64): Int = value + 1\n'
                'let main (): Unit = { let number: Int = 41; println(number.alias_step().to_string()) }\n')
        source.write_text(text)
        offset = text.index('number.alias_step') + len('number.')
        line, col = text[:offset].count('\n') + 1, offset - text.rfind('\n', 0, offset)
        members = json.loads(run(COMPILER, 'members', project, source, line, col).stdout)
        assert any(item['name'] == 'alias_step' for item in members), members
        hover = json.loads(run(COMPILER, 'query', project, source, line, col).stdout)
        assert hover['name'] == 'alias_step' and 'I64.alias_step' in hover['type'], hover
        template = 'let main (): Unit = { let value: TYPE = 41; println((value + 1).to_string()) }\n'
        ir = []
        for kind in ('Int', 'I64'):
            source.write_text(template.replace('TYPE', kind))
            ir.append(run(COMPILER, 'emit-llvm', project, '--release').stdout)
        assert ir[0] == ir[1], 'alias changed emitted IR'
    print('integer-inference-check: aliases, signed bounds, preserved magnitudes, diagnostics and editor queries passed')


if __name__ == '__main__':
    main()
