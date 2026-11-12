#!/usr/bin/env python3
"""References use binding identity and the current project snapshot."""
import importlib.util
import sys
import tempfile
from pathlib import Path

from lsp_test_support import file_uri

spec = importlib.util.spec_from_file_location("lifecycle", Path(__file__).with_name("lsp-lifecycle-test.py"))
lifecycle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lifecycle)


def check(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-references-") as directory:
        root = Path(directory)
        main = root / "main.plum"
        other = root / "other.plum"
        text = ("let target (n: Int): Int = n + 1\n"
                "let caller (): Int = {\n"
                "    let local = target(1);\n"
                "    let nested = { let local = 9; local };\n"
                "    local + nested\n"
                "}\n")
        main.write_text(text)
        other.write_text("let second (): Int = target(2)\n")
        messages = [lifecycle.request("initialize", "init")]
        cases = {}

        def refs(ident, path, line, col, expected, include=False):
            messages.append(lifecycle.request("textDocument/references", ident, params={
                "textDocument": {"uri": file_uri(path)},
                "position": {"line": line, "character": col},
                "context": {"includeDeclaration": include}}))
            cases[ident] = expected

        def at(path, line, col, length):
            return (file_uri(path), line, col, line, col + length)

        target_uses = [at(main, 2, 16, 6), at(other, 0, 21, 6)]
        refs("function", main, 0, 5, target_uses)
        refs("from-use", other, 0, 23, target_uses + [at(main, 0, 4, 6)], True)
        refs("local", main, 2, 9, [at(main, 4, 4, 5)])
        refs("shadow", main, 3, 38, [at(main, 3, 34, 5), at(main, 3, 23, 5)], True)
        refs("parameter", main, 0, 12, [at(main, 0, 27, 1), at(main, 0, 12, 1)], True)
        refs("whitespace", main, 2, 0, [])
        # Both the querying file and another file can have unsaved edits.
        changed = "// unsaved\nlet second (): Int = target(3) + target(4)\n"
        messages.append(lifecycle.request("textDocument/didOpen", params={
            "textDocument": {"uri": file_uri(other), "text": changed}}))
        refs("unsaved", other, 1, 22,
             [at(main, 2, 16, 6), at(other, 1, 21, 6), at(other, 1, 33, 6)])
        messages += [lifecycle.request("shutdown", "stop"), lifecycle.request("exit")]
        replies = {r["id"]: r for r in lifecycle.run(compiler, messages) if "id" in r}
        assert replies["init"]["result"]["capabilities"].get("referencesProvider") is True
        for ident, expected in cases.items():
            reply = replies[ident]
            assert "result" in reply, reply
            actual = []
            for location in reply["result"]:
                start, end = location["range"]["start"], location["range"]["end"]
                actual.append((location["uri"], start["line"], start["character"],
                               end["line"], end["character"]))
            assert sorted(actual) == sorted(expected), (ident, actual, expected)
            assert len(actual) == len(set(actual)), (ident, "duplicate references", actual)
        assert other.read_text() == "let second (): Int = target(2)\n"
    check_modules(compiler)
    check_invalid(compiler)
    check_unicode_and_errors(compiler)
    check_externs(compiler)
    print("lsp references: ok")


def check_modules(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-reference-modules-") as directory:
        root = Path(directory)
        (root / "plum.pkg").write_text('Package { name: "references" }')
        sources = {
            "main.plum": ("use left as l;\nuse right;\n"
                          "let start (): Int = {\n"
                          "    let callback = same;\n"
                          "    let mut local = callback(1);\n"
                          "    local = local + l.same(2);\n"
                          "    local + right.same(3) + left.VALUE\n}\n"
                          "let same (n: Int): Int = n\n"),
            "left/left.plum": ("pub let same (n: Int): Int = n + 1\n"
                               "pub let VALUE: Int = same(8)\n"
                               "struct Box { value: Int }\n"
                               "let Box.read_value (b: Box): Int = b.value\n"
                               "let test (): Int = { let b = Box { value: 1 }; b.read_value() + Box.read_value(b) }\n"
                               "let indirect (): Int = { let f = same; f(2) }\n"),
            "right/right.plum": "pub let same (n: Int): Int = n + 2\n",
        }
        for name, text in sources.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)

        # Derive positions from exact source snippets, including a name
        # in call position and a bare function value referring to the same fn.
        def site(name, snippet, word):
            text = sources[name]
            off = text.index(snippet) + snippet.index(word)
            line = text.count("\n", 0, off)
            col = off - (text.rfind("\n", 0, off) + 1)
            return {"uri": file_uri(root / name), "range": {
                "start": {"line": line, "character": col},
                "end": {"line": line, "character": col + len(word)}}}

        left_uses = [site("main.plum", "l.same(2)", "same"),
                     site("left/left.plum", "same(8)", "same"),
                     site("left/left.plum", "f = same", "same")]
        method_uses = [site("left/left.plum", "b.read_value()", "read_value"),
                       site("left/left.plum", "Box.read_value(b)", "read_value")]
        local_uses = [site("main.plum", "local = local", "local"),
                      site("main.plum", "local + l.same", "local"),
                      site("main.plum", "local + right", "local")]
        cases = {
            "module": (left_uses[0], left_uses),
            "global": (site("left/left.plum", "VALUE:", "VALUE"),
                       [site("main.plum", "left.VALUE", "VALUE")]),
            "method": (method_uses[0], method_uses),
            "method-declaration": (site("left/left.plum", "Box.read_value (", "read_value"), method_uses),
            "root-function": (site("main.plum", "let same", "same"),
                              [site("main.plum", "callback = same", "same")]),
            "writes": (site("main.plum", "mut local", "local"), local_uses),
            "closure-call": (site("main.plum", "let callback", "callback"),
                             [site("main.plum", "callback(1)", "callback")]),
        }
        messages = [lifecycle.request("initialize", "init")]
        for ident, (query, _) in cases.items():
            messages.append(lifecycle.request("textDocument/references", ident, params={
                "textDocument": {"uri": query["uri"]}, "position": query["range"]["start"],
                "context": {"includeDeclaration": False}}))
        messages += [lifecycle.request("shutdown", "stop"), lifecycle.request("exit")]
        replies = {r["id"]: r for r in lifecycle.run(compiler, messages) if "id" in r}
        for ident, (_, expected) in cases.items():
            actual = replies[ident].get("result")
            assert actual is not None and len(actual) == len(expected), (ident, actual, expected)
            assert all(location in actual for location in expected), (ident, actual, expected)


def check_invalid(compiler):
    params = {"textDocument": {"uri": "file:///nonexistent-references.plum"},
              "position": {"line": 0, "character": 0}, "context": {"includeDeclaration": False}}
    messages = [lifecycle.request("initialize", "init")]
    for ident, patch in enumerate([{"position": {"line": -1, "character": 0}},
                                   {"position": {"line": 0.5, "character": 0}},
                                   {"context": None}, {"context": {}},
                                   {"context": {"includeDeclaration": "false"}}]):
        messages.append(lifecycle.request("textDocument/references", ident, params=params | patch))
    messages += [lifecycle.request("shutdown", "stop"), lifecycle.request("exit")]
    replies = {r["id"]: r for r in lifecycle.run(compiler, messages) if "id" in r}
    for ident in range(5):
        lifecycle.error(replies[ident], ident, -32602)


def check_unicode_and_errors(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-references-unicode-") as directory:
        path = Path(directory) / "main.plum"
        lines = ["let target (): Int = 1", '// target in a comment is not a use',
                 'let call (): Int = { let text = "é😀 target"; target() }']
        for newline in ["\n", "\r", "\r\n"]:
            text = newline.join(lines)
            path.write_bytes(text.encode())
            col = len(lines[2][:lines[2].rindex("target")].encode("utf-16-le")) // 2
            params = {"textDocument": {"uri": file_uri(path)},
                      "position": {"line": 2, "character": col + 2},
                      "context": {"includeDeclaration": False}}
            replies = lifecycle.run(compiler, [lifecycle.request("initialize", "init"),
                lifecycle.request("textDocument/references", "refs", params=params),
                lifecycle.request("textDocument/didOpen", params={"textDocument": {
                    "uri": file_uri(path), "text": "let broken = {"}}),
                lifecycle.request("textDocument/references", "broken", params=params),
                lifecycle.request("shutdown", "stop"), lifecycle.request("exit")])
            replies = {r["id"]: r for r in replies if "id" in r}
            assert replies["refs"]["result"] == [{"uri": file_uri(path), "range": {
                "start": {"line": 2, "character": col},
                "end": {"line": 2, "character": col + 6}}}], replies
            assert replies["broken"]["result"] == [], replies
            assert replies["stop"]["result"] is None


def check_externs(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-references-extern-") as directory:
        path = Path(directory) / "main.plum"
        # A preceding parameter shares the next function's name. Declaration
        # lookup must identify the fn token, not the first textual name.
        path.write_text('extern "C" {\n    fn first (foreign: Int) -> Int;\n'
                        '    fn foreign (n: Int) -> Int;\n}\n'
                        'let call (): Int = unsafe { foreign(1) }\n')
        params = {"textDocument": {"uri": file_uri(path)},
                  "position": {"line": 2, "character": 7},
                  "context": {"includeDeclaration": True}}
        replies = lifecycle.run(compiler, [lifecycle.request("initialize", "init"),
            lifecycle.request("textDocument/references", "refs", params=params),
            lifecycle.request("shutdown", "stop"), lifecycle.request("exit")])
        locations = next(r["result"] for r in replies if r.get("id") == "refs")
        assert len(locations) == 2, locations
        assert {(l["range"]["start"]["line"], l["range"]["start"]["character"])
                for l in locations} == {(2, 7), (4, 28)}, locations


if __name__ == "__main__":
    check(sys.argv[1])
