#!/usr/bin/env python3
"""Default layout must be readable, lossless, and stable on the next save."""
from pathlib import Path
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

    source.write_text('let main (): Unit = { println("hello"); println("world") }\n')
    result = subprocess.run([compiler, "fmt", "--check", str(source)], capture_output=True)
    assert result.returncode == 1
    run("fmt", "--write", str(source))
    run("fmt", "--check", str(source))

print("formatter-width: boundaries, compact expressions, comments, literals, CRLF, tokens and idempotence passed")
