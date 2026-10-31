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
// by then belong to a completely different file.
//
// Each opening gets a positive identity: slot index plus a generation.
// Reusing storage never reuses an identity. At Int exhaustion the slot
// is retired rather than wrapping to an identity a stale value may hold.
// Closed handles need no heap record or tombstone: the slot remembers
// only its last identity, so memory stays bounded across repeated opens.
//
// Each slot has a mutex covering identity checks AND the entire stdio
// operation, including fclose. Locking just lookup would leave a FILE *
// vulnerable to concurrent close. Separate slots can perform I/O in
// parallel; allocation skips reserved slots without waiting on their
// I/O locks. Concurrent operations on one file are serialized. The read
// buffer remains thread-local, independent of the slot's lifetime.

#if defined(__APPLE__)
#define _DARWIN_C_SOURCE 1
#endif
#if !defined(_WIN32)
#ifndef _FILE_OFFSET_BITS
#define _FILE_OFFSET_BITS 64
#endif
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#endif

#include <limits.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include "path_shim.h"
#include <stdlib.h>
#include <string.h>
#if !defined(_WIN32)
#include <sys/types.h>
// Fail at build time if the toolchain disables large-file support.
_Static_assert(sizeof(off_t) >= sizeof(long long), "File.seek requires 64-bit off_t");
#endif

#if defined(_MSC_VER)
#define PLUM_TLS __declspec(thread)
#else
#define PLUM_TLS _Thread_local
#endif

#define PLUM_MAX_FILES 256

typedef struct {
    pthread_mutex_t lock;
    FILE *stream;
    long long identity;
    int reserved; // Protected by slots_allocator, not the I/O lock.
} PlumFileSlot;

static PlumFileSlot slots[PLUM_MAX_FILES];
// Allocation never holds this mutex while waiting on I/O or a slot lock.
static pthread_mutex_t slots_allocator = PTHREAD_MUTEX_INITIALIZER;
static pthread_once_t slots_once = PTHREAD_ONCE_INIT;
static int slots_ready;

static void slots_init(void) {
    for (int i = 0; i < PLUM_MAX_FILES; i++) {
        if (pthread_mutex_init(&slots[i].lock, NULL) != 0) {
            while (i > 0) pthread_mutex_destroy(&slots[--i].lock);
            return;
        }
    }
    slots_ready = 1;
}

// Returns a locked live slot, or NULL. No caller may keep its FILE *
// after unlocking. Positive identities encode (identity - 1) % 256.
static PlumFileSlot *slot_lock(long long h) {
    if (h <= 0) return NULL;
    pthread_once(&slots_once, slots_init);
    if (!slots_ready) return NULL;
    PlumFileSlot *slot = &slots[(h - 1) % PLUM_MAX_FILES];
    pthread_mutex_lock(&slot->lock);
    if (slot->stream == NULL || slot->identity != h) {
        pthread_mutex_unlock(&slot->lock);
        return NULL;
    }
    return slot;
}

// Publish a closed/failed slot for reuse. Called with its I/O lock held.
static void slot_make_available(PlumFileSlot *slot) {
    pthread_mutex_lock(&slots_allocator);
    slot->reserved = 0;
    pthread_mutex_unlock(&slots_allocator);
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
    pthread_once(&slots_once, slots_init);
    if (!slots_ready) return -1;
    for (int i = 0; i < PLUM_MAX_FILES; i++) {
        PlumFileSlot *slot = &slots[i];
        pthread_mutex_lock(&slots_allocator);
        int available = !slot->reserved;
        if (available) slot->reserved = 1;
        pthread_mutex_unlock(&slots_allocator);
        if (!available) continue;
        pthread_mutex_lock(&slot->lock);
        if (slot->identity > LLONG_MAX - PLUM_MAX_FILES) {
            // Keep an exhausted slot reserved permanently.
            pthread_mutex_unlock(&slot->lock);
            continue;
        }
        FILE *f = plum_path_fopen(path, m);
        long long h = -1;
        if (f != NULL) {
            slot->identity = slot->identity == 0 ? i + 1 : slot->identity + PLUM_MAX_FILES;
            slot->stream = f;
            h = slot->identity;
        } else slot_make_available(slot);
        pthread_mutex_unlock(&slot->lock);
        return h;
    }
    return -1;
}

// 0 on success, -1 on a real error. Closing an already-closed handle is
// SUCCESS, not an error: the guard runs after an explicit close on every
// well-written program, and reporting that as a failure would make the
// safe pattern the noisy one.
long long file_close_checked(long long h) {
    PlumFileSlot *slot = slot_lock(h);
    if (slot == NULL) return 0;
    FILE *f = slot->stream;
    slot->stream = NULL;
    long long result = fclose(f) == 0 ? 0 : -1;
    slot_make_available(slot);
    pthread_mutex_unlock(&slot->lock);
    return result;
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
    while (next < need) {
        if (next > SIZE_MAX / 2) { next = need; break; }
        next *= 2;
    }
    char *bigger = (char *)realloc(read_buf, next);
    if (bigger == NULL) return 0;
    read_buf = bigger;
    read_cap = next;
    return 1;
}

long long file_read_n(long long h, long long max_len) {
    // Only a positive request can report zero bytes as end of file.
    if (max_len <= 0 || (uintmax_t)max_len >= SIZE_MAX) return -1;
    PlumFileSlot *slot = slot_lock(h);
    if (slot == NULL) return -1;
    long long result = -1;
    if (read_ensure((size_t)max_len + 1)) {
        size_t got = fread(read_buf, 1, (size_t)max_len, slot->stream);
        if (got != 0 || !ferror(slot->stream)) {
            read_buf[got] = '\0';
            result = (long long)got;
        }
    }
    pthread_mutex_unlock(&slot->lock);
    return result;
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
    if (len < 0 || (uintmax_t)len > SIZE_MAX) return -1;
    PlumFileSlot *slot = slot_lock(h);
    if (slot == NULL) return -1;
    size_t wrote = fwrite(buf, 1, (size_t)len, slot->stream);
    pthread_mutex_unlock(&slot->lock);
    return (long long)wrote;
}

// The new absolute position, or -1. `whence` is 0 start, 1 current,
// 2 end -- the same order `SEEK_SET`/`SEEK_CUR`/`SEEK_END` are usually
// written in, but passed as Plum's own constants rather than assuming
// the C macros have those values on every platform.
static long long file_seek_locked(FILE *stream, long long offset, long long whence) {
    int w = whence == 0 ? SEEK_SET : whence == 1 ? SEEK_CUR : whence == 2 ? SEEK_END : -1;
    if (w == -1) return -1;
#if defined(_WIN32)
    if (_fseeki64(stream, offset, w) != 0) return -1;
    __int64 position = _ftelli64(stream);
#else
    if (fseeko(stream, (off_t)offset, w) != 0) return -1;
    off_t position = ftello(stream);
#endif
    // Preserve tell failures and reject positions outside Plum Int's range
    // before narrowing, including on platforms with a wider off_t.
    if (position < 0 || (uintmax_t)position > (uintmax_t)LLONG_MAX) return -1;
    return (long long)position;
}

long long file_seek(long long h, long long offset, long long whence) {
    PlumFileSlot *slot = slot_lock(h);
    if (slot == NULL) return -1;
    long long result = file_seek_locked(slot->stream, offset, whence);
    pthread_mutex_unlock(&slot->lock);
    return result;
}

// Flushes buffered writes without closing, so a caller can make data
// visible to another process and keep the handle.
long long file_flush(long long h) {
    PlumFileSlot *slot = slot_lock(h);
    if (slot == NULL) return -1;
    long long result = fflush(slot->stream) == 0 ? 0 : -1;
    pthread_mutex_unlock(&slot->lock);
    return result;
}
