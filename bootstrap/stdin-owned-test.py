#!/usr/bin/env python3
"""Exercise the public wrappers, including overlapping native-to-Plum copies."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
PLUM = str(Path(sys.argv[1]).resolve())


def run(args, **kwargs):
    return subprocess.run(args, check=True, timeout=120, **kwargs)


with tempfile.TemporaryDirectory(prefix="plum-stdin-owned-") as directory:
    work = Path(directory)
    project = work / "concurrent"
    project.mkdir()
    (project / "main.plum").write_text('''use Os;
let read_lines (tx: Sender[String]) (remaining: Int): Unit = {
    if remaining <= 0 { () } else {
        let line = match Os.read_stdin_line() {
            Ok(Some(line)) => line,
            _ => "unexpected read result",
        };
        tx.send(line);
        read_lines(tx, remaining - 1)
    }
}
let receive_lines (rx: Receiver[String]) (remaining: Int): Unit = {
    if remaining <= 0 { () } else {
        match rx.recv() { Some(line) => println(line), None => println("closed") };
        receive_lines(rx, remaining - 1)
    }
}
let main (): Unit = {
    let (tx, rx) = channel[String]();
    let a = spawn { read_lines(tx, 8) };
    let b = spawn { read_lines(tx, 8) };
    let c = spawn { read_lines(tx, 8) };
    let d = spawn { read_lines(tx, 8) };
    receive_lines(rx, 32);
    a.join(); b.join(); c.join(); d.join()
}
''', encoding="utf-8")
    binary = str(work / "concurrent.exe")
    run([PLUM, "build", str(project), "-o", binary], capture_output=True)
    lines = [f"{i:04}:".encode() + bytes([65 + i % 26]) * 16384 for i in range(32)]
    result = run([binary], input=b"\n".join(lines) + b"\n", capture_output=True)
    assert sorted(result.stdout.splitlines()) == sorted(lines), result.stdout[:500]
    print("stdin public ownership: 32 large lines across 4 tasks, no loss or overwrites", flush=True)

    project = work / "errors"
    project.mkdir()
    (project / "main.plum").write_text('''use Os;
use Time;
let main (): Unit = {
    assert(match Os.read_stdin_line_timeout(Time.seconds(1)) {
        Err(_) => true, _ => false,
    });
    assert(match Os.read_stdin_line() { Ok(Some(line)) => line == "after", _ => false });
    assert(match Os.read_stdin_timeout(4, Time.seconds(1)) {
        Ok(Got(bytes)) => bytes.to_hex() == "001a0d0a", _ => false,
    });
    assert(match Os.read_stdin_line_timeout(Time.seconds(1)) {
        Ok(Eof) => true, _ => false,
    });
    println("stdin error and binary ownership: ok")
}
''', encoding="utf-8")
    binary = str(work / "errors.exe")
    run([PLUM, "build", str(project), "-o", binary], capture_output=True)
    # Use a file here so all four binary bytes are ready in one read.
    # Pipe readiness and short reads are covered separately by stdin-smoke.
    data = work / "input"
    data.write_bytes(b"\xff\nafter\n\x00\x1a\r\n")
    with data.open("rb") as source:
        result = run([binary], stdin=source, capture_output=True)
    assert result.stdout == b"stdin error and binary ownership: ok\n", result
    print(result.stdout.decode().strip(), flush=True)
