#!/usr/bin/env python3
"""Nominal type references through the real stdio LSP, including Asteroids."""
import importlib.util
import re
import subprocess
import sys
import tempfile
from pathlib import Path

from lsp_test_support import file_uri

spec = importlib.util.spec_from_file_location("lifecycle", Path(__file__).with_name("lsp-lifecycle-test.py"))
lifecycle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lifecycle)


def location(path, text, off, length):
    line = text.count("\n", 0, off)
    prefix = text[text.rfind("\n", 0, off) + 1:off]
    col = len(prefix.encode("utf-16-le")) // 2
    return {"uri": file_uri(path), "range": {
        "start": {"line": line, "character": col},
        "end": {"line": line, "character": col + length}}}


def reference_request(ident, query, include):
    return lifecycle.request("textDocument/references", ident, params={
        "textDocument": {"uri": query["uri"]}, "position": query["range"]["start"],
        "context": {"includeDeclaration": include}})


def check_asteroids(compiler):
    path = Path(__file__).resolve().parent.parent / "examples/asteroids/main.plum"
    text = path.read_text()
    # In this example every written Bullet token is either the declaration,
    # an annotation or a construction. This independent oracle catches missed
    # positions without depending on how the compiler builds its index.
    expected = [location(path, text, m.start(), 6) for m in re.finditer(r"\bBullet\b", text)]
    assert len(expected) >= 10
    messages = [lifecycle.request("initialize", "init"),
                reference_request("declaration", expected[0], True),
                reference_request("use", expected[-1], False),
                lifecycle.request("shutdown", "stop"), lifecycle.request("exit")]
    replies = {r["id"]: r for r in lifecycle.run(compiler, messages) if "id" in r}
    for ident, wanted in [("declaration", expected), ("use", expected[1:])]:
        actual = replies[ident].get("result")
        assert actual is not None and len(actual) == len(wanted), (ident, actual, wanted)
        assert all(item in actual for item in wanted), (ident, actual, wanted)


def check(compiler):
    check_asteroids(compiler)
    check_scopes(compiler)
    check_unsaved(compiler)
    check_dependency(compiler)
    check_primitive_names(compiler)
    print("lsp type references: ok")


def check_scopes(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-type-scopes-") as directory:
        root = Path(directory)
        (root / "plum.pkg").write_text('Package { name: "type_refs" }')
        sources = {
            "main.plum": (
                "use shapes as s;\nuse other;\n"
                "struct Bullet { x: Int }\n"
                "struct Box[T] { value: T }\n"
                "struct Holder { bullets: Array[Bullet] }\n"
                "enum Event { Hit(Bullet), Miss }\n"
                "let make (): Bullet = Bullet { x: 1 }\n"
                "let copy (b: Bullet): Bullet = { let local: Bullet = b; match local { Bullet { x } => Bullet { x: x } } }\n"
                "let generic[Bullet] (b: Bullet): Bullet = b\n"
                "let construct_generic[Bullet] (b: Bullet): Unit = { let nominal = Bullet { x: 1 }; () }\n"
                "let pair (b: (Bullet, Int)): Bullet = make()\n"
                "let callable (f: (Bullet) -> Bullet) (b: Bullet): Bullet = f(b)\n"
                "let boxed (b: Bullet): Box[Bullet] = Box { value: b }\n"
                "let shaped (b: shapes.Bullet): shapes.Bullet = { let local: s.Bullet = b; shapes.Bullet { x: local.x } }\n"
                "let another (b: other.Bullet): other.Bullet = b\n"
                "let state (e: Event): Event = match e { Event.Hit(b) => Event.Hit(b), Event.Miss => Event.Miss }\n"
                "let Bullet.launch (b: Bullet): Bullet = b\n"
                "let call (): Bullet = Bullet.launch(make())\n"
                'let mention (): String = "Bullet Event Box"\n'
                "// Bullet Event Box are not references here.\n"
                "let inferred (b: shapes.Bullet): Int = match b { Bullet { x } => x }\n"
                "let nested (b: (shapes.Bullet, Bullet)): Int = match b { (Bullet { x: left }, Bullet { x: right }) => left + right }\n"
                "let positional (b: shapes.Bullet): Int = match b { Bullet(x) => x }\n"
                "let destructure (b: shapes.Bullet): Int = { let Bullet { x } = b; x }\n"
                "let remote (e: other.Event): other.Event = match e { other.Event.Hit(n) => e, other.Event.Miss => e }\n"
            ),
            "shapes/types.plum": "pub struct Bullet { pub x: Int }\n",
            "other/types.plum": "pub struct Bullet { pub y: Int }\npub enum Event { Hit(Int), Miss }\n",
        }
        for name, text in sources.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)

        main = root / "main.plum"
        text = sources["main.plum"]
        checked = subprocess.run([compiler, "check", str(root)], capture_output=True, timeout=60)
        assert checked.returncode == 0, (checked.stdout.decode(), checked.stderr.decode())
        groups = {"Bullet": [], "Event": [], "Box": [], "shapes": [], "other": [], "other-event": []}
        # All nominal names in this fixture have explicit expected ownership.
        # Generic parameters, comments and strings deliberately share spellings.
        for line in text.splitlines(keepends=True):
            off = text.index(line)
            if line.startswith(("let generic", "let mention", "//")):
                continue
            for match in re.finditer(r"\b(Bullet|Event|Box)\b", line):
                word = match.group()
                if line.startswith("let construct_generic") and match.start() < line.index("nominal"):
                    continue
                group = word
                if word == "Event" and line.startswith("let remote"):
                    group = "other-event"
                if word == "Bullet":
                    if "other.Bullet" in line:
                        group = "other"
                    elif line.startswith(("let shaped", "let inferred", "let positional", "let destructure")):
                        group = "shapes"
                    elif line.startswith("let nested"):
                        # The pattern's first bare Bullet resolves from the
                        # first tuple element, rather than the visible root type.
                        occurrences = [m.start() for m in re.finditer(r"\bBullet\b", line)]
                        group = "shapes" if match.start() in (occurrences[0], occurrences[2]) else "Bullet"
                groups[group].append(location(main, text, off + match.start(), len(word)))
        for group, name in [("shapes", "shapes/types.plum"), ("other", "other/types.plum")]:
            source = sources[name]
            groups[group].insert(0, location(root / name, source, source.index("Bullet"), 6))
        source = sources["other/types.plum"]
        groups["other-event"].insert(0, location(root / "other/types.plum", source, source.index("Event"), 5))
        messages = [lifecycle.request("initialize", "init")]
        for group, expected in groups.items():
            messages.append(reference_request(group, expected[0], True))
            messages.append(reference_request(group + "-uses", expected[-1], False))
        replies = lifecycle.run(compiler, messages + [lifecycle.request("shutdown", "stop"), lifecycle.request("exit")])
        replies = {r["id"]: r for r in replies if "id" in r}
        for group, expected in groups.items():
            for ident, wanted in [(group, expected), (group + "-uses", expected[1:])]:
                actual = replies[ident].get("result")
                assert actual is not None and len(actual) == len(wanted), (ident, actual, wanted)
                assert all(item in actual for item in wanted), (ident, actual, wanted)


def check_unsaved(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-type-unsaved-") as directory:
        path = Path(directory) / "main.plum"
        path.write_text("struct Bullet { x: Int }\n")
        # Non-BMP text changes LSP columns; an unsaved declaration and uses
        # must all come from the buffer, without writing it back to disk.
        text = ('struct Bullet { x: Int }\n'
                'let make (): Bullet = { let s = "é😀"; Bullet { x: 1 } }\n')
        expected = [location(path, text, m.start(), 6) for m in re.finditer(r"\bBullet\b", text)]
        messages = [lifecycle.request("initialize", "init"),
                    lifecycle.request("textDocument/didOpen", params={
                        "textDocument": {"uri": file_uri(path), "text": text}}),
                    reference_request("buffer", expected[-1], True),
                    lifecycle.request("shutdown", "stop"), lifecycle.request("exit")]
        replies = {r["id"]: r for r in lifecycle.run(compiler, messages) if "id" in r}
        actual = replies["buffer"].get("result")
        assert actual is not None and len(actual) == len(expected), actual
        assert all(item in actual for item in expected), (actual, expected)
        assert path.read_text() == "struct Bullet { x: Int }\n"


def check_dependency(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-type-dependency-") as directory:
        root = Path(directory)
        app, dep = root / "app", root / "dep"
        app.mkdir()
        dep.mkdir()
        (app / "plum.pkg").write_text('Package { name: "app", deps: [Dep { name: "dep", path: "../dep" }] }')
        (dep / "plum.pkg").write_text('Package { name: "dep" }')
        (dep / "dep").mkdir()
        dependency = dep / "dep/types.plum"
        dependency.write_text("pub struct Bullet { pub x: Int }\n")
        main = app / "main.plum"
        text = "use dep;\nlet inspect (b: dep.Bullet): dep.Bullet = b\n"
        main.write_text(text)
        changed = "// unsaved dependency\npub struct Bullet { pub x: Int }\n"
        expected = [location(main, text, m.start(), 6) for m in re.finditer(r"\bBullet\b", text)]
        expected.append(location(dependency, changed, changed.index("Bullet"), 6))
        messages = [lifecycle.request("initialize", "init"),
                    lifecycle.request("textDocument/didOpen", params={
                        "textDocument": {"uri": file_uri(dependency), "text": changed}}),
                    reference_request("dependency", expected[0], True),
                    lifecycle.request("shutdown", "stop"), lifecycle.request("exit")]
        replies = {r["id"]: r for r in lifecycle.run(compiler, messages) if "id" in r}
        actual = replies["dependency"].get("result")
        assert actual is not None and len(actual) == len(expected), (actual, expected)
        assert all(item in actual for item in expected), (actual, expected)


def check_primitive_names(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-type-primitive-") as directory:
        path = Path(directory) / "main.plum"
        text = "struct Int { flag: Bool }\nlet primitive (n: Int): Int = n\n"
        path.write_text(text)
        occurrences = [location(path, text, m.start(), 3) for m in re.finditer(r"\bInt\b", text)]
        messages = [lifecycle.request("initialize", "init"),
                    reference_request("decl", occurrences[0], True),
                    reference_request("primitive", occurrences[1], False),
                    lifecycle.request("shutdown", "stop"), lifecycle.request("exit")]
        replies = {r["id"]: r for r in lifecycle.run(compiler, messages) if "id" in r}
        assert replies["decl"]["result"] == [occurrences[0]], replies
        assert replies["primitive"]["result"] == [], replies


if __name__ == "__main__":
    check(sys.argv[1])
