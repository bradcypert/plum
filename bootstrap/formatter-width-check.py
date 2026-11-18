#!/usr/bin/env python3
"""Default layout must be readable, lossless, and stable on the next save."""
from pathlib import Path
import os
import runpy
import subprocess
import sys
import tempfile

compiler = str(Path(sys.argv[1]).resolve())


def run(*args):
    result = subprocess.run([compiler, *args], capture_output=True, timeout=30)
    assert result.returncode == 0, result.stdout + result.stderr
    return result.stdout


with tempfile.TemporaryDirectory(prefix="plum-width-") as temp:
    root = Path(temp)
    source = root / "input.plum"
    output = root / "output.plum"

    def check(before, expected=None):
        source.write_bytes(before.encode())
        actual = run("fmt", str(source))
        if expected is not None:
            assert actual.decode() == expected, (actual.decode(), expected)
        output.write_bytes(actual)
        assert run("fmt", str(output)) == actual, "default layout is not idempotent"
        assert run("relex", str(source), "--tokens") == run("relex", str(output), "--tokens"), "tokens changed"
        return actual.decode()

    check("let total (): Int = { let a = 1; let b = 2; a + b }\n"
          "let identity (x: Int): Int = x\n",
          "let total (): Int = {\n    let a = 1;\n    let b = 2;\n    a + b\n}\n\n"
          "let identity (x: Int): Int = x\n")

    check("use Os;\nuse Path;\nstruct Progress { stage: Int, done: Int, total: Int }\n"
          "enum Stage { Idle, Checking, Ready, Failed }\n",
          "use Os;\nuse Path;\n\nstruct Progress {\n    stage: Int,\n    done: Int,\n    total: Int\n}\n\n"
          "enum Stage {\n    Idle,\n    Checking,\n    Ready,\n    Failed\n}\n")

    check("let pair (): P = P { x: 1, y: 2 }\n"
          "let guard (ok: Bool): Int = if ok { 1 } else { 0 }\n",
          "let pair (): P = P { x: 1, y: 2 }\n\n"
          "let guard (ok: Bool): Int = if ok { 1 } else { 0 }\n")

    check("let choose (n: Int): Int = match n { 0 => 1, _ => 2 }\n",
          "let choose (n: Int): Int = match n {\n    0 => 1,\n    _ => 2\n}\n")

    check("let next (s: S): S = S { a: 1, b: P { x: 2, y: 3, z: 4 }, ..s }\n",
          "let next (s: S): S = S {\n    a: 1,\n    b: P {\n        x: 2,\n        y: 3,\n        z: 4\n    },\n    ..s\n}\n")

    check("let callbacks (): P = P { f: |a,b| a + b, g: 1, h: 2 }\n",
          "let callbacks (): P = P {\n    f: |a, b| a + b,\n    g: 1,\n    h: 2\n}\n")

    comments = ("let first (): Int = { let a = 1; // keep trailing\n"
                "// documents b\nlet b = 2; a + b } // end of first\n"
                "/// documents second\nlet second (): Int = 3\n")
    expected = ("let first (): Int = {\n    let a = 1; // keep trailing\n"
                "    // documents b\n    let b = 2;\n    a + b\n} // end of first\n\n"
                "/// documents second\nlet second (): Int = 3\n")
    check(comments, expected)
    check(comments.replace("\n", "\r\n"), expected.replace("\n", "\r\n"))

    check('let text (): String = { let s = "λ🙂 { // , ;\n\t\n\n\nb"; s }',
          'let text (): String = {\n    let s = "λ🙂 { // , ;\n\t\n\n\nb";\n    s\n}')

    check('struct Commented {\n// first field\na: Int, b: Int, c: Int\n// still inside\n}\n',
          'struct Commented {\n    // first field\n    a: Int,\n    b: Int,\n    c: Int\n    // still inside\n}\n')

    # Contracts synthesize statements with no source offsets; they must never
    # produce a break in another part of the file.
    check('let half (n: Int): Int\n    require n > 0 : "positive"\n= n / 2\n')
    check('let mapped (xs: Array[Int]): Array[Int] = if true { xs } else { Array.map(xs, { let k = xs.len(); |v| v + k }) }\n')
    check('let choose (ok: Bool): String = match ok { true => if ok { "' + 'x' * 50 + '" } else { "" }, false => "" }\n')
    check('let filtered (): Int = functions().filter(|f| { assert_eq(total(), 4); f() < 0 }).len()\n')
    check('let callback (): Int = consume({ let marker = make(8); |r: Resource| { assert_eq(total(), 6); r.raw() } })\n')
    check('let condition (a: Int) (b: Int): Bool = a == 0 && ((a == 1 && b == 3)\n                || (a == 2 && b == 2 && oracle_at(b, 1) == 61))\n')
    check('let callback_body (): Int = consume(\n    value,\n    |v|\n    if v > 0 { produce(v) } else { 0 }\n)\n')
    check('let literal_chain (): Unit = println(\n    "now_millis agrees with now(): "\n    .concat((now() / 1000 - then() <= 1).to_string()))\n')
    check('let unicode_chain (): Unit = println(String.char_len("日本語").to_string().concat(" ").concat("日本語".len().to_string()))\n')
    check('let choose (ok: Bool): String = produce(\n    if ok { "yes" }\nelse { "no" }\n)\n')
    check('let trailing_operator (a: Int): Bool =\n    (a == 1 &&\n    (a == 2 || a == 3))\n')
    nested = check('let nested (a: Bool): Int =\n    if\n        if a { true } else { false }\n    { 1 }\n    else { 0 }\n')
    assert '\n    else { 0 }' in nested, nested
    check('let outer (): R = R { a: R { x: 1, y: 2, z: 3 } }\n')
    check('let wait (rx: Receiver[Int]): Int = select { n = rx => n.unwrap_or(0), else => 0 }\n')
    check('struct Nested { first: Map[String, Int], second: (Int, String), third: Array[Int] }\n')
    check('let long_record (): R = R { first: "' + 'x' * 80 + '", second: 2 }\n')
    check('let existing (): R = R { first: 1,\nsecond: 2 }\n')
    check('')
    check('// comment only, no trailing newline')

    check('let assemble (source_values: Array[String]) (separator: String) (prefix: String) (suffix: String): String = prefix\n',
          'let assemble\n    (source_values: Array[String])\n    (separator: String)\n    (prefix: String)\n    (suffix: String): String\n= prefix\n')

    values = ['"' + letter * 30 + '"' for letter in 'abc']
    check('let render (): String = render_values(' + ', '.join(values) + ')\n',
          'let render (): String =\n    render_values(\n'
          + ',\n'.join('        ' + value for value in values) + '\n    )\n')
    for source_text in [
        'let names (): Array[String] = [' + ', '.join(values) + ']\n',
        'let nested (): String = outer(inner(' + ', '.join(values) + '), "end")\n',
        'let method (): String = "start"' + ''.join('.concat(' + v + ')' for v in values) + '\n',
        'let pipes (names: Array[String]): Array[String] = names'
        + ' |> Array.map(_, |s| s.concat("suffix"))' * 4 + '\n',
        'let callbacks (): P = P { apply: |left: Map[String, Int], right: Int| left, b: 1, c: 2 }\n',
        'let bit_or (): Int = use_values(1 | 2, 3, 4)\n',
        'let empty_closure (): Int = invoke(|| 42)\n',
    ]:
        result = check(source_text)
        assert all(len(line) <= 100 for line in result.splitlines()), result
    # Comment markers, comma text and escapes inside long literal tokens are
    # indivisible, even when they alone exceed the soft line width.
    check('let literal (): String = "' + 'λ🙂, // ; ' * 20 + '"\n')

    # Incidental breaks inside short groups have the same canonical answer.
    for compact, broken in [
        ('consume(first, second)', 'consume(\nfirst,\nsecond\n)'),
        ('outer(inner(1, 2))', 'outer(\ninner(\n1,\n2\n)\n)'),
        ('consume(|v| produce(v))', 'consume(\n|v|\nproduce(v)\n)'),
        ('consume(value.first().second())', 'consume(\nvalue\n.first()\n.second()\n)'),
        ('consume([1, 2])', 'consume(\n[\n1,\n2\n]\n)'),
        ('consume("λ🙂", "日本語")', 'consume(\n"λ🙂",\n"日本語"\n)'),
    ]:
        canonical = 'let short (): Int = ' + compact + '\n'
        check('let short (): Int = ' + broken + '\n', canonical)
        check(canonical, canonical)

    # Long source indentation inside a short nested type must not make the
    # signature appear wider than its canonical form.
    signature = 'let types (xs: Array[Map[String, Int]]): Int = xs.len()\n'
    broken_type = 'Array[\n' + ' ' * 100 + 'Map[\nString,\nInt\n]\n]'
    check('let types (xs: ' + broken_type + '): Int = xs.len()\n', signature)

    fixture = Path(__file__).with_name('fmt_corpus') / 'grouping'
    grouped = (fixture / 'expected.plum').read_text()
    check((fixture / 'in.plum').read_text(), grouped)
    check((fixture / 'in.plum').read_text().replace('\n', '\r\n'), grouped.replace('\n', '\r\n'))

    # Fitting an inner group includes the outer closing delimiters and `;`.
    for count in range(30, 83):
        result = check('let boundary (): Unit = { let x = outer(inner("' + 'x' * count + '")); x }\n')
        assert all(len(line) <= 100 for line in result.splitlines()), result

    # Moving a chain's closing delimiters must not require a second save
    # to move the method dot. Measure the same canonical link both times.
    for count in range(20, 84):
        result = check('let chain (): Unit = { consume("' + 'x' * count
                       + '"\n.concat(name()).concat("' + 's' * 40 + '")) }\n')
        assert all(len(line) <= 100 for line in result.splitlines()), result

    # A conditional's final branch is not a wrapper payload. Keep the
    # control expression in ordinary argument layout rather than joining
    # everything through its `else {` prefix.
    conditional = check('let conditional (): Int = choose(0, if flag { 1 } else { produce("'
                        + 'x' * 60 + '") })\n')
    assert 'choose(\n' in conditional, conditional

    # An index is not an array-literal payload: move the arguments instead
    # of splitting a short `values.offsets[index]` at the right margin.
    indexed = check('let index (): Unit = { consume("' + 'x' * 65
                    + '", values.offsets[index]); }\n')
    assert 'values.offsets[index]' in indexed, indexed

    # A short final call belongs on a fresh argument line when the whole
    # outer call is too wide; do not split its tiny payload instead.
    short_call = check('let nested (): Unit = { consume("' + 'x' * 65
                       + '", values.concat(token)); }\n')
    assert 'values.concat(token)' in short_call, short_call

    # An earlier callback is not the final call's body and stays inline.
    earlier = check('let earlier (): Int = consume(|v| v, inner('
                    + ', '.join('"' + v * 35 + '"' for v in 'abc') + '))\n')
    assert 'consume(|v| v, inner(' in earlier, earlier

    # Comments block contraction at both wrapper joins, including a final
    # comment inside the payload. Multiline tokens remain indivisible.
    for text in [
        'let comment (): R = Ok(\n// before record\nP { x: 1, y: 2, z: 3 })\n',
        'let comment (): R = Ok(P { x: 1, y: 2, z: 3 } // after record\n)\n',
        'let comment (): R = Ok(P { x: 1, y: 2, z: 3\n// inside record\n})\n',
        'let text (): R = Ok(P { value: "λ🙂\n\t// literal", second: 2, third: 3 })\n',
    ]:
        check(text)

    source.write_text('let main (): Unit = { println("hello"); println("world") }\n')
    result = subprocess.run([compiler, "fmt", "--check", str(source)], capture_output=True)
    assert result.returncode == 1
    run("fmt", "--write", str(source))
    run("fmt", "--check", str(source))

# Generated Plum must also be a formatter fixed point. Keep the temporary
# program beside the real manifest so @embed_file keeps its relative paths.
generator = runpy.run_path(str(Path(__file__).with_name("gen-shims")))
manifest = generator["OUT"]
fd, generated_path = tempfile.mkstemp(prefix=".fmt-generator-", suffix=".plum", dir=manifest.parent)
os.close(fd)
generated_path = Path(generated_path)
try:
    generated = generator["generate"]()
    generated_path.write_bytes(generated.encode("utf-8"))
    assert run("fmt", str(generated_path)).decode() == generated, "shim generator output needs formatting"

    # Exercise either side of the actual call-width boundary, rather than
    # relying on today's vendored filenames to happen to cross it.
    with tempfile.TemporaryDirectory(prefix=".fmt-payload-", dir=manifest.parent) as payloads:
        for width in [99, 100, 101, 130]:
            prefix = '        source: @embed_file("'
            name_size = width - len(prefix) - len('")') - len(Path(payloads).name) - 1
            payload = Path(payloads) / ("x" * name_size)
            payload.write_text("sample")
            generated = generator["HEADER"] + generator["render"]("sample", False, payload) + "]\n"
            generated_path.write_bytes(generated.encode("utf-8"))
            assert run("fmt", str(generated_path)).decode() == generated, f"generated embed call width {width} needs formatting"
finally:
    generated_path.unlink(missing_ok=True)

print("formatter-width: boundaries, compact expressions, comments, literals, CRLF, tokens and idempotence passed")
