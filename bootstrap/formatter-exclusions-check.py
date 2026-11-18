#!/usr/bin/env python3
"""Formatter directives preserve source text while surrounding code formats."""
from pathlib import Path
import os
import re
import subprocess
import sys
import tempfile

compiler = str(Path(sys.argv[1]).resolve())
checks = 0


def run(*args):
    result = subprocess.run([compiler, *args], capture_output=True, timeout=30)
    assert result.returncode == 0, result.stdout + result.stderr
    return result.stdout


with tempfile.TemporaryDirectory(prefix="plum-fmt-exclusions-") as temp:
    root = Path(temp)
    source, output = root / "input.plum", root / "output.plum"

    def check(text, expected=None, kept=()):
        global checks
        source.write_bytes(text.encode())
        actual = run("fmt", str(source)).decode()
        if expected is not None:
            assert actual == expected, (actual, expected)
        for region in kept:
            assert region in actual, (region, actual)
        output.write_bytes(actual.encode())
        assert run("fmt", str(output)).decode() == actual, ("not idempotent", actual)
        assert run("relex", str(source), "--tokens") == run("relex", str(output), "--tokens"), "tokens changed"
        checks += 1
        return actual

    whole = '// fmt: off\nlet keep (): Int = { let x = 1;    x }\n// fmt: on\n'
    check(whole, whole)
    # A fully protected file need not be parseable, but it must lex.
    opaque = '// fmt: off\nlet incomplete = (1, [2, 3]\n// fmt: skip\n'
    check(opaque, opaque)
    check("// fmt: skip\n" + opaque, "// fmt: skip\n" + opaque)

    region = ('// fmt: off\nlet aligned (): Int = { let a=1;   a }\n'
              '\t// fmt: off\nlet nested (): Int = 2\n'
              '  // fmt: on\nlet still_off (): Int = 3\n// fmt: on\n')
    actual = check('let before (): Int = { let n = 1; n }\n' + region
                   + '    let after (): Int = { let n = 2; n }\n', kept=[region])
    assert actual.startswith('let before (): Int = {\n    let n = 1;\n    n\n}\n')
    assert actual.endswith('let after (): Int = {\n    let n = 2;\n    n\n}\n'), actual

    local_region = ('    // fmt: off\n'
                    ' let table = [1,   2,\n              3,4];\n'
                    '\t// fmt: on\n')
    actual = check('let local (): Int = { let before = 1;\n' + local_region
                   + '         let after = 2; before + after }\n', kept=[local_region])
    assert '\n    let after = 2;\n    before + after\n}\n' in actual, actual

    skipped = '// fmt: skip\npub let table (): Array[Int] = [1,   2,\n       3,4]\n'
    actual = check(skipped + 'let after (): Int = { let n = 2; n }\n', kept=[skipped])
    assert actual.endswith('let after (): Int = {\n    let n = 2;\n    n\n}\n')
    check('// fmt: skip\npub struct P { pub x: Int, pub y: Int, pub z: Int }\n',
          '// fmt: skip\npub struct P { pub x: Int, pub y: Int, pub z: Int }\n')

    local_skip = '    // fmt: skip\n    let table = [1,   2,\n           3,4];'
    actual = check('let f (): Int = {\n' + local_skip
                   + '\n          let n = 2; n }\n', kept=[local_skip])
    assert '\n    let n = 2;\n    n\n}\n' in actual, actual
    for statement in ['value = consume(1,   2);', 'consume(1,   2);']:
        keep = '    // fmt: skip\n    ' + statement
        actual = check('let f (): Unit = {\n' + keep + ' let n = 2; consume(n) }\n', kept=[keep])
        assert '\n    let n = 2;\n    consume(n)\n}\n' in actual, actual

    # Expression spans select the whole call/array/record, not its first leaf.
    for expression in ['[1,   2,\n       3,4]', 'produce(1,   2)', 'P { x: 1, y: 2, z: 3 }']:
        keep = '    // fmt: skip\n    ' + expression
        actual = check('let f (): R = consume(\n' + keep + ', 8)\n', kept=[keep])
        assert '\n    8\n)' in actual, actual

    # Markers nest, empty regions are valid, and overlapping skip/regions union.
    check('let f (): Int = {\n    // fmt: off\n    // fmt: on\n  1\n}\n',
          'let f (): Int = {\n    // fmt: off\n    // fmt: on\n    1\n}\n')
    overlap = ('// fmt: skip\nlet f (): Int = {\n    // fmt: off\n'
               ' let n=1;\n    // fmt: on\n n }\n')
    check(overlap, overlap)
    trailing = '// fmt: off\nlet keep (): Int = {   1 }\n// fmt: off\n'
    check('let before (): Int = { 0 }\n' + trailing, kept=[trailing])
    check('// fmt: on\nlet n (): Int = { 1 }\n', '// fmt: on\nlet n (): Int = {\n    1\n}\n')
    check('// fmt: skip\n// nothing follows\n', '// fmt: skip\n// nothing follows\n')

    # Neither a trailing comment nor directive-like literal/doc text is a directive.
    for prefix in ['/// fmt: off\n', '// fmt: off for this function\n', '// fmt:off\n']:
        actual = check(prefix + 'let f (): Int = { let n = 1; n }\n')
        assert 'let n = 1;\n    n\n}' in actual, actual
    actual = check('let a (): Int = 1 // fmt: off\nlet f (): Int = { 2 }\n')
    assert actual.endswith('let f (): Int = {\n    2\n}\n')
    literal = '"line\n// fmt: off\n// fmt: skip\nλ🙂"'
    actual = check('let f (): String = { let text = ' + literal + '; text }\n')
    assert literal in actual and ';\n    text\n}' in actual, actual

    # Regions can cross a delimiter boundary; later code still reaches its indent.
    cross = '// fmt: off\n  1\n}\nlet odd (): Int = { 2 }\n// fmt: on\n'
    actual = check('let first (): Int = {\n' + cross + '  let after (): Int = { 3 }\n', kept=[cross])
    assert actual.endswith('let after (): Int = {\n    3\n}\n'), actual

    for ending in ['\n', '\r\n']:
        text = ('let first (): Int = { 1 }\n// fmt: off\n'
                ' let raw (): String = "λ🙂\n\t\n// fmt: on in literal"\n'
                '// fmt: on\n    let last (): Int = { 2 }').replace('\n', ending)
        protected = text[text.index('// fmt: off'):text.rindex('// fmt: on') + len('// fmt: on') + len(ending)]
        check(text, kept=[protected])

    # Indentation after a preserved opening delimiter uses its real column.
    opening = '// fmt: off\n   let f (): Int = {\n// fmt: on\n'
    check(opening + '  1\n}\n', opening + '       1\n   }\n')

    # A skip without a matching definition/statement/expression is a no-op.
    check('struct P {\n// fmt: skip\nx: Int, y: Int, z: Int\n}\n',
          'struct P {\n    // fmt: skip\n    x: Int,\n    y: Int,\n    z: Int\n}\n')

    # Many separated exclusions must not copy a growing range/output prefix.
    allocated = []
    for count in [200, 400]:
        text = ''.join('// fmt: skip\nlet raw' + str(i) + ' (): Int = {   0 }\n'
                       + 'let normal' + str(i) + ' (): Int = { 1 }\n' for i in range(count))
        actual = check(text)
        assert actual.count('= {   0 }') == count
        assert actual.count('= {\n    1\n}') == count
        source.write_bytes(text.encode())
        result = subprocess.run([compiler, 'fmt', str(source)], capture_output=True,
                                env={**os.environ, 'PLUM_RT_STATS': '1'}, timeout=30)
        assert result.returncode == 0, result.stderr
        stats = re.search(rb'alloc n=\d+ bytes=(\d+)', result.stderr)
        assert stats, 'missing allocation counters'
        allocated.append(int(stats[1]))
    assert allocated[1] <= allocated[0] * 2.8, ('exclusion allocation growth', allocated)

    # Check/write use the same policy, including real changes outside exclusions.
    source.write_bytes((whole + 'let after (): Int = { 2 }\n').encode())
    result = subprocess.run([compiler, "fmt", "--check", str(source)], capture_output=True)
    assert result.returncode == 1
    run("fmt", "--write", str(source))
    assert whole in source.read_text()
    run("fmt", "--check", str(source))

print(f"formatter-exclusions: {checks} cases; preserved bytes, tokens, idempotence and CLI modes passed")
