#!/usr/bin/env python3
"""Exercise edits as a client would: apply the returned UTF-16 range."""
import json
import subprocess
import sys
import tempfile
from pathlib import Path

from lsp_test_support import file_uri


def frame(message):
    body = json.dumps(message, ensure_ascii=False).encode()
    return b"Content-Length: %d\r\n\r\n" % len(body) + body


def decode(data):
    messages = []
    while data:
        header, data = data.split(b"\r\n\r\n", 1)
        size = int(header.split(b":", 1)[1])
        messages.append(json.loads(data[:size]))
        data = data[size:]
    return messages


def offset(text, position):
    # splitlines(keepends=True) handles all three protocol line endings.
    lines = text.splitlines(keepends=True)
    line, col = position["line"], position["character"]
    prefix = "".join(lines[:line])
    rest = lines[line] if line < len(lines) else ""
    return len(prefix) + len(rest.encode("utf-16-le")[:col * 2].decode("utf-16-le"))


def apply(text, edits):
    for edit in reversed(edits):
        span = edit["range"]
        text = text[:offset(text, span["start"])] + edit["newText"] + text[offset(text, span["end"]):]
    return text


def check(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-lsp-format-") as directory:
        root = Path(directory)
        a, b = root / "a.plum", root / "b.plum"
        disk = 'let disk (): Int = 1\n'
        a.write_text(disk)
        b.write_text('let other (): Int = 2\n')
        uri_a, uri_b = file_uri(a), file_uri(b)
        messages = [{"id": 1, "method": "initialize", "params": {}}]
        expected = {}

        def notify(method, uri, **extra):
            messages.append({"method": "textDocument/" + method,
                             "params": {"textDocument": {"uri": uri, **extra}}})

        def change(uri, text):
            messages.append({"method": "textDocument/didChange", "params": {
                "textDocument": {"uri": uri}, "contentChanges": [{"text": text}]}})

        def formatting(text, end=None, formatted=None):
            ident = 10 + len(expected)
            messages.append({"id": ident, "method": "textDocument/formatting",
                             "params": {"textDocument": {"uri": uri_a}, "options": {}}})
            expected[ident] = (text, end, formatted)

        # A must survive an interleaved open/change to B. Open text itself
        # is authoritative; there need not have been a didChange for A.
        original = 'let live (): Int = {\n  7\n}'
        notify("didOpen", uri_a, text=original)
        notify("didOpen", uri_b, text='let other (): Int = 3\n')
        change(uri_b, 'let other (): Int = 4\n')
        formatting(original, {"line": 2, "character": 1}, 'let live (): Int = {\n    7\n}')
        # Replacing A and saving without text must preserve the new version.
        original = 'let changed (): Int = {\n  8\n}'
        change(uri_a, original)
        change(uri_b, 'let other (): Int = 5\n')
        notify("didSave", uri_a)
        formatting(original, {"line": 2, "character": 1}, 'let changed (): Int = {\n    8\n}')
        # didSave's optional text is at params.text, unlike didOpen.
        original = 'let saved (): Int = {\n  9\n}'
        messages.append({"method": "textDocument/didSave", "params": {
            "textDocument": {"uri": uri_a}, "text": original}})
        formatting(original, {"line": 2, "character": 1}, 'let saved (): Int = {\n    9\n}')
        # The exact end covers empty final lines, Unicode, and every newline.
        for original, end, formatted in [
            ('let live (): Array[Int] = [1,2]', {"line": 0, "character": 31}, 'let live (): Array[Int] = [1, 2]'),
            ('let live (): Array[Int] = [1,2]\n', {"line": 1, "character": 0}, 'let live (): Array[Int] = [1, 2]\n'),
            ('let live (): Int = {\r\n\t\r\n\t7\r\n}', {"line": 3, "character": 1}, 'let live (): Int = {\r\n\r\n    7\r\n}'),
            ('let live (): Int = {\r\n  7\r\n}', {"line": 2, "character": 1}, 'let live (): Int = {\r\n    7\r\n}'),
            ('let live (): Array[Int] = [1,2]\r// end', {"line": 1, "character": 6}, 'let live (): Array[Int] = [1, 2]\r// end'),
            ('let word (): Array[String] = ["é😀","x"]', {"line": 0, "character": 40}, 'let word (): Array[String] = ["é😀", "x"]'),
        ]:
            change(uri_a, original)
            formatting(original, end, formatted)
        # Indentation tabs are replaced while tabs and blank lines inside
        # a multiline literal survive the edit returned to the editor.
        original = 'let text (): String = {\n\t"a\n\t\n\n\nb"\n\t}'
        change(uri_a, original)
        formatting(original, {"line": 6, "character": 2},
                   'let text (): String = {\n    "a\n\t\n\n\nb"\n}')
        # Close releases unsaved text: formatting must now use disk.
        notify("didClose", uri_a)
        formatting(disk)
        # Reopen after close, and preserve an explicitly empty buffer.
        notify("didOpen", uri_a, text="")
        formatting("")
        # Opening unsaved invalid text must diagnose that text, not disk.
        notify("didOpen", uri_a, text='let broken (): Int = "wrong"\n')
        notify("didClose", uri_a)
        notify("didClose", uri_b)
        messages += [{"id": 2, "method": "shutdown"}, {"method": "exit"}]
        result = subprocess.run([compiler, "lsp"], input=b"".join(
            frame({"jsonrpc": "2.0", **message}) for message in messages),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=90)
        assert result.returncode == 0, result.stderr.decode()
        replies = decode(result.stdout)
        by_id = {m["id"]: m for m in replies if "id" in m}
        for ident, (original, end, formatted) in expected.items():
            edits = by_id[ident]["result"]
            if end is None:
                assert edits == [], (ident, edits)
            else:
                assert len(edits) == 1, (ident, edits)
                assert edits[0]["range"]["end"] == end, (ident, edits)
                assert apply(original, edits) == edits[0]["newText"], (ident, edits)
                assert edits[0]["newText"] == formatted, (ident, edits)
        diagnostics = [m["params"] for m in replies if m.get("method") == "textDocument/publishDiagnostics"]
        assert any("function broken" in json.dumps(d["diagnostics"]) for d in diagnostics)
        assert diagnostics[-2:] == [{"uri": uri_a, "diagnostics": []}, {"uri": uri_b, "diagnostics": []}]
        assert a.read_text() == disk
        assert b.read_text() == 'let other (): Int = 2\n'
        assert by_id[2]["result"] is None
    print("lsp formatting lifecycle: ok")


if __name__ == "__main__":
    check(sys.argv[1])
