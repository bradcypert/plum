#!/usr/bin/env python3
"""Check Windows listener exclusivity and POSIX wildcard listener conflicts."""
import errno
import os
from pathlib import Path
import queue
import socket
import subprocess
import tempfile
import threading

ROOT = Path(__file__).resolve().parent.parent


def main():
    with tempfile.TemporaryDirectory(prefix="plum-listener-") as work:
        exe = Path(work) / "listener-test.exe"
        command = ["clang", "-O1", "-g", "-Wall", "-Wextra", "-Werror",
                   str(ROOT / "bootstrap/listener-test.c"), "-o", str(exe)]
        if os.name == "nt":
            command += ["-lws2_32"]
        subprocess.run(command, check=True, timeout=60)
        subprocess.run([str(exe)], check=True, timeout=15)
        owner = subprocess.Popen([str(exe), "--hold"], stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, text=True)
        try:
            lines = queue.Queue()
            threading.Thread(target=lambda: lines.put(owner.stdout.readline()),
                             daemon=True).start()
            port = int(lines.get(timeout=10))
            # Windows exclusivity must reject both wildcard and specific binds.
            # POSIX preserves SO_REUSEADDR: BSD/macOS can allow a specific bind
            # alongside a wildcard listener, so only require wildcard conflicts.
            hosts = ("0.0.0.0", "127.0.0.1") if os.name == "nt" else ("0.0.0.0",)
            for host in hosts:
                for reuse in (False, True):
                    with socket.socket() as attacker:
                        if reuse:
                            attacker.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                        try:
                            attacker.bind((host, port))
                        except OSError as error:
                            if os.name == "nt":
                                # Python translates errno to a POSIX equivalent;
                                # winerror preserves the native Winsock code.
                                assert error.winerror in (10013, 10048), error
                            else:
                                assert error.errno == errno.EADDRINUSE, error
                        else:
                            raise AssertionError(f"listener hijacked: {host=}, {reuse=}")
            # The original listener still receives connections after the attempts.
            with socket.create_connection(("127.0.0.1", port), timeout=5):
                pass
            owner.communicate("x\n", timeout=10)
            assert owner.returncode == 0, owner.returncode
        finally:
            if owner.poll() is None:
                owner.kill()
            owner.wait(timeout=10)
        print("separate-process listener binding conflicts rejected")


if __name__ == "__main__":
    main()
