#!/usr/bin/env python3
"""Check deep layout, dense lines, and formatter allocation volume."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

compiler = Path(sys.argv[1]).resolve()


def format_source(root, source):
    path = root / "input.plum"
    path.write_bytes(source.encode("utf-8"))
    result = subprocess.run(
        [str(compiler), "fmt", str(path)], capture_output=True, timeout=30,
        env={**os.environ, "PLUM_RT_STATS": "1"})
    assert result.returncode == 0, (result.stdout + result.stderr).decode("utf-8")
    stats = re.search(rb"alloc n=(\d+) bytes=(\d+)", result.stderr)
    assert stats, "formatter did not report allocation counters"
    return result.stdout.decode("utf-8"), int(stats[2])


with tempfile.TemporaryDirectory(prefix="plum-format-layout-") as temp:
    root = Path(temp)
    # Expectations come directly from the block nesting, with depth far
    # beyond the former eight-pass budget. Test both shifted directions.
    depth = 64
    expected = ("let nested (): Int = {\n"
                + "".join(" " * (4 * (i + 1)) + "if true {\n" for i in range(depth))
                + " " * (4 * (depth + 1)) + "1\n"
                + "".join(" " * (4 * (i + 1)) + "} else { 0 }\n"
                          for i in reversed(range(depth))) + "}\n")
    bare = "\n".join(line.lstrip() for line in expected.splitlines()) + "\n"
    for source in [bare, "\n".join("\t\t\t" + line for line in bare.splitlines()) + "\n"]:
        actual, allocated = format_source(root, source)
        assert actual == expected, "deep blocks did not reach their assigned indentation"
        assert format_source(root, actual)[0] == actual, "deep layout is not idempotent"
        # A generous volume ceiling catches reintroducing repeated parses.
        # This counts cumulative allocated bytes, not peak/live memory.
        assert allocated < 16_000_000, f"deep formatting allocated {allocated} bytes"

    # Many nodes on one line exercise indexed source/token lookups. Keep
    # a broad timeout above; timing ratios would be noisy across CI hosts.
    for count in [1000, 4000]:
        source = "let dense (): Int = { " + "let x = 1; " * count + "x }\n"
        assert format_source(root, source)[0] == source
        source = " ".join(f'let f{i} (): Int = {{ let s = "// {{"; 1 }}'
                          for i in range(count)) + "\n"
        assert format_source(root, source)[0] == source

    # A comment marker INSIDE a token must not hide the following brace.
    source = 'let literal (): Int = {\nmatch "//" {\n_ => {\n1\n},\n}\n}\n'
    expected = ('let literal (): Int = {\n    match "//" {\n'
                '        _ => {\n            1\n        },\n    }\n}\n')
    assert format_source(root, source)[0] == expected

    # Lexer offsets count codepoints, not UTF-8 bytes. Also exercise CRLF,
    # a trailing comment before a block body, and EOF without a newline.
    source = 'let prefix (): String = "λ🙂"\r\n\tlet text (): String = { // {\r\n// owner\r\n"λ🙂"\r\n}'
    expected = 'let prefix (): String = "λ🙂"\r\nlet text (): String = { // {\r\n    // owner\r\n    "λ🙂"\r\n}'
    actual, _ = format_source(root, source)
    assert actual == expected
    assert format_source(root, actual)[0] == actual

print("formatter-layout: deep indentation, dense lines, source offsets and allocation ceiling passed")
