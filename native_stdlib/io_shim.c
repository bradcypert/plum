// Blocking reads from standard input, for the language server.
//
// The runtime can already WRITE (printf) and read whole FILES, but had
// no way to read stdin at all — which is what an LSP server does for a
// living.
//
// **The buffers are owned HERE**, reused across calls and valid until
// the next call to the same function. The obvious alternative — malloc
// and hand ownership to the caller — does not work portably: the real
// compiler materializes an extern's `CStr` RETURN as a Plum string, so
// the pointer cannot be handed back to a `free`-shaped extern, and
// Plum has no `free` of its own. Caller-owned buffers would therefore
// leak one allocation per message in a process designed to run for
// hours.
//
// Not thread-safe, and does not need to be: a language server reads its
// requests on one thread, in order.
//
// Deliberately blocking and synchronous. A language server is a request
// loop: read one message, answer it, read the next. Nothing here needs
// to be asynchronous, and pretending otherwise would mean inventing a
// concurrency story for a problem that does not have one.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static char *line_buf = NULL;
static size_t line_cap = 0;
static char *body_buf = NULL;
static size_t body_cap = 0;

static int ensure(char **buf, size_t *cap, size_t need) {
    if (*cap >= need) return 1;
    size_t next = *cap ? *cap : 256;
    while (next < need) next *= 2;
    char *bigger = (char *)realloc(*buf, next);
    if (!bigger) return 0;
    *buf = bigger;
    *cap = next;
    return 1;
}

// One line, without its newline. Returns "" at EOF — which is how the
// server learns its client has gone away, since it cannot distinguish a
// null pointer from an empty string once the value crosses into Plum.
const char *stdin_read_line(void) {
    size_t len = 0;
    if (!ensure(&line_buf, &line_cap, 256)) return "";
    for (;;) {
        int c = fgetc(stdin);
        if (c == EOF) break;
        if (c == '\n') break;
        if (!ensure(&line_buf, &line_cap, len + 2)) return "";
        line_buf[len++] = (char)c;
    }
    // LSP framing is CRLF; the caller only ever wants the content.
    if (len > 0 && line_buf[len - 1] == '\r') len--;
    line_buf[len] = '\0';
    return line_buf;
}

// Exactly `n` bytes, or "" if the stream ended early. An LSP message
// body is length-prefixed, so a short read is a protocol error rather
// than something to paper over.
const char *stdin_read_n(long long n) {
    if (n < 0) return "";
    if (!ensure(&body_buf, &body_cap, (size_t)n + 1)) return "";
    size_t got = fread(body_buf, 1, (size_t)n, stdin);
    if (got != (size_t)n) return "";
    body_buf[n] = '\0';
    return body_buf;
}

// Writes without a trailing newline, which `println` cannot do and an
// LSP header needs.
void stdout_write(const char *s) { fputs(s, stdout); }

// stdout is a pipe here, not a terminal, so it is block-buffered: a
// response written but not flushed leaves the client waiting forever
// while the server waits for the next request. The one deadlock this
// design can produce, and the one line that prevents it.
void stdout_flush(void) { fflush(stdout); }

// The shadow call stack, for a stack trace on a runtime failure.
//
// Pushed on entry and popped on the way out, by code the compiler emits
// only when `--trace` is passed. Without it nothing calls these, the
// depth stays zero, and `plum_trace` prints nothing.
//
// Why this instead of a real unwinder: `backtrace()` is glibc and
// macOS, `_Unwind_Backtrace` needs a library on musl, and Windows has
// its own API -- three implementations, two of which CI can only
// cross-compile and never run. This is one implementation for every
// target, and it prints the names the compiler already knows rather
// than mangled symbols an unwinder would hand back to be decoded.
//
// It lives in C rather than in emitted IR because the trace goes to
// STDERR, and naming `stderr` portably from LLVM text is not possible:
// it is `stderr` on glibc and musl and `__stderrp` on macOS. Here the C
// compiler knows which. Stderr and not stdout because the panic MESSAGE
// goes to stdout, where `bootstrap/abort_corpus` compares it byte for
// byte; a trace that varies with the build must stay out of that.
//
// --- Where the pop goes, and why it is not in the epilogue ---
//
// These two functions look symmetric, and the PLACEMENT of the pop is
// what made this hard. Popping once before a function's `ret` -- the
// obvious design -- puts a side-effecting call between a tail-recursive
// call and the return of its value, so the optimiser cannot rewrite the
// recursion as a loop. Measured, not reasoned about: three million
// deep, fine without tracing, segfault with.
//
// The compiler instead pays the pop PER PATH, and a self-call in tail
// position pops BEFORE calling (see `cg_expr_t` in
// `bootstrap/self_host/codegen/codegen.plum`). Nothing then sits
// between the call and the `ret`, tail recursion runs in constant stack
// space under `--trace`, and the depth still balances: LLVM's
// tail-recursion elimination hoists only allocas into its new entry
// block, so the push rides inside the loop with the pop.
//
// A consequence visible from in here: a tail-recursive chain leaves ONE
// frame, not one per iteration, because a tail call really does replace
// its caller's frame. Two earlier designs that tried to detect that by
// STACK ADDRESS instead both failed -- an `alloca` handed out escapes
// and blocks the same optimisation, and `__builtin_frame_address(1)`
// does not order by call depth (a four-deep chain reported 2b0, 2a0,
// 2a0, 2b0). DESIGN.md has both in full, under "Stack traces, and
// the thing they cost".
#include <stdio.h>

#define PLUM_TRACE_CAP 256

static const char *plum_frame_names[PLUM_TRACE_CAP];
static long long plum_depth = 0;

void plum_frame_push(const char *name) {
    if (plum_depth >= 0 && plum_depth < PLUM_TRACE_CAP) {
        plum_frame_names[plum_depth] = name;
    }
    // Counted past the end on purpose: a runaway recursion must not
    // overflow this array, and the count is what lets a trace say how
    // many frames it is not showing.
    plum_depth++;
}

void plum_frame_pop(void) {
    if (plum_depth > 0) plum_depth--;
}

// Innermost first, which is the order a reader scans: the frame the
// failure happened in is the one they are looking for.
void plum_trace(void) {
    if (plum_depth <= 0) return;
    long long shown = plum_depth < PLUM_TRACE_CAP ? plum_depth : PLUM_TRACE_CAP;
    fprintf(stderr, "stack trace:\n");
    for (long long i = shown - 1; i >= 0; i--) {
        fprintf(stderr, "  at %s\n", plum_frame_names[i] ? plum_frame_names[i] : "?");
    }
    if (plum_depth > shown) {
        fprintf(stderr, "  ... %lld more frames\n", plum_depth - shown);
    }
}

// --- A public stdin, and stderr (issue #17, timeouts in #7) ---
//
// Added ALONGSIDE `stdin_read_line`/`stdin_read_n` rather than changing
// them: those are called by the language server in this same binary, and
// MAINTENANCE.md's rule is not to change the arity of a shim the
// compiler still calls.
//
// The reason for new ones is a contract the old pair cannot express.
// `stdin_read_line` returns "" at end of stream AND for an empty line,
// so a filter reading until EOF cannot tell "the input ended" from "the
// input contained a blank line" -- and a program that stops at the first
// blank line is wrong in a way that only shows up on real data.
// `stdin_read_n` has the mirror problem: it returns "" on a short read,
// discarding however many bytes it did get.
//
// Both new functions return a COUNT and leave the bytes in a buffer the
// caller reads separately, the same split `tcp_recv_n`/`tcp_recv_data`
// and `file_read_n`/`file_read_data` use, and for the same reason: a
// `CStr` return cannot carry a length.
//
// --- ONE reader, and why stdio could not stay ---
//
// These four functions and their timed counterparts share a single
// buffer, filled by raw `read`/`ReadFile`. They used to use `fgetc` and
// `fread`, and that had to go the moment a TIMED read existed:
//
// `poll` asks the KERNEL whether bytes are available. It knows nothing
// about bytes stdio has already pulled into its own buffer. A timed read
// layered over stdio therefore reports "nothing there, timed out" while
// a complete line sits in the stdio buffer -- not a Windows quirk, a
// silent wrong answer everywhere. Two readers over one file descriptor
// cannot both be right, so there is one.
//
// The language server's own `stdin_read_line`/`stdin_read_n` above still
// use stdio. They are a different consumer in the same binary and never
// mix with these, because the language server does not do timed reads --
// but MIXING THEM WOULD LOSE BYTES, and that is the reason to leave them
// alone rather than the reason not to worry.
//
// --- What a timeout bounds ---
//
// The WHOLE CALL, not just the wait for the first byte. `poll` returning
// readable means A byte is available, not a line, so a slow writer can
// leave a reader blocked mid-line long past its deadline -- which would
// pass every test written against a terminal, where a line arrives all
// at once, and fail against a pipe.
//
// That is only safe because a partial line SURVIVES the timeout. Bytes
// already read stay in the buffer and the next call continues where this
// one stopped, so a timeout is never data loss. A design that discarded
// them would be worse than the blocking one it replaced.

#include <stdlib.h>
#include <string.h>

#if defined(_WIN32)
#include <windows.h>
#else
#include <poll.h>
#include <unistd.h>
#include <errno.h>
#include <time.h>
#endif

#define PLUM_STDIN_TIMED_OUT (-2)

static unsigned char *plum_in_buf = NULL;   // unconsumed bytes
static size_t plum_in_cap = 0;
static size_t plum_in_len = 0;
static int plum_in_eof = 0;

static char *plum_out_buf = NULL;           // the line/bytes just handed out
static size_t plum_out_cap = 0;
static long long plum_out_len = 0;

static int plum_grow(unsigned char **buf, size_t *cap, size_t need) {
    if (*cap >= need) return 1;
    size_t next = *cap ? *cap : 1024;
    while (next < need) next *= 2;
    unsigned char *bigger = (unsigned char *)realloc(*buf, next);
    if (!bigger) return 0;
    *buf = bigger;
    *cap = next;
    return 1;
}

// Hands `n` bytes from the front of the buffer to the caller, and drops
// them from it. The copy is what lets the returned pointer stay valid
// while the buffer keeps being refilled.
static long long plum_in_take(size_t n, size_t drop) {
    if (!plum_grow((unsigned char **)&plum_out_buf, &plum_out_cap, n + 1)) return -1;
    memcpy(plum_out_buf, plum_in_buf, n);
    plum_out_buf[n] = '\0';
    memmove(plum_in_buf, plum_in_buf + drop, plum_in_len - drop);
    plum_in_len -= drop;
    plum_out_len = (long long)n;
    return (long long)n;
}

// Milliseconds left until `deadline_ns`, or -1 for "no deadline".
// `now_ns` is only read when there is a deadline to compare against.
#if !defined(_WIN32)
static long long plum_now_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) return 0;
    return (long long)ts.tv_sec * 1000000000LL + (long long)ts.tv_nsec;
}
#else
static long long plum_now_ns(void) {
    LARGE_INTEGER freq, ticks;
    if (!QueryPerformanceFrequency(&freq) || freq.QuadPart == 0) return 0;
    QueryPerformanceCounter(&ticks);
    long long secs = (long long)(ticks.QuadPart / freq.QuadPart);
    long long rem = (long long)(ticks.QuadPart % freq.QuadPart);
    return secs * 1000000000LL + (rem * 1000000000LL) / (long long)freq.QuadPart;
}
#endif

// Reads once into the buffer, waiting no longer than `deadline_ns`
// (negative means wait as long as it takes). Returns 1 if bytes were
// added, 0 on end of stream, PLUM_STDIN_TIMED_OUT if the deadline
// passed first, -1 on error.
static int plum_in_fill(long long deadline_ns) {
    if (plum_in_eof) return 0;
    if (!plum_grow(&plum_in_buf, &plum_in_cap, plum_in_len + 4096)) return -1;

#if defined(_WIN32)
    HANDLE h = GetStdHandle(STD_INPUT_HANDLE);
    if (h == INVALID_HANDLE_VALUE || h == NULL) return -1;
    DWORD type = GetFileType(h);
    for (;;) {
        // A file redirected into stdin is always ready; there is nothing
        // to wait for and no way to block.
        if (type == FILE_TYPE_DISK) break;
        if (type == FILE_TYPE_PIPE) {
            DWORD avail = 0;
            if (!PeekNamedPipe(h, NULL, 0, NULL, &avail, NULL)) {
                // A closed pipe peeks as an error, which is end of
                // stream rather than a failure.
                plum_in_eof = 1;
                return 0;
            }
            if (avail > 0) break;
        } else {
            // Console. `WaitForSingleObject` signals for any input
            // record -- a key release, a mouse move, a focus change --
            // so a wake is not proof that a byte is readable, and the
            // loop re-checks rather than trusting it.
            DWORD w = WaitForSingleObject(h, 0);
            if (w == WAIT_OBJECT_0) break;
        }
        if (deadline_ns >= 0 && plum_now_ns() >= deadline_ns) return PLUM_STDIN_TIMED_OUT;
        Sleep(5);
    }
    DWORD got = 0;
    if (!ReadFile(h, plum_in_buf + plum_in_len, 4096, &got, NULL) || got == 0) {
        plum_in_eof = 1;
        return 0;
    }
    plum_in_len += (size_t)got;
    return 1;
#else
    for (;;) {
        struct pollfd pfd;
        pfd.fd = 0;
        pfd.events = POLLIN;
        pfd.revents = 0;
        int ms = -1;
        if (deadline_ns >= 0) {
            long long left = deadline_ns - plum_now_ns();
            if (left <= 0) return PLUM_STDIN_TIMED_OUT;
            long long lms = left / 1000000LL;
            if (left % 1000000LL != 0) lms++;
            ms = lms > 2147483647LL ? 2147483647 : (int)lms;
        }
        int r = poll(&pfd, 1, ms);
        if (r < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        if (r == 0) return PLUM_STDIN_TIMED_OUT;
        break;
    }
    ssize_t got = read(0, plum_in_buf + plum_in_len, 4096);
    if (got < 0) {
        if (errno == EINTR) return 1; // nothing added; the caller loops
        return -1;
    }
    if (got == 0) {
        plum_in_eof = 1;
        return 0;
    }
    plum_in_len += (size_t)got;
    return 1;
#endif
}

// The index of the first newline in the buffer, or -1.
static long long plum_in_find_nl(void) {
    for (size_t i = 0; i < plum_in_len; i++) {
        if (plum_in_buf[i] == '\n') return (long long)i;
    }
    return -1;
}

// Shared by the timed and untimed line reads. `deadline_ns` negative
// means block.
static long long plum_line_until(long long deadline_ns) {
    for (;;) {
        long long nl = plum_in_find_nl();
        if (nl >= 0) {
            size_t len = (size_t)nl;
            // LSP framing is CRLF; a caller only ever wants the content.
            if (len > 0 && plum_in_buf[len - 1] == '\r') len--;
            return plum_in_take(len, (size_t)nl + 1);
        }
        if (plum_in_eof) {
            // End of stream with bytes still held is a final line
            // without a trailing newline, which is an ordinary line and
            // must not be thrown away.
            if (plum_in_len > 0) {
                size_t len = plum_in_len;
                if (len > 0 && plum_in_buf[len - 1] == '\r') len--;
                return plum_in_take(len, plum_in_len);
            }
            return -1;
        }
        int r = plum_in_fill(deadline_ns);
        if (r == PLUM_STDIN_TIMED_OUT) return PLUM_STDIN_TIMED_OUT;
        if (r < 0) return -1;
        // r == 0 sets `plum_in_eof`; the next turn of the loop handles it.
    }
}

static long long plum_bytes_until(long long max, long long deadline_ns) {
    if (max < 0) max = 0;
    while (plum_in_len == 0 && !plum_in_eof) {
        int r = plum_in_fill(deadline_ns);
        if (r == PLUM_STDIN_TIMED_OUT) return PLUM_STDIN_TIMED_OUT;
        if (r < 0) return -1;
    }
    // End of stream with nothing buffered is -1 here, so the TIMED
    // caller can report it as a distinct outcome. `stdin_bytes_n` maps
    // it back to 0, which is the contract its callers already have.
    if (plum_in_eof && plum_in_len == 0) return -1;
    size_t n = plum_in_len < (size_t)max ? plum_in_len : (size_t)max;
    return plum_in_take(n, n);
}

// The line just read, without its newline. Returns the byte count, or
// -1 at end of stream -- which is the distinction the whole pair exists
// for. An empty line is 0, and that is not the same answer.
long long stdin_line_n(void) { return plum_line_until(-1); }

const char *stdin_line_data(void) { return plum_out_buf ? plum_out_buf : ""; }

// Up to `max` bytes. Returns how many were actually read: 0 at end of
// stream, and a SHORT COUNT is data, not a failure -- a pipe hands over
// what it has.
long long stdin_bytes_n(long long max) {
    long long r = plum_bytes_until(max, -1);
    if (r == PLUM_STDIN_TIMED_OUT) return 0;
    return r < 0 ? 0 : r;
}

const char *stdin_bytes_data(void) { return plum_out_buf ? plum_out_buf : ""; }

// The timed counterparts. `-2` is "the deadline passed", which is a
// third outcome neither of the two above has room to report -- they
// spend their sentinel on end of stream.
//
// A timeout leaves everything already read IN THE BUFFER, so the next
// call resumes mid-line rather than starting over. That is what makes
// bounding the whole call safe.
long long stdin_line_timed(long long timeout_nanos) {
    return plum_line_until(timeout_nanos < 0 ? -1 : plum_now_ns() + timeout_nanos);
}

long long stdin_bytes_timed(long long max, long long timeout_nanos) {
    return plum_bytes_until(max, timeout_nanos < 0 ? -1 : plum_now_ns() + timeout_nanos);
}

// Unbuffered by convention: stderr is where a program says something is
// wrong, and a message lost in a buffer at exit is the one that mattered.
void stderr_write(const char *s) {
    fputs(s, stderr);
    fflush(stderr);
}
