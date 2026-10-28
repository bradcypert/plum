#!/usr/bin/env python3
"""Source offsets survive LF, CR, CRLF, and mixed document line endings."""
import importlib.util
import re
import subprocess
import sys
import tempfile
from pathlib import Path

# The lifecycle harness owns the framed client and snapshot cleanup checks.

spec = importlib.util.spec_from_file_location("snapshot", Path(__file__).with_name("lsp-snapshot-test.py"))
snapshot = importlib.util.module_from_spec(spec)
spec.loader.exec_module(snapshot)
Client = snapshot.Client
uri = snapshot.uri


def check(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-source-positions-") as directory:
        root = Path(directory)
        project, scratch = root / "project", root / "scratch"
        project.mkdir()
        scratch.mkdir()
        path = project / "main.plum"
        lines = ["// a comment before the code", "let number (): Int = {",
                 "    let value = 7;", "    value", "}",
                 "let main (): Unit = println(number().to_string())", ""]
        client = Client(compiler, scratch)
        try:
            client.request("initialize")
            for endings in [["\n"] * 6, ["\r"] * 6, ["\r\n"] * 6,
                            ["\r\n", "\r", "\n", "\r", "\r\n", "\n"]]:
                text = "".join(line + ending for line, ending in zip(lines, endings))
                # write_bytes avoids Python's/platform's newline translation.
                path.write_bytes(text.encode())
                client.notify("didOpen", path, text)
                params = {"textDocument": {"uri": uri(path)},
                          "position": {"line": 3, "character": 4}}
                hover = client.request("textDocument/hover", params)
                assert hover is not None and "Int" in hover["contents"]["value"], (endings, hover)
                definition = client.request("textDocument/definition", params)
                assert definition["range"] == {
                    "start": {"line": 2, "character": 8},
                    "end": {"line": 2, "character": 13}}, (endings, definition)
                selection = client.request("textDocument/selectionRange", {
                    "textDocument": {"uri": uri(path)}, "positions": [params["position"]]})
                assert selection[0]["range"] == {
                    "start": {"line": 3, "character": 4},
                    "end": {"line": 3, "character": 9}}, (endings, selection)
                completions = client.request("textDocument/completion", params)
                assert any(item["label"] == "value" for item in completions["items"]), completions

                ir = subprocess.run([compiler, "emit-llvm", str(project)],
                                    capture_output=True, timeout=60)
                assert ir.returncode == 0, ir.stderr.decode()
                # Block functions currently anchor at the final expression.
                assert re.search(rb'DISubprogram\(name: "number",[^\r\n]*line: 4,', ir.stdout), endings
                assert re.search(rb'DISubprogram\(name: "main",[^\r\n]*line: 6,', ir.stdout), endings
                assert b"DILocation(line: 3," in ir.stdout, endings
                assert b"DILocation(line: 4," in ir.stdout, endings

                broken = text.replace("    value", "    missing")
                before = len(client.notifications)
                client.notify("didChange", path, broken)
                client.request("initialize")  # Drain the diagnostic notification.
                diagnostics = [m["params"]["diagnostics"] for m in client.notifications[before:]
                               if m.get("method") == "textDocument/publishDiagnostics"]
                assert diagnostics and diagnostics[-1], (endings, diagnostics)
                assert diagnostics[-1][0]["range"]["start"] == {
                    "line": 3, "character": 4}, (endings, diagnostics)
                path.write_bytes(broken.encode())
                error = subprocess.run([compiler, "check", str(project)], capture_output=True, timeout=60)
                assert error.returncode != 0
                # Windows may translate the compiler's output newlines.
                diagnostic = (error.stdout + error.stderr).replace(b"\r\n", b"\n")
                assert b"main.plum:4:5" in diagnostic, diagnostic
                assert b"4 |     missing\n" in diagnostic, diagnostic
                assert b"\r" not in diagnostic, diagnostic
                client.notify("didClose", path)
            assert client.request("shutdown") is None
            client.send("exit")
            client.proc.wait(timeout=10)
            assert client.proc.returncode == 0, client.proc.stderr.read().decode()
        finally:
            if client.proc.poll() is None:
                client.proc.kill()
                client.proc.wait()
    print("source positions across line endings: ok")


if __name__ == "__main__":
    check(sys.argv[1])
