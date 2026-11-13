#!/usr/bin/env python3
"""Bounded BIOS/QEMU hello check: exact serial bytes and CPU in kernel halt."""
import argparse
import json
import os
from pathlib import Path
import re
import select
import subprocess
import tempfile
import time

HERE = Path(__file__).resolve().parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=HERE / 'build')
    parser.add_argument('--timeout', type=float, default=20)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error('--timeout must be positive')
    out = args.output.resolve()
    iso, elf = out / 'kernel.iso', out / 'kernel.elf'
    symbols = subprocess.check_output(['nm', '-S', elf], text=True)
    match = re.search(r'^([0-9a-f]+) ([0-9a-f]+) T plum_kernel_halt$', symbols, re.M)
    if not match:
        parser.error('missing sized plum_kernel_halt symbol')
    start, size = (int(value, 16) for value in match.groups())
    version = subprocess.check_output(['qemu-system-x86_64', '--version'], text=True).splitlines()[0]
    deadline = time.monotonic() + args.timeout
    with tempfile.TemporaryDirectory(prefix='plum-hello-smoke-') as tmp:
        serial = Path(tmp) / 'serial.log'
        command = ['qemu-system-x86_64', '-machine', 'pc', '-accel', 'tcg', '-cpu', 'qemu64',
                   '-smp', '1', '-m', '128M', '-cdrom', str(iso), '-boot', 'd',
                   '-display', 'none', '-serial', f'file:{serial}', '-monitor', 'none',
                   '-qmp', 'stdio', '-no-reboot', '-no-shutdown']
        # QMP uses pipes; no listening TCP port or Unix socket is required.
        with (Path(tmp) / 'qemu.stderr').open('w+b') as errors:
            process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors)
            pending = bytearray()

            def receive():
                while b'\n' not in pending:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or not select.select([process.stdout], [], [], remaining)[0]:
                        raise TimeoutError('QEMU did not reach the expected kernel halt')
                    data = os.read(process.stdout.fileno(), 65536)
                    if not data:
                        raise RuntimeError('QEMU exited before success')
                    pending.extend(data)
                line, _, rest = pending.partition(b'\n')
                pending[:] = rest
                return json.loads(line)

            def execute(name, arguments=None):
                process.stdin.write((json.dumps({'execute': name, 'arguments': arguments or {}}) + '\n').encode())
                process.stdin.flush()
                while True:
                    response = receive()
                    if 'error' in response:
                        raise RuntimeError(response)
                    if response.get('event') in ('SHUTDOWN', 'GUEST_PANICKED'):
                        raise RuntimeError(response)
                    if 'return' in response:
                        return response['return']

            try:
                if 'QMP' not in receive():
                    raise RuntimeError('missing QMP greeting')
                execute('qmp_capabilities')
                while True:
                    registers = execute('human-monitor-command', {'command-line': 'info registers'})
                    pc = re.search(r'\bRIP=([0-9a-fA-F]+)', registers)
                    if 'HLT=1' in registers and pc and start <= int(pc.group(1), 16) < start + size:
                        data = serial.read_bytes() if serial.exists() else b''
                        if data == b'PLUM_KERNEL_OK\n':
                            break
                        if data:
                            raise RuntimeError(f'unexpected serial output: {data!r}')
                    if time.monotonic() >= deadline:
                        raise TimeoutError('QEMU did not reach the expected kernel halt')
                    time.sleep(0.05)
                if process.poll() is not None:
                    raise RuntimeError('QEMU exited unexpectedly')
                (out / 'serial.log').write_bytes(data)
                (out / 'halt-registers.txt').write_text(registers)
                manifest = json.loads((out / 'toolchain.json').read_text())
                manifest['qemu-system-x86_64'] = version
                manifest['qemu_machine'] = 'pc, tcg, qemu64, smp=1, memory=128M, BIOS ISO'
                (out / 'toolchain.json').write_text(json.dumps(manifest, indent=2) + '\n')
                print('kernel-hello: exact Plum marker and CPU halted in plum_kernel_halt passed')
            except Exception:
                errors.seek(0)
                print(errors.read().decode(errors='replace'))
                if serial.exists():
                    print(f'Serial: {serial.read_bytes()!r}')
                raise
            finally:
                if process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                process.stdin.close()
                process.stdout.close()


if __name__ == '__main__':
    main()
