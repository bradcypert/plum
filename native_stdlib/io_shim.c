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
#include <limits.h>
#include <stdint.h>
#include <stdatomic.h>

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

#if defined(_MSC_VER)
#define PLUM_TRACE_TLS __declspec(thread)
#else
#define PLUM_TRACE_TLS _Thread_local
#endif

// Tasks run on separate OS threads. Both the slots and the depth belong
// to that thread, including when its panic calls plum_trace. A lock on
// individual pushes/pops would still mix unrelated call stacks.
// Fixed TLS storage needs no heap allocation or thread-exit destructor;
// the slots borrow compiler-emitted static function-name strings.
static PLUM_TRACE_TLS const char *plum_frame_names[PLUM_TRACE_CAP];
static PLUM_TRACE_TLS long long plum_depth = 0;

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
// The public wrappers use an owned native result, carrying a byte count
// and data until explicitly released after the copy into Plum. CStr alone
// cannot express a length or ownership. The old count/data symbols below
// remain only to link older compiler seeds.
//
// --- ONE reader, and why stdio could not stay ---
//
// All public reads share one serialized reader and its pending buffer,
// filled by raw `read`/`ReadFile`. They used to use `fgetc` and
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

// One process-owned reader serializes consumption, including pending bytes.
// Results have independent ownership: unlocking the reader never exposes a
// buffer that another task can realloc, shift, or overwrite (#271).
typedef struct {
    size_t len;
    unsigned char bytes[];
} plum_stdin_result;

#if defined(_WIN32)
typedef struct {
    HANDLE request, done, thread, input;
    volatile LONG stopping;
    DWORD count, error;
    int pending;
    unsigned char bytes[4096];
} plum_console_reader;
#endif

typedef struct {
    atomic_flag lock;
    atomic_int closing;
    unsigned char *bytes;
    size_t len, cap;
    int eof;
#if defined(_WIN32)
    plum_console_reader console;
#endif
} plum_stdin_reader;

static plum_stdin_reader plum_stdin = { .lock = ATOMIC_FLAG_INIT };

#define PLUM_STDIN_EOF (-1)
#define PLUM_STDIN_ERROR (-3)

static int plum_grow(unsigned char **buf, size_t *cap, size_t need) {
    if (*cap >= need) return 1;
    size_t next = *cap ? *cap : 1024;
    while (next < need) {
        if (next > SIZE_MAX / 2) { next = need; break; }
        next *= 2;
    }
    unsigned char *bigger = (unsigned char *)realloc(*buf, next);
    if (!bigger) return 0;
    *buf = bigger;
    *cap = next;
    return 1;
}

// Snapshot `n` bytes before consuming them. Allocation failure leaves the
// pending bytes untouched; a successful result outlives the reader lock.
static long long plum_in_take(plum_stdin_reader *reader, size_t n, size_t drop) {
    if (n > SIZE_MAX - sizeof(plum_stdin_result) - 1) return PLUM_STDIN_ERROR;
    plum_stdin_result *result = malloc(sizeof(*result) + n + 1);
    if (!result) return PLUM_STDIN_ERROR;
    result->len = n;
    memcpy(result->bytes, reader->bytes, n);
    result->bytes[n] = '\0';
    memmove(reader->bytes, reader->bytes + drop, reader->len - drop);
    reader->len -= drop;
    return (long long)(intptr_t)result;
}

// Monotonic time for both lock acquisition and adapter waits.
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

// Only negative durations mean "forever". Saturate large finite waits
// instead of overflowing into that sentinel (issue #224).
static long long plum_stdin_deadline(long long timeout_nanos) {
    if (timeout_nanos < 0) return -1;
    long long now = plum_now_ns();
    return timeout_nanos > LLONG_MAX - now ? LLONG_MAX : now + timeout_nanos;
}

#if defined(_WIN32)
// Keep the OS's stream semantics (line editing, echo, code page, VT input).
// Console readiness only promises an INPUT_RECORD, so neither ReadFile nor
// ReadConsole is safe on the calling thread after a wait (#225).
// One lazily started worker owns at most ONE outstanding read, even across
// thousands of timeouts. It never touches the reader's resizable buffer.
static DWORD WINAPI plum_console_worker(void *raw) {
    plum_console_reader *console = raw;
    for (;;) {
        if (WaitForSingleObject(console->request, INFINITE) != WAIT_OBJECT_0) return 1;
        if (InterlockedCompareExchange(&console->stopping, 0, 0)) return 0;
        console->count = 0;
        console->error = ReadFile(console->input, console->bytes,
                                  sizeof(console->bytes), &console->count, NULL)
                             ? ERROR_SUCCESS : GetLastError();
        SetEvent(console->done);
    }
}

static void plum_console_close(plum_console_reader *console) {
    if (console->thread) {
        InterlockedExchange(&console->stopping, 1);
        // Cancellation can race the start of ReadFile. Keep cancelling
        // until the worker observes stop; never free live worker storage.
        do {
            SetEvent(console->request);
            CancelSynchronousIo(console->thread);
        } while (WaitForSingleObject(console->thread, 1) == WAIT_TIMEOUT);
        CloseHandle(console->thread);
    }
    if (console->request) CloseHandle(console->request);
    if (console->done) CloseHandle(console->done);
    memset(console, 0, sizeof(*console));
}

static void plum_stdin_console_close(void) {
    // exit() can run on a different task while a reader is waiting.
    // Stop admission and wake that caller before taking its lock. Events
    // and worker storage must stay alive until the caller has left them.
    atomic_store_explicit(&plum_stdin.closing, 1, memory_order_release);
    plum_console_reader *console = &plum_stdin.console;
    while (atomic_flag_test_and_set_explicit(&plum_stdin.lock, memory_order_acquire)) {
        if (console->thread) {
            InterlockedExchange(&console->stopping, 1);
            SetEvent(console->request);
            CancelSynchronousIo(console->thread);
            SetEvent(console->done);
        }
        Sleep(1);
    }
    plum_console_close(console);
    atomic_flag_clear_explicit(&plum_stdin.lock, memory_order_release);
}

static int plum_console_start(plum_console_reader *console) {
    console->request = CreateEventW(NULL, FALSE, FALSE, NULL);
    console->done = CreateEventW(NULL, TRUE, FALSE, NULL);
    if (!console->request || !console->done) goto failed;
    // Bound the reserved stack rather than taking the default 1 MiB.
    console->thread = CreateThread(NULL, 64 * 1024, plum_console_worker, console,
                                  STACK_SIZE_PARAM_IS_A_RESERVATION, NULL);
    if (!console->thread) goto failed;
    return 1;
failed:
    plum_console_close(console);
    return 0;
}

// A long finite wait must not narrow into INFINITE (0xffffffff).
static DWORD plum_console_wait_ms(long long deadline_ns) {
    if (deadline_ns < 0) return INFINITE;
    long long left = deadline_ns - plum_now_ns();
    if (left <= 0) return 0;
    unsigned long long ms = (unsigned long long)(left / 1000000LL) +
                            (left % 1000000LL != 0);
    return ms > 0xfffffffeULL ? 0xfffffffeUL : (DWORD)ms;
}

static int plum_console_fill(plum_stdin_reader *reader, HANDLE input,
                             long long deadline_ns) {
    plum_console_reader *console = &reader->console;
    if (!console->pending) {
        if (deadline_ns >= 0 && plum_now_ns() >= deadline_ns) return PLUM_STDIN_TIMED_OUT;
        if (!console->thread) {
            if (!plum_console_start(console)) return PLUM_STDIN_ERROR;
            if (reader == &plum_stdin && atexit(plum_stdin_console_close) != 0) {
                plum_console_close(console);
                return PLUM_STDIN_ERROR;
            }
        }
        console->input = input;
        if (!ResetEvent(console->done)) return PLUM_STDIN_ERROR;
        if (!SetEvent(console->request)) return PLUM_STDIN_ERROR;
        console->pending = 1;
    }
    for (;;) {
        DWORD wait = WaitForSingleObject(console->done, plum_console_wait_ms(deadline_ns));
        if (wait == WAIT_OBJECT_0) break;
        if (wait != WAIT_TIMEOUT) return PLUM_STDIN_ERROR;
        if (deadline_ns >= 0 && plum_now_ns() >= deadline_ns) return PLUM_STDIN_TIMED_OUT;
    }
    if (atomic_load_explicit(&reader->closing, memory_order_acquire)) return PLUM_STDIN_ERROR;
    console->pending = 0;
    if (console->error != ERROR_SUCCESS) return PLUM_STDIN_ERROR;
    if (console->count == 0) { reader->eof = 1; return 0; }
    memcpy(reader->bytes + reader->len, console->bytes, console->count);
    reader->len += console->count;
    return 1;
}
#endif

static void plum_stdin_pause(void) {
#if defined(_WIN32)
    Sleep(1);
#else
    struct timespec pause = {0, 1000000};
    nanosleep(&pause, NULL);
#endif
}

// Lock acquisition spends the SAME deadline as reading. A task holding a
// blocking line read cannot make another task's finite read wait forever.
static int plum_stdin_lock(plum_stdin_reader *reader, long long deadline_ns) {
    while (atomic_flag_test_and_set_explicit(&reader->lock, memory_order_acquire)) {
        if (deadline_ns >= 0 && plum_now_ns() >= deadline_ns) return 0;
        plum_stdin_pause();
    }
    return 1;
}

// Reads once into the buffer, waiting no longer than `deadline_ns`
// (negative means wait as long as it takes). Returns 1 if bytes were
// added, 0 on end of stream, PLUM_STDIN_TIMED_OUT if the deadline
// passed first, PLUM_STDIN_ERROR on error.
static int plum_in_fill(plum_stdin_reader *reader, long long deadline_ns) {
    if (reader->eof) return 0;
    if (reader->len > SIZE_MAX - 4096 ||
        !plum_grow(&reader->bytes, &reader->cap, reader->len + 4096)) return PLUM_STDIN_ERROR;

#if defined(_WIN32)
    HANDLE h = GetStdHandle(STD_INPUT_HANDLE);
    if (h == INVALID_HANDLE_VALUE || h == NULL) return PLUM_STDIN_ERROR;
    DWORD type = GetFileType(h);
    DWORD mode;
    if (GetConsoleMode(h, &mode)) return plum_console_fill(reader, h, deadline_ns);
    if (type != FILE_TYPE_DISK && type != FILE_TYPE_PIPE) return PLUM_STDIN_ERROR;
    for (;;) {
        // A file redirected into stdin is always ready; there is nothing
        // to wait for and no way to block.
        if (type == FILE_TYPE_DISK) break;
        if (type == FILE_TYPE_PIPE) {
            DWORD avail = 0;
            if (!PeekNamedPipe(h, NULL, 0, NULL, &avail, NULL)) {
                if (GetLastError() == ERROR_BROKEN_PIPE) { reader->eof = 1; return 0; }
                return PLUM_STDIN_ERROR;
            }
            if (avail > 0) break;
        }
        if (deadline_ns >= 0 && plum_now_ns() >= deadline_ns) return PLUM_STDIN_TIMED_OUT;
        Sleep(5);
    }
    DWORD got = 0;
    if (!ReadFile(h, reader->bytes + reader->len, 4096, &got, NULL)) {
        DWORD error = GetLastError();
        if (error == ERROR_BROKEN_PIPE || error == ERROR_HANDLE_EOF) {
            reader->eof = 1; return 0;
        }
        return PLUM_STDIN_ERROR;
    }
    if (got == 0) { reader->eof = 1; return 0; }
    reader->len += (size_t)got;
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
            return PLUM_STDIN_ERROR;
        }
        if (r == 0) return PLUM_STDIN_TIMED_OUT;
        break;
    }
    ssize_t got = read(0, reader->bytes + reader->len, 4096);
    if (got < 0) {
        if (errno == EINTR) return 1; // nothing added; the caller loops
        return PLUM_STDIN_ERROR;
    }
    if (got == 0) {
        reader->eof = 1;
        return 0;
    }
    reader->len += (size_t)got;
    return 1;
#endif
}

// The index of the first newline in the buffer, or -1.
static long long plum_in_find_nl(plum_stdin_reader *reader) {
    for (size_t i = 0; i < reader->len; i++) {
        if (reader->bytes[i] == '\n') return (long long)i;
    }
    return -1;
}

// Shared by the timed and untimed line reads. `deadline_ns` negative
// means block.
static long long plum_line_until(plum_stdin_reader *reader, long long deadline_ns) {
    for (;;) {
        long long nl = plum_in_find_nl(reader);
        if (nl >= 0) {
            size_t len = (size_t)nl;
            // LSP framing is CRLF; a caller only ever wants the content.
            if (len > 0 && reader->bytes[len - 1] == '\r') len--;
            return plum_in_take(reader, len, (size_t)nl + 1);
        }
        if (reader->eof) {
            // End of stream with bytes still held is a final line
            // without a trailing newline, which is an ordinary line and
            // must not be thrown away.
            if (reader->len > 0) {
                size_t len = reader->len;
                if (len > 0 && reader->bytes[len - 1] == '\r') len--;
                return plum_in_take(reader, len, reader->len);
            }
            return -1;
        }
        int r = plum_in_fill(reader, deadline_ns);
        if (r < 0) return r;
        // r == 0 sets EOF; the next turn of the loop handles it.
    }
}

static long long plum_bytes_until(plum_stdin_reader *reader, long long max, long long deadline_ns) {
    // A nonpositive request is an empty result even at EOF. Do not fill
    // or take: either would allocate, and filling could wait or consume
    // input. Existing owned results and pending input remain untouched.
    if (max <= 0) {
        return 0;
    }
    while (reader->len == 0 && !reader->eof) {
        int r = plum_in_fill(reader, deadline_ns);
        if (r < 0) return r;
    }
    // End of stream with nothing buffered is -1 here, so the TIMED
    // caller can report it as a distinct outcome. The blocking public
    // wrapper maps EOF to empty bytes for compatibility.
    if (reader->eof && reader->len == 0) return PLUM_STDIN_EOF;
    size_t n = reader->len < (unsigned long long)max ? reader->len : (size_t)max;
    return plum_in_take(reader, n, n);
}

// Owned result protocol: positive = result handle, 0 = empty byte request,
// -1 = EOF, -2 = timeout, -3 = I/O/allocation failure. A handle owns its
// bytes until release, independent of later reads on any task. Plum wraps
// it in a private handle before copying, including UTF-8 validation errors.
long long stdin_read_owned(long long max, long long timeout_nanos, long long line) {
    if (!line && max <= 0) return 0;
    if (atomic_load_explicit(&plum_stdin.closing, memory_order_acquire)) return PLUM_STDIN_ERROR;
    long long deadline = plum_stdin_deadline(timeout_nanos);
    if (!plum_stdin_lock(&plum_stdin, deadline)) return PLUM_STDIN_TIMED_OUT;
    long long result;
    if (atomic_load_explicit(&plum_stdin.closing, memory_order_acquire)) result = PLUM_STDIN_ERROR;
    else result = line ? plum_line_until(&plum_stdin, deadline) :
                         plum_bytes_until(&plum_stdin, max, deadline);
    atomic_flag_clear_explicit(&plum_stdin.lock, memory_order_release);
    return result;
}

long long stdin_result_len(long long handle) {
    return handle > 0 ? (long long)((plum_stdin_result *)(intptr_t)handle)->len : 0;
}

const char *stdin_result_data(long long handle) {
    return handle > 0 ? (const char *)((plum_stdin_result *)(intptr_t)handle)->bytes : "";
}

void stdin_result_release(long long handle) {
    if (handle > 0) free((void *)(intptr_t)handle);
}

// Bootstrap ABI only: old seeds still reference these symbols. Their
// single-consumer borrowed-result contract cannot express ownership. The
// current public Os wrappers use the owned protocol above instead.
static long long plum_legacy_result;
static long long plum_stdin_legacy(long long max, long long timeout, long long line) {
    stdin_result_release(plum_legacy_result);
    plum_legacy_result = stdin_read_owned(max, timeout, line);
    return plum_legacy_result < 0 ? plum_legacy_result : stdin_result_len(plum_legacy_result);
}
long long stdin_line_n(void) { return plum_stdin_legacy(0, -1, 1); }
long long stdin_line_timed(long long timeout) { return plum_stdin_legacy(0, timeout, 1); }
long long stdin_bytes_n(long long max) {
    long long n = plum_stdin_legacy(max, -1, 0);
    return n < 0 ? 0 : n;
}
long long stdin_bytes_timed(long long max, long long timeout) {
    return plum_stdin_legacy(max, timeout, 0);
}
const char *stdin_line_data(void) { return stdin_result_data(plum_legacy_result); }
const char *stdin_bytes_data(void) { return stdin_result_data(plum_legacy_result); }

// Unbuffered by convention: stderr is where a program says something is
// wrong, and a message lost in a buffer at exit is the one that mattered.
void stderr_write(const char *s) {
    fputs(s, stderr);
    fflush(stderr);
}
