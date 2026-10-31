#!/usr/bin/env python3
"""Check JSON-RPC errors and lifecycle transitions through the real stdio server."""
import json
import subprocess
import sys


def frame(message):
    body = message.encode() if isinstance(message, str) else json.dumps(message).encode()
    return b"Content-Length: %d\r\n\r\n" % len(body) + body


def request(method, ident=None, **extra):
    message = {"jsonrpc": "2.0", "method": method, **extra}
    if ident is not None:
        message["id"] = ident
    return message


def run(compiler, messages, status=0):
    result = subprocess.run([compiler, "lsp"], input=b"".join(map(frame, messages)),
                            capture_output=True, timeout=60)
    assert result.returncode == status, (result.returncode, result.stderr)
    data, replies = result.stdout, []
    while data:
        header, data = data.split(b"\r\n\r\n", 1)
        length = int(header.split(b":", 1)[1])
        replies.append(json.loads(data[:length]))
        data = data[length:]
    return replies


def error(message, ident, code):
    assert set(message) == {"jsonrpc", "id", "error"}, message
    assert message["jsonrpc"] == "2.0" and message["id"] == ident, message
    assert message["error"]["code"] == code, message
    assert isinstance(message["error"]["message"], str), message


def check(compiler):
    initialize = request("initialize", 1, params={})
    shutdown = request("shutdown", 2)
    exit_message = request("exit")
    assert run(compiler, [exit_message], 1) == []
    assert len(run(compiler, [initialize, exit_message], 1)) == 1
    assert run(compiler, []) == []  # Transport EOF preserves existing behavior.

    document = {"textDocument": {"uri": "file:///nonexistent-lifecycle.plum", "text": "bad"}}
    messages = [
        request("initialized"), request("initialize"),  # Notifications cannot initialize.
        request("textDocument/didOpen", params=document),
        request("textDocument/hover", "early", params={}),
        shutdown, initialize,
        request("initialized"), request("initialize", "duplicate"),
        request("shutdown"),  # A notification cannot shut down the server.
        request("unknown", "unsupported"), request("unknown"),
        request("exit", "not-a-notification"),
        request("textDocument/formatting", 3, params={}),
        shutdown,
        request("initialize", "restart"), request("shutdown", "again"),
        request("textDocument/hover", "late", params={}),
        request("textDocument/didOpen", params=document),
        request("textDocument/didChange", params=document),
        request("textDocument/didClose", params=document),
        request("unknown"), exit_message,
        request("unknown", "after-exit"),
    ]
    replies = run(compiler, messages)
    assert len(replies) == 11, replies
    for index, ident, code in [(0, "early", -32002), (1, 2, -32002),
                               (3, "duplicate", -32600), (4, "unsupported", -32601),
                               (5, "not-a-notification", -32601),
                               (8, "restart", -32600), (9, "again", -32600),
                               (10, "late", -32600)]:
        error(replies[index], ident, code)
    assert "capabilities" in replies[2]["result"], replies
    assert replies[6] == {"jsonrpc": "2.0", "id": 3, "result": []}, replies
    assert replies[7] == {"jsonrpc": "2.0", "id": 2, "result": None}, replies

    malformed = [
        ("{", None, -32700),
        ([], None, -32600), (42, None, -32600),
        ({"jsonrpc": "1.0", "method": "initialize", "id": "version"}, "version", -32600),
        ({"jsonrpc": "2.0", "id": "missing-method"}, "missing-method", -32600),
        ({"jsonrpc": "2.0", "method": 7, "id": 9}, 9, -32600),
        ({"method": "initialize", "id": "missing-version"}, "missing-version", -32600),
    ]
    for ident in [True, [], {}]:
        malformed.append(({"jsonrpc": "2.0", "method": "exit", "id": ident}, None, -32600))
    for params in [None, True, 3, "text"]:
        malformed.append((request("shutdown", "bad-params", params=params), "bad-params", -32600))
    # Invalid envelopes get errors even without an id; valid notifications do not.
    malformed.append(({"jsonrpc": "2.0", "method": False}, None, -32600))
    for prefix in [[], [initialize], [initialize, shutdown]]:
        suffix = [initialize, shutdown][len(prefix):]
        replies = run(compiler, prefix + [item[0] for item in malformed] + suffix + [exit_message])
        for reply, (_, ident, code) in zip(replies[len(prefix):len(prefix) + len(malformed)], malformed, strict=True):
            error(reply, ident, code)
        assert len(replies) == len(malformed) + 2, replies
        if not prefix:
            assert "capabilities" in replies[-2]["result"], replies
        if len(prefix) < 2:
            assert replies[-1] == {"jsonrpc": "2.0", "id": 2, "result": None}, replies

    # JSON-RPC null ids are discouraged but still require a reply.
    replies = run(compiler, [initialize, {"jsonrpc": "2.0", "method": "unknown", "id": None},
                             request("unknown", 42), shutdown, exit_message])
    error(replies[1], None, -32601)
    error(replies[2], 42, -32601)

    # A long stream of ignored document notifications must produce no side
    # effects or responses, and must still leave the server usable.
    ignored = request("textDocument/didOpen", params={"textDocument": {
        "uri": "file:///nonexistent-lifecycle.plum", "text": "x" * 8192}})
    replies = run(compiler, [ignored] * 2000 + [initialize, shutdown] +
                  [ignored] * 2000 + [exit_message])
    assert [reply["id"] for reply in replies] == [1, 2], replies
    print("lsp lifecycle and JSON-RPC errors: ok")


if __name__ == "__main__":
    check(sys.argv[1])
