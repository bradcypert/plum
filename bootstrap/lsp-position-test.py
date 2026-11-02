#!/usr/bin/env python3
"""Untrusted position magnitudes cannot stall the stdio request loop."""
import importlib.util
import subprocess
import sys
import tempfile
from pathlib import Path

from lsp_test_support import file_uri

spec = importlib.util.spec_from_file_location("lifecycle", Path(__file__).with_name("lsp-lifecycle-test.py"))
lifecycle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lifecycle)


def check(compiler):
    with tempfile.TemporaryDirectory(prefix="plum-lsp-position-") as directory:
        path = Path(directory) / "main.plum"
        path.write_text("let main (): Unit = ()\n")
        messages = [lifecycle.request("initialize", 1),
                    lifecycle.request("textDocument/selectionRange", "invalid", params={
                        "textDocument": {"uri": file_uri(path)},
                        "positions": [{"line": 1e100, "character": 0}]}),
                    lifecycle.request("textDocument/selectionRange", 2, params={
                        "textDocument": {"uri": file_uri(path)},
                        "positions": [{"line": 2147483647, "character": 2147483647}]}),
                    lifecycle.request("shutdown", 3), lifecycle.request("exit")]
        result = subprocess.run([compiler, "lsp"],
                                input=b"".join(map(lifecycle.frame, messages)),
                                capture_output=True, timeout=5)
        assert result.returncode == 0, result.stderr
        assert b'"id":2,"result":' in result.stdout, result.stdout
        assert b'"id":"invalid","error":' in result.stdout, result.stdout
        assert b'"id":3,"result":null' in result.stdout, result.stdout
        methods = ["textDocument/hover", "textDocument/definition",
                   "textDocument/completion", "textDocument/selectionRange"]
        invalid = [None, True, 1, "position", [], {}, {"line": 0}, {"character": 0}]
        for value in [-1, -0.5, 0.5, 2147483648, 1e100, "0", None, True, [], {}]:
            invalid.extend([{"line": value, "character": 0},
                            {"line": 0, "character": value}])
        messages = [lifecycle.request("initialize", "init")]
        expected_errors = []
        for method in methods:
            for position in invalid:
                params = {"textDocument": {"uri": file_uri(path)}}
                if method.endswith("selectionRange"):
                    # A valid first position cannot hide an invalid later entry.
                    params["positions"] = [{"line": 0, "character": 0}, position]
                else:
                    params["position"] = position
                ident = len(expected_errors) + 1
                messages.append(lifecycle.request(method, ident, params=params))
                expected_errors.append(ident)
                # Invalid notifications must be ignored without replying.
                messages.append(lifecycle.request(method, params=params))
            for params in [{}, {"positions": None}, {"positions": {}}, {"positions": 1}]:
                if method.endswith("selectionRange") or params == {}:
                    ident = len(expected_errors) + 1
                    messages.append(lifecycle.request(method, ident, params=params))
                    expected_errors.append(ident)
        messages += [lifecycle.request("shutdown", "stop"), lifecycle.request("exit")]
        replies = lifecycle.run(compiler, messages)
        assert len(replies) == len(expected_errors) + 2, replies
        for reply, ident in zip(replies[1:-1], expected_errors, strict=True):
            lifecycle.error(reply, ident, -32602)
        assert replies[-1]["id"] == "stop" and replies[-1]["result"] is None

        # Clamping must use the unsaved buffer, including its line endings,
        # and send the same normalized position to compiler subprocesses.
        for ending in ["\n", "\r", "\r\n"]:
            text = ending.join(["let main (): Unit = {", "    let local = 7;", "    println(local.to_string())", "}"])
            messages = [lifecycle.request("initialize", "init"),
                        lifecycle.request("textDocument/didOpen", params={
                            "textDocument": {"uri": file_uri(path), "text": text}})]
            pairs = []
            for method in methods:
                for large, clamped in [({"line": 2147483647, "character": 2147483647},
                                        {"line": 3, "character": 1}),
                                       ({"line": 2, "character": 2147483647},
                                        {"line": 2, "character": len(text.split(ending)[2])}),
                                       ({"line": 0, "character": 0},
                                        {"line": 0.0, "character": -0.0})]:
                    ids = []
                    for position in [large, clamped]:
                        ident = len(messages)
                        ids.append(ident)
                        params = {"textDocument": {"uri": file_uri(path)}}
                        params["positions" if method.endswith("selectionRange") else "position"] = (
                            [position] if method.endswith("selectionRange") else position)
                        messages.append(lifecycle.request(method, ident, params=params))
                    pairs.append(ids)
            messages += [lifecycle.request("textDocument/selectionRange", "empty", params={
                "textDocument": {"uri": file_uri(path)}, "positions": []}),
                lifecycle.request("shutdown", "stop"), lifecycle.request("exit")]
            replies = {reply["id"]: reply for reply in lifecycle.run(compiler, messages) if "id" in reply}
            for large, clamped in pairs:
                assert replies[large]["result"] == replies[clamped]["result"], (ending, replies[large], replies[clamped])
            assert replies["empty"]["result"] == []
            assert replies["stop"]["result"] is None
    print("lsp bounded position conversion: ok")


if __name__ == "__main__":
    check(sys.argv[1])
