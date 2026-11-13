#!/usr/bin/env python3
"""Build the pinned BIOS-only Plum kernel ISO. Run from any directory."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
SUPPORT = ROOT / 'runtime/freestanding/x86_64'
COMMIT = 'aad3edd370955449717a334f0289dee10e2c5f01'  # v8.7.0-binary
URL = f'https://codeload.github.com/Limine-Bootloader/Limine/tar.gz/{COMMIT}'
SHA256 = '9c23ca261778cbe015041c02220ce26f45e05f38fe3e0c9913be71961f9023e5'
HEADER_SHA256 = 'f8b35287579dbe50e9d84cca7b558e39b2b64e499f9b37ef5ebe969c05104c33'
# Extract only the BIOS image/tool sources and matching protocol header/license.
INPUTS = ('limine.h', 'LICENSE', 'limine.c', 'limine-bios-hdd.h',
          'limine-bios.sys', 'limine-bios-cd.bin')


def run(args):
    subprocess.run(list(map(str, args)), check=True)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--plum', type=Path, default=ROOT / 'sh.real',
                        help='compiler built from current source (default: ./sh.real)')
    parser.add_argument('--output', type=Path, default=HERE / 'build')
    parser.add_argument('--limine-archive', type=Path,
                        help='use an offline copy of the pinned archive; still checksum-verified')
    parser.add_argument('--release', action='store_true')
    args = parser.parse_args()
    compiler = args.plum.resolve()
    if not compiler.is_file():
        parser.error(f'build a current-source compiler first: {compiler}')
    for tool in ('clang', 'ld.lld', 'xorriso', 'nm', 'readelf'):
        if not shutil.which(tool):
            parser.error(f'missing required tool: {tool}')
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    archive = args.limine_archive.resolve() if args.limine_archive else out / 'limine-v8.7.0-binary.tar.gz'
    if not archive.exists():
        if args.limine_archive:
            parser.error(f'offline archive does not exist: {archive}')
        # Publish the cache only after a complete, verified download.
        with tempfile.TemporaryDirectory(prefix='limine-download-', dir=out) as tmp:
            download = Path(tmp) / 'archive.tar.gz'
            run(['curl', '--fail', '--location', '--retry', '2', '--output', download, URL])
            if digest(download) != SHA256:
                parser.error('Limine archive checksum mismatch')
            download.replace(archive)
    if digest(archive) != SHA256:
        parser.error('Limine archive checksum mismatch')
    deps = out / 'limine'
    deps.mkdir(exist_ok=True)
    with tarfile.open(archive, 'r:gz') as tar:
        prefix = f'Limine-{COMMIT}/'
        for name in INPUTS:
            member = tar.getmember(prefix + name)
            if not member.isfile():
                parser.error(f'expected regular archive member: {name}')
            with tar.extractfile(member) as source:
                (deps / name).write_bytes(source.read())
    if digest(deps / 'limine.h') != HEADER_SHA256:
        parser.error('Limine protocol header checksum mismatch')
    # Build the host installer; this executable is never linked into the kernel.
    installer = deps / 'limine'
    run(['clang', '-std=c99', '-O2', deps / 'limine.c', '-o', installer])
    flags = ['--target=x86_64-unknown-none', '-march=x86-64', '-mcmodel=kernel',
             '-mno-red-zone', '-ffreestanding', '-fno-builtin', '-fno-stack-protector',
             '-fno-pic', '-fno-pie', '-fno-unwind-tables', '-fno-asynchronous-unwind-tables',
             '-g', '-O2' if args.release else '-O0', '-Wall', '-Wextra', '-Werror',
             '-I', str(deps)]
    objects = []
    for source in (HERE / 'boot/start.S', HERE / 'boot/adapter.c', HERE / 'boot/hardware.c',
                   SUPPORT / 'kernel.c', SUPPORT / 'memory.c', SUPPORT / 'platform.c'):
        obj = out / (source.stem + '.o')
        run(['clang', *flags, '-c', source, '-o', obj])
        objects += ['--link-object', obj]
    elf = out / 'kernel.elf'
    run([compiler, 'build', HERE, '--target', 'x86_64-unknown-none',
         *(['--release'] if args.release else []), *objects,
         '--linker-script', HERE / 'boot/kernel.ld', '-o', elf])
    # Fail closed on accidental host dependencies, even before the QEMU smoke.
    if subprocess.check_output(['nm', '-u', elf]).strip():
        raise RuntimeError('kernel has undefined symbols')
    if b'NEEDED' in subprocess.check_output(['readelf', '-d', elf]):
        raise RuntimeError('kernel has dynamic dependencies')
    if b'INTERP' in subprocess.check_output(['readelf', '-l', elf]):
        raise RuntimeError('kernel has a host loader')
    # Stage in a fresh directory so stale output cannot leak into the image.
    with tempfile.TemporaryDirectory(prefix='iso-root-', dir=out) as tmp:
        stage = Path(tmp)
        (stage / 'boot/limine').mkdir(parents=True)
        shutil.copyfile(elf, stage / 'boot/kernel.elf')
        for name in ('limine-bios.sys', 'limine-bios-cd.bin', 'LICENSE'):
            shutil.copyfile(deps / name, stage / 'boot/limine' / name)
        shutil.copyfile(HERE / 'boot/limine.conf', stage / 'boot/limine/limine.conf')
        iso = out / 'kernel.iso'
        run(['xorriso', '-as', 'mkisofs', '-R', '-r', '-J',
             '-b', 'boot/limine/limine-bios-cd.bin', '-no-emul-boot',
             '-boot-load-size', '4', '-boot-info-table', '-o', iso, stage])
        run([installer, 'bios-install', iso])
    versions = {}
    for tool in ('clang', 'ld.lld', 'xorriso'):
        option = '-version' if tool == 'xorriso' else '--version'
        result = subprocess.run([tool, option], text=True, capture_output=True, check=True)
        versions[tool] = result.stdout.splitlines()[0]
    versions['plum'] = str(compiler)
    versions['limine'] = {'version': '8.7.0', 'commit': COMMIT, 'url': URL,
                          'archive_sha256': SHA256, 'header_sha256': HEADER_SHA256}
    (out / 'toolchain.json').write_text(json.dumps(versions, indent=2) + '\n')
    print(f'Built {iso}\nToolchain recorded in {out / "toolchain.json"}')


if __name__ == '__main__':
    main()
