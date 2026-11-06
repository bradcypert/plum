#!/usr/bin/env python3
"""Verify native buffer ownership, scoped mutation, workers and panic cleanup."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
compiler = str(Path(sys.argv[1] if len(sys.argv) > 1 else './sh.real').resolve())


def main():
    with tempfile.TemporaryDirectory(prefix='plum-native-buffer-') as scratch:
        work = Path(scratch)
        env = dict(os.environ, ASAN_OPTIONS='detect_leaks=1:halt_on_error=1',
                   LSAN_OPTIONS='exitcode=23', PLUM_CACHE=str(work / 'cache'))
        env.pop('PLUM_CC', None)

        def run(args, *, fails=False, contains=None, overlay=None):
            r = subprocess.run([str(a) for a in args], cwd=root, env=env | (overlay or {}),
                               capture_output=True, text=True, timeout=300)
            if (r.returncode != 0) != fails or (contains and contains not in r.stdout + r.stderr):
                raise RuntimeError(f'{args}: exit {r.returncode}\n{r.stdout[-5000:]}\n{r.stderr[-9000:]}')
            return r

        fixture = root / 'bootstrap/exec_corpus/native_buffer'
        for index, flags in enumerate([[], ['--release'], ['--memory-stats'], ['--release', '--memory-stats']]):
            binary = work / ('owned' + str(index))
            run([compiler, 'build', fixture, '--sanitize=address', *flags, '-o', binary])
            run([binary], contains='native buffers: bounded text/binary edits, aliases, leases, copies and cleanup')
        escaped = work / 'escaped'
        run([compiler, 'build', root / 'bootstrap/sanitizer_corpus/native_buffer_escape',
             '--sanitize=address', '--memory-stats', '-o', escaped])
        run([escaped], fails=True, contains='heap-use-after-free')

        # Verify the source C primitives too: 64 real socketpair readers and
        # deterministic SSL-read adapters leave no TLS scratch on worker exit.
        archive = run([root / 'bootstrap/mbedtls-build', work / 'mbedtls'],
                      overlay={'CC': 'clang', 'CFLAGS': '-O1 -g -fsanitize=address'}).stdout.strip()
        unit = work / 'native'
        run(['clang', '-g', '-O1', '-fsanitize=address', '-DPLUM_HEAP_STATS',
             '-I' + str(root / 'vendor/mbedtls/include'), root / 'bootstrap/native-buffer-test.c',
             root / 'native_stdlib/native_buffer_shim.c', root / 'native_stdlib/net_shim.c',
             archive, '-lpthread', '-lm', '-o', unit])
        run([unit], contains='64 TCP/TLS workers release every buffer and lease')

        # Exercise the actual Plum task/Net.read_bytes ownership path too,
        # rather than relying only on explicit C release calls in the unit test.
        workers = work / 'workers'
        (workers / 'native').mkdir(parents=True)
        (workers / 'plum.pkg').write_text('Package { name: "buffer_workers", native_deps: [] }\n')
        (workers / 'main.plum').write_text('''use NativeBuffer;
use Net;
extern "C" { fn ready_socket() -> Int; }
let worker (): Unit = {
    let fd = unsafe { ready_socket() };
    assert(fd >= 0);
    let result = Net.read_bytes(fd, 4);
    Net.close(fd);
    match result { Err(e) => panic_raw(e), Ok(bytes) => assert_eq(bytes.to_hex(), "4100ff42") }
}
let run_worker (): Unit = { let task = spawn { worker() }; task.join() }
let live (): Int = match NativeBuffer.snapshot() { Some(s) => s.live_buffers, None => -1 }
let main (): Unit = {
    let before = live();
    for i in 0..32 { run_worker(); assert_eq(live(), before); };
    println("Plum network workers restore native buffer baseline")
}
''')
        (workers / 'native/producer.c').write_text('''#include <sys/socket.h>
#include <unistd.h>
long long ready_socket(void) {
    int pair[2];
    unsigned char data[4] = {65, 0, 255, 66};
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair)) return -1;
    if (write(pair[0], data, 4) != 4) { close(pair[0]); close(pair[1]); return -1; }
    close(pair[0]);
    return pair[1];
}
''')
        worker_binary = work / 'worker-bin'
        run([compiler, 'build', workers, '--sanitize=address', '--memory-stats', '-o', worker_binary])
        run([worker_binary], contains='Plum network workers restore native buffer baseline')

        panic = work / 'panic'
        (panic / 'native').mkdir(parents=True)
        (panic / 'plum.pkg').write_text('Package { name: "buffer_panic", native_deps: [] }\n')
        (panic / 'main.plum').write_text('''use NativeBuffer;
extern "C" { fn observe_cleanup(); }
let main (): Unit = unsafe {
    observe_cleanup();
    match NativeBuffer.create(32) {
        Err(e) => panic_raw(e),
        Ok(buffer) => {
            NativeBuffer.with_mut(buffer, |pointer, cap| {
                panic_raw("expected buffer panic"); Ok(0)
            }); ()
        },
    }
}
''')
        (panic / 'native/observer.c').write_text('''#include <stdio.h>
#include <stdlib.h>
extern long long plum_buffer_snapshot(void);
extern long long plum_buffer_snapshot_field(long long, long long);
static void verify(void) {
    long long token = plum_buffer_snapshot();
    if (!token || plum_buffer_snapshot_field(token, 2) || plum_buffer_snapshot_field(token, 3)) {
        fputs("native buffer panic cleanup failed\\n", stderr); _Exit(91);
    }
    puts("native buffer panic cleanup: zero live buffers/capacity");
}
void observe_cleanup(void) { if (atexit(verify)) abort(); }
''')
        binary = work / 'panic-bin'
        run([compiler, 'build', panic, '--sanitize=address', '--memory-stats', '-o', binary])
        r = run([binary], fails=True, contains='native buffer panic cleanup: zero live buffers/capacity')
        if 'ERROR: AddressSanitizer' in r.stderr or 'LeakSanitizer' in r.stderr:
            raise RuntimeError('panic cleanup has a sanitizer finding:\n' + r.stderr)
    print('native-buffer-check: ownership, bounds, leases, counters, TCP/TLS worker exit, escape detection and panic cleanup passed')


if __name__ == '__main__':
    try:
        main()
    except (OSError, subprocess.SubprocessError, RuntimeError) as error:
        sys.exit(str(error))
