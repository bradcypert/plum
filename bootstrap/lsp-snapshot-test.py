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


def frame(message):
    body = json.dumps({"jsonrpc": "2.0", **message}).encode()
    return b"Content-Length: %d\r\n\r\n" % len(body) + body


class Client:
    def __init__(self, compiler, scratch):
        env = {**os.environ, "TMPDIR": str(scratch) + "/", "TMP": str(scratch), "TEMP": str(scratch)}
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


def uri(path):
    # Match this server's current file-URI contract (escaping is #160).
    return ("file:///" if path.drive else "file://") + path.as_posix()


def check(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-lsp-snapshot-") as directory:
        root = Path(directory)
        project, scratch = root / "project", root / "scratch"
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
            # POSIX dangling symlinks fail during copy, after some entries
            # may already have been copied. Windows may not allow symlinks.
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
            outside = root / "project-other"
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


if __name__ == "__main__":
    check(sys.argv[1])
