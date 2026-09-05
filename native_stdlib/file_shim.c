// Streaming file I/O, behind Plum's `handle` type (issues #16 and #9).
//
// Same ABI conventions as its neighbours (see `net_shim.c`'s header for
// the general pattern): every function takes and returns only
// `long long` (Plum `Int`) or `const char *` (Plum `CStr`), because
// Plum's extern surface has no raw pointers, no out-parameters and no
// multi-value return.
//
// --- Why a SLOT TABLE and not a cast pointer ---
//
// `dir_shim.c` casts its `DIR *` straight to a handle, which is simpler
// and is fine there because a directory is closed exactly once, by the
// one function that reads it to the end.
//
// A file cannot do that, because closing one happens TWICE by design.
// `File.close` is explicit and returns a `Result`, so a caller who cares
// whether the final flush succeeded can see it; and the handle's
// `on_drop` closes it again on the way out, so a caller who forgets --
// or who returns early -- still cannot leak it. That pairing is Rust's
// `File`, and it is the shape issue #16 sketched.
//
// Rust makes the second close impossible: `close` CONSUMES the value.
// Plum has no move semantics, so both really do run, and the second one
// must be harmless. A cast pointer cannot be: `fclose` on an
// already-closed `FILE *` is undefined, and the fd number underneath may
// by then belong to a completely different file. A slot the shim owns
// can simply be marked empty, and every later close on it does nothing.
//
// So the number Plum holds is an INDEX (biased by one so that 0 is never
// a live handle), and the real `FILE *` never leaves this file. That is
// also what the `handle` design meant by "the state does not have to fit
// in the number".
//
// **Not thread-safe**, matching `dir_shim.c` and `process_shim.c`: the
// table is unguarded, so opening and closing files from several threads
// at once is not supported. The read BUFFER is thread-local, because
// that one is cheap to make safe and `net_shim.c` learned the hard way
// what a shared one costs.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if defined(_MSC_VER)
#define PLUM_TLS __declspec(thread)
#else
#define PLUM_TLS _Thread_local
#endif

#define PLUM_MAX_FILES 256

static FILE *slots[PLUM_MAX_FILES];

// A handle is `index + 1`, so a valid one is never 0 and never collides
// with the -1 every wrapper here uses for failure.
static int slot_of(long long h) {
    if (h < 1 || h > PLUM_MAX_FILES) return -1;
    if (slots[h - 1] == NULL) return -1;
    return (int)(h - 1);
}

// Modes are passed as small integers rather than strings: an extern
// taking a `CStr` would mean the caller could hand `fopen` anything at
// all, and the set Plum offers is closed. 0 read, 1 write, 2 append.
//
// All three are BINARY (`b`), which is a no-op on POSIX and load-bearing
// on Windows, where a text-mode stream rewrites `\n` on the way through
// and would corrupt exactly the payloads `Bytes` exists for.
long long file_open(const char *path, long long mode) {
    const char *m = mode == 0 ? "rb" : mode == 1 ? "wb" : mode == 2 ? "ab" : NULL;
    if (m == NULL) return -1;
    for (int i = 0; i < PLUM_MAX_FILES; i++) {
        if (slots[i] == NULL) {
            FILE *f = fopen(path, m);
            if (f == NULL) return -1;
            slots[i] = f;
            return (long long)i + 1;
        }
    }
    return -1;
}

// 0 on success, -1 on a real error. Closing an already-closed handle is
// SUCCESS, not an error: the guard runs after an explicit close on every
// well-written program, and reporting that as a failure would make the
// safe pattern the noisy one.
long long file_close_checked(long long h) {
    int i = slot_of(h);
    if (i < 0) return 0;
    FILE *f = slots[i];
    slots[i] = NULL;
    return fclose(f) == 0 ? 0 : -1;
}

// The `on_drop` hook. A release runs while a value is being freed and
// has nowhere to report to, so the result is discarded here -- which is
// exactly why `file_close_checked` exists alongside it.
void file_close(long long h) {
    file_close_checked(h);
}

// --- Reading ---
//
// Split across two calls for the same reason `tcp_recv_n`/
// `tcp_recv_data` are: a `CStr` return cannot carry a length, so a
// single-call read would have to NUL-terminate and the caller would have
// to `strlen` -- which truncates any file containing a zero byte, i.e.
// every file this API exists to read.
//
// Three outcomes, kept apart:
//   > 0   that many bytes are in the buffer
//   = 0   end of file
//   < 0   a read error
static PLUM_TLS char *read_buf = NULL;
static PLUM_TLS size_t read_cap = 0;

static int read_ensure(size_t need) {
    if (read_cap >= need) return 1;
    size_t next = read_cap ? read_cap : 4096;
    while (next < need) next *= 2;
    char *bigger = (char *)realloc(read_buf, next);
    if (bigger == NULL) return 0;
    read_buf = bigger;
    read_cap = next;
    return 1;
}

long long file_read_n(long long h, long long max_len) {
    int i = slot_of(h);
    if (i < 0) return -1;
    if (max_len < 0) max_len = 0;
    if (!read_ensure((size_t)max_len + 1)) return -1;
    size_t got = fread(read_buf, 1, (size_t)max_len, slots[i]);
    if (got == 0 && ferror(slots[i])) return -1;
    read_buf[got] = '\0';
    return (long long)got;
}

// The bytes `file_read_n` just read. Valid until the next read on this
// thread, and meaningful only with the count that call returned -- the
// buffer is not NUL-delimited data, and reading it with `strlen` is the
// bug this pair exists to avoid.
const char *file_read_data(void) {
    return read_buf ? read_buf : "";
}

// Bytes written, or -1. A short write is possible under real POSIX
// semantics and is NOT retried here; the Plum wrapper reports the count
// and lets the caller decide, matching `tcp_send`'s own scope note.
long long file_write(long long h, const char *buf, long long len) {
    int i = slot_of(h);
    if (i < 0) return -1;
    if (len < 0) return -1;
    size_t wrote = fwrite(buf, 1, (size_t)len, slots[i]);
    return (long long)wrote;
}

// The new absolute position, or -1. `whence` is 0 start, 1 current,
// 2 end -- the same order `SEEK_SET`/`SEEK_CUR`/`SEEK_END` are usually
// written in, but passed as Plum's own constants rather than assuming
// the C macros have those values on every platform.
long long file_seek(long long h, long long offset, long long whence) {
    int i = slot_of(h);
    if (i < 0) return -1;
    int w = whence == 0 ? SEEK_SET : whence == 1 ? SEEK_CUR : whence == 2 ? SEEK_END : -1;
    if (w == -1) return -1;
    if (fseek(slots[i], (long)offset, w) != 0) return -1;
    return (long long)ftell(slots[i]);
}

// Flushes buffered writes without closing, so a caller can make data
// visible to another process and keep the handle.
long long file_flush(long long h) {
    int i = slot_of(h);
    if (i < 0) return -1;
    return fflush(slots[i]) == 0 ? 0 : -1;
}
