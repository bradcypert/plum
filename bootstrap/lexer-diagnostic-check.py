#!/usr/bin/env python3
"""Lexer failures retain their file, original source line, and character offset."""
import subprocess
import sys
import tempfile
from pathlib import Path


def run(compiler, command, target):
    return subprocess.run([compiler, command, str(target)], capture_output=True,
                          text=True, timeout=60)


def check(compiler):
    count = 0
    with tempfile.TemporaryDirectory(prefix="plum-lexer-diagnostics-") as directory:
        project = Path(directory)
        (project / "main.plum").write_text('let main (): Unit = println("ok")\n')
        module = project / "nested"
        module.mkdir()
        path = module / "bad.plum"
        # Non-ASCII prefix distinguishes character offsets from byte offsets.
        prefix = '// header\nlet bad (): String = {\n    let prefix = "é😀"; '
        cases = [
            ('99999999999999999999', '99999999999999999999', 'integer literal out of range'),
            ('1e400', '1e400', 'float literal out of range'),
            ('-1e400', '1e400', 'float literal out of range'),
            ('"\\q"', '\\q', 'unknown escape'),
            ('"\\xG0"', '\\xG0', 'requires exactly two hexadecimal digits'),
            ('"\\u1234"', '\\u1234', 'must use the form'),
            ('"\\u{}"', '\\u{}', 'requires 1 to 6 hexadecimal digits'),
            ('"\\u{D800}"', '\\u{D800}', 'not a Unicode scalar value'),
            ('"\\u{110000}"', '\\u{110000}', 'not a Unicode scalar value'),
            ('@', '@', 'expected a builtin name'),
            ('`', '`', 'unexpected character'),
            ('"${99999999999999999999}"', '99999999999999999999', 'integer literal out of range'),
            ('"${1e400}"', '1e400', 'float literal out of range'),
            ('"${"\\q"}"', '\\q', 'unknown escape'),
            ('"${"${1e400}"}"', '1e400', 'float literal out of range'),
            # Repeated raw text must not confuse an index-of based mapping.
            ('"1e400 ${1e400}"', '1e400}', 'float literal out of range'),
            ('"${@}"', '@', 'expected a builtin name'),
            ('"${`}"', '`', 'unexpected character'),
            ('"${1 + }"', '}', 'expected an expression'),
        ]
        for ending in ('\n', '\r', '\r\n'):
            for literal, marker, message in cases:
                source = (prefix + literal + ';\n    ""\n}\n').replace('\n', ending)
                path.write_bytes(source.encode())
                line = (prefix + literal + ';').split('\n')[-1]
                column = line.index(marker) + 1
                expected = 'error:', message, f'{path}:3:{column}', f'3 | {line}', '  | ' + ' ' * (column - 1) + '^'
                for command in ('check', 'emit-llvm'):
                    result = run(compiler, command, project)
                    output = result.stdout + result.stderr
                    assert result.returncode == 1, (command, literal, output)
                    assert all(part in output for part in expected), (command, literal, expected, output)
                    assert '<source>' not in output, output
                    count += 1
        # EOF is deliberate: no final newline or generated closing delimiters.
        eof_cases = [
            ('"plain', '"plain', 'unterminated string literal'),
            ('"${1}', '"${', 'unterminated string literal'),
            ('"${1', '${', 'unterminated string interpolation'),
            ('"${"nested', '"nested', 'unterminated string literal'),
            ('"trailing\\', '\\', 'incomplete escape sequence'),
        ]
        for literal, marker, message in eof_cases:
            source = prefix + literal
            path.write_bytes(source.encode())
            column = source.split('\n')[-1].index(marker) + 1
            for command, target in [('check', project), ('emit-llvm', project),
                                    ('tokens', path), ('fmt', path), ('relex', path)]:
                result = run(compiler, command, target)
                output = result.stdout + result.stderr
                assert result.returncode == 1, (command, literal, output)
                assert message in output and f'{path}:3:{column}' in output, output
                count += 1
        manifest = project / 'plum.pkg'
        for text, marker, message in [('Package { name: "\\q" }', '\\q', 'unknown escape'),
                                      ('Package { version: 1e400 }', '1e400', 'float literal out of range')]:
            manifest.write_text(text)
            result = run(compiler, 'check-manifest', manifest)
            output = result.stdout + result.stderr
            assert result.returncode == 1 and message in output, output
            assert f'{manifest}:1:{text.index(marker) + 1}' in output, output
            count += 1
        manifest.unlink()
        # Valid strings, nesting, and the enclosing parser context still work.
        path.unlink()
        valid = '''let main (): Unit = {
    let number = 7;
    println("${number} ${"${number}"} ${number}");
    println("line one
line two");
}
'''
        main = project / 'main.plum'
        main.write_text(valid)
        result = run(compiler, 'check', project)
        assert result.returncode == 0, result.stdout + result.stderr
        main.write_text(valid.replace('println("line one', 'missing("line one'))
        result = run(compiler, 'check', project)
        assert result.returncode == 1 and f'{main}:4:' in result.stdout + result.stderr, result
    print(f'lexer-diagnostic-check: {count} positioned rejections and parent context restoration pass')


if __name__ == '__main__':
    check(str(Path(sys.argv[1] if len(sys.argv) > 1 else './sh.real').resolve()))
