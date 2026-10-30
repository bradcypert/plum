#!/usr/bin/env python3
"""Semantic requests use current buffers and release each project snapshot."""
import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

from lsp_test_support import file_uri as uri, native_path, make_symlink


def frame(message):
    body = json.dumps({"jsonrpc": "2.0", **message}).encode()
    return b"Content-Length: %d\r\n\r\n" % len(body) + body


class Client:
    def __init__(self, compiler, scratch):
        temp = native_path(scratch)
        env = {**os.environ, "TMPDIR": temp + "/", "TMP": temp, "TEMP": temp}
        self.proc = subprocess.Popen([compiler, "lsp"], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
        self.answers = queue.Queue()
        self.ident = 0
        self.notifications = []
        self.scratch = scratch
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        try:
            while True:
                header = self.proc.stdout.readline()
                if not header:
                    raise EOFError("LSP exited before replying")
                size = int(header.split(b":", 1)[1])
                assert self.proc.stdout.readline() == b"\r\n"
                self.answers.put(json.loads(self.proc.stdout.read(size)))
        except Exception as error:
            self.answers.put(error)

    def send(self, method, params=None, ident=None):
        self.proc.stdin.write(frame({"method": method, "params": params or {},
                                    **({"id": ident} if ident is not None else {})}))
        self.proc.stdin.flush()

    def request(self, method, params=None):
        self.ident += 1
        self.send(method, params, self.ident)
        while True:
            message = self.answers.get(timeout=30)
            if isinstance(message, Exception):
                raise message
            if "id" in message:
                assert message["id"] == self.ident, message
                assert not list(self.scratch.iterdir()), "snapshot survived a request"
                return message["result"]
            self.notifications.append(message)

    def notify(self, method, path, text=None):
        params = {"textDocument": {"uri": uri(path)}}
        if method == "didChange":
            params["contentChanges"] = [{"text": text}]
        elif text is not None:
            params["textDocument"]["text"] = text
        self.send("textDocument/" + method, params)

    def query(self, method, path, text, token, occurrence=0, after=False):
        off = -1
        for _ in range(occurrence + 1):
            off = text.index(token, off + 1)
        if after:
            off += len(token)
        prefix = text[:off]
        return self.request("textDocument/" + method, {
            "textDocument": {"uri": uri(path)},
            "position": {"line": prefix.count("\n"), "character": len(prefix.rsplit("\n", 1)[-1])}})


def check(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-lsp-snapshot-") as directory:
        root = Path(directory)
        project, scratch = root / "project-üni", root / "scratch"
        project.mkdir()
        scratch.mkdir()
        a, b = project / "main.plum", project / "other.plum"
        disk = 'let disk (): Int = 1\n'
        a.write_text(disk)
        b.write_text('let helper (): Int = 2\n')
        client = Client(compiler, scratch)
        try:
            client.request("initialize")
            live = 'let consume (): String = {\n    let current = helper();\n    current\n}\n'
            sibling = '\n\nlet helper (): String = "live"\n'
            client.notify("didOpen", a, live)
            client.notify("didOpen", b, sibling)
            hover = client.query("hover", a, live, "current", 1)
            assert hover is not None and hover["contents"]["value"] == "current: String", hover
            assert hover["range"] == {"start": {"line": 2, "character": 4},
                                      "end": {"line": 2, "character": 11}}, hover
            target = client.query("definition", a, live, "current", 1)
            assert target["uri"] == uri(a) and target["range"]["start"] == {"line": 1, "character": 8}, target
            target = client.query("definition", a, live, "helper")
            assert target["uri"] == uri(b) and target["range"]["start"]["line"] == 2, target
            # A declaration hover uses defs rather than the checker table.
            declaration = client.query("hover", b, sibling, "helper")
            assert "String" in declaration["contents"]["value"], declaration
            assert client.notifications[-1]["params"]["diagnostics"] == [], client.notifications

            # Rename and move a local, interleaving another buffer change.
            changed = 'let consume (): String = {\n\n    let renamed = helper();\n    renamed\n}\n'
            client.notify("didChange", a, changed)
            client.notify("didChange", b, sibling + "\n")
            target = client.query("definition", a, changed, "renamed", 1)
            assert target["range"]["start"] == {"line": 2, "character": 8}, target
            assert "String" in client.query("hover", a, changed, "renamed", 1)["contents"]["value"]
            # Snapshot overlays include new unsaved files in nested modules.
            nested = project / "nested"
            nested.mkdir()
            new = nested / "new.plum"
            new_text = 'pub let fresh (): Int = 3\n'
            client.notify("didOpen", new, new_text)
            use_new = 'let consume (): Int = nested.new.fresh()\n'
            client.notify("didChange", a, use_new)
            target = client.query("definition", a, use_new, "fresh")
            assert target["uri"] == uri(new), target

            # Both member types and locals come from the current buffer.
            old_members = 'struct Old { old: Int }\nlet consume (): Int = { let value = Old { old: 1 }; value.old }\n'
            a.write_text(old_members)
            members = 'struct New { fresh: Int }\nlet consume (): Int = { let receiver = New { fresh: 1 }; receiver.fresh }\n'
            client.notify("didChange", a, members)
            completions = client.query("completion", a, members, "receiver.", after=True)
            labels = {item["label"] for item in completions["items"]}
            assert "fresh" in labels and "old" not in labels, completions
            locals_ = client.query("completion", a, members, "receiver", 1)
            assert any(item["label"] == "receiver" and item["kind"] == 6 for item in locals_["items"]), locals_
            # An incomplete edit still offers general names/keywords.
            incomplete = members + 'let unfinished (): Int = '
            client.notify("didChange", a, incomplete)
            general = client.query("completion", a, incomplete, "unfinished")
            assert any(item["label"] == "let" for item in general["items"]), general
            assert client.query("hover", a, incomplete, "receiver", 1) is None

            # An unterminated token must not be lexed inside the server after
            # the isolated formatter refuses it.
            client.notify("didChange", a, 'let broken = "unterminated')
            assert client.request("textDocument/formatting", {"textDocument": {"uri": uri(a)}}) == []
            # Empty buffers never silently fall back to the disk program.
            client.notify("didChange", a, "")
            assert client.query("hover", a, "disk", "disk") is None
            client.notify("didClose", a)
            assert "Old" in client.query("hover", a, old_members, "Old")["contents"]["value"]

            # Repeat successful and failing requests in one live session.
            for _ in range(20):
                client.notify("didOpen", a, changed)
                assert client.query("hover", a, changed, "renamed", 1) is not None
                client.notify("didChange", a, 'let broken = +')
                assert client.query("hover", a, 'let broken = +', "broken") is None
                client.notify("didClose", a)
            # A missing source directory forces copy failure. Never analyze
            # a partial snapshot containing just this buffer (#166).
            client.request("initialize")  # Drain the final didClose notification.
            missing = root / "missing" / "main.plum"
            before = len(client.notifications)
            client.notify("didOpen", missing, 'let broken (): Int = "bad"\n')
            assert client.query("hover", missing, "broken", "broken") is None
            assert len(client.notifications) == before
            # Force an overlay write failure using an existing directory.
            unwritable = project / "directory.plum"
            unwritable.mkdir()
            before = len(client.notifications)
            client.notify("didOpen", unwritable, 'let broken (): Int = "bad"\n')
            assert client.query("definition", a, old_members, "Old") is None
            assert client.request("textDocument/completion", {"textDocument": {"uri": uri(a)}}) == []
            assert len(client.notifications) == before
            client.notify("didClose", unwritable)
            # Copy preserves links, but source discovery must refuse this
            # absolute dangling link. Windows may not allow symlinks.
            link = project / "dangling"
            try:
                link.symlink_to(project / "absent")
            except (OSError, NotImplementedError):
                pass
            else:
                assert client.query("hover", a, old_members, "Old") is None
                link.unlink()
            assert client.query("hover", a, old_members, "Old") is not None
            # A URI sharing the directory's textual prefix is outside it.
            outside = root / "project-üni-other"
            outside.mkdir()
            unrelated = outside / "main.plum"
            unrelated.write_text('let outside (): Int = 1\n')
            client.notify("didOpen", unrelated, 'let broken = +')
            assert client.query("hover", a, old_members, "Old") is not None
            assert a.read_text() == old_members
            assert b.read_text() == 'let helper (): Int = 2\n'
            assert not new.exists()
            assert client.request("shutdown") is None
            client.send("exit")
            client.proc.wait(timeout=10)
            assert client.proc.returncode == 0, client.proc.stderr.read().decode()
            assert not list(scratch.iterdir())
        finally:
            if client.proc.poll() is None:
                client.proc.kill()
                client.proc.wait()
    print("lsp snapshot lifecycle: ok")


def check_roots(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-lsp-roots-") as directory:
        base = Path(directory)
        project, dependency, scratch = (base / name for name in ("project", "dependency", "scratch"))
        for folder in (project, dependency, scratch):
            folder.mkdir()
        nested = project / "nested"
        nested.mkdir()
        manifest = project / "plum.pkg"
        manifest.write_text('Package { name: "app", deps: [Dep { name: "dep", path: "../dependency" }] }')
        (dependency / "plum.pkg").write_text('Package { name: "dep" }')
        (dependency / "dep").mkdir()
        dep = dependency / "dep" / "main.plum"
        dep.write_text('pub let answer (): Int = 1\n')
        source = nested / "main-üni.plum"
        text = 'use dep;\nlet consume (): String = { let value = dep.answer(); value }\n'
        source.write_text(text)
        client = Client(compiler, scratch)
        try:
            client.request("initialize")
            client.notify("didOpen", source, text)
            client.notify("didOpen", dep, 'pub let answer (): String = "live"\n')
            hover = client.query("hover", source, text, "value", 1)
            assert hover and hover["contents"]["value"] == "value: String", hover
            target = client.query("definition", source, text, "answer")
            assert target and target["uri"] == uri(dep), target
            client.notify("didClose", dep)
            client.notify("didClose", source)
            # Transitive external roots are rebound inside the snapshot too.
            transitive = base / "transitive"
            (transitive / "leaf").mkdir(parents=True)
            (transitive / "plum.pkg").write_text('Package { name: "leaf" }')
            leaf = transitive / "leaf" / "main.plum"
            leaf.write_text('pub let answer (): Int = 1\n')
            (dependency / "plum.pkg").write_text('Package { name: "dep", deps: [Dep { name: "leaf", path: "../transitive" }] }')
            dep.write_text('use leaf;\npub let answer (): String = leaf.answer()\n')
            client.notify("didOpen", leaf, 'pub let answer (): String = "live"\n')
            hover = client.query("hover", source, text, "value", 1)
            assert hover and "String" in hover["contents"]["value"], hover
            client.notify("didClose", leaf)
            dep.write_text('pub let answer (): Int = 1\n')
            # Replacing several copied inodes must not retain obsolete
            # identities that could be recycled for another buffer's target.
            overlays = []
            for index in range(12):
                extra = project / ("buffer%d.plum" % index)
                disk = 'let buffer%d (): Int = 0\n' % index
                live = 'let buffer%d (): Int = %d\n' % (index, index + 1)
                extra.write_text(disk)
                overlays.append((extra, live, disk))
                client.notify("didOpen", extra, live)
            extra, live, disk = overlays[-1]
            assert client.query("hover", extra, live, "buffer11") is not None
            for extra, live, disk in overlays:
                assert extra.read_text() == disk
                client.notify("didClose", extra)
                extra.unlink()
            # Invalid manifests are isolated in the worker, not the server.
            manifest.write_text('Package {')
            assert client.query("hover", source, text, "value", 1) is None
            manifest.write_text('Package { name: "app" }')
            source.unlink()
            hidden = project / ".hidden"
            hidden.mkdir()
            physical = hidden / "target.plum"
            physical.write_text('let disk (): Int = 1\n')
            alias = project / "visible.plum"
            try:
                make_symlink(alias, ".hidden/target.plum")
            except (OSError, NotImplementedError):
                print("lsp alias cases: symlinks unavailable")
            else:
                live = 'let consume (): String = { let value = "live"; value }\n'
                client.notify("didOpen", physical, live)
                hover = client.query("hover", physical, live, "value", 1)
                if hover is None:
                    # Keep a failed Windows snapshot reviewable: the worker's
                    # refusal is deliberately hidden from normal LSP replies.
                    with tempfile.TemporaryDirectory(dir=scratch) as debug:
                        payload = {"path": uri(physical), "container": uri(Path(debug)),
                                   "buffers": [{"uri": uri(physical), "text": live}]}
                        body = json.dumps(payload).encode()
                        result = subprocess.run([compiler, "__lsp_snapshot"],
                            input=b"Content-Length: %d\r\n\r\n" % len(body) + body,
                            capture_output=True, timeout=30)
                        raise AssertionError(("snapshot worker", result.returncode, result.stdout, result.stderr))
                assert "String" in hover["contents"]["value"], hover
                target = client.query("definition", physical, live, "value", 1)
                assert target and target["uri"] == uri(physical), target
                if len(native_path(physical)) > 1 and native_path(physical)[1] == ":":
                    case_uri = "file:///" + native_path(physical).upper()
                    client.send("textDocument/didOpen", {"textDocument": {"uri": case_uri, "text": live}})
                    assert client.query("hover", physical, live, "value", 1) is not None
                    client.send("textDocument/didChange", {"textDocument": {"uri": case_uri},
                        "contentChanges": [{"text": live.replace('"live"', '"conflict"')}]})
                    assert client.query("hover", physical, live, "value", 1) is None
                    client.send("textDocument/didClose", {"textDocument": {"uri": case_uri}})
                client.notify("didOpen", alias, live)
                assert client.query("hover", alias, live, "value", 1) is not None
                client.notify("didChange", alias, live.replace('"live"', '"different"'))
                assert client.query("hover", physical, live, "value", 1) is None
                client.notify("didClose", alias)
                # The policy applies to both buffer overlays and discovery.
                manifest.write_text('Package { name: "app", symlinks: "reject" }')
                assert client.query("hover", physical, live, "value", 1) is None
                manifest.write_text('Package { name: "app" }')
                physical.unlink()
                assert client.query("hover", physical, live, "value", 1) is not None
                assert not physical.exists(), "overlay modified original dangling target"
                client.notify("didClose", physical)
                alias.unlink()
            assert dep.read_text() == 'pub let answer (): Int = 1\n'
            assert client.request("shutdown") is None
            client.send("exit")
            client.proc.wait(timeout=10)
            assert client.proc.returncode == 0, client.proc.stderr.read().decode()
        finally:
            if client.proc.poll() is None:
                client.proc.kill()
                client.proc.wait()
    print("lsp declared roots and aliases: ok")


if __name__ == "__main__":
    check(sys.argv[1])
    check_roots(sys.argv[1])
