// A thin ABI-adapter shim between Plum's `extern "C"` surface (Int/
// Float/Bool/CStr/qualifying-struct only — no raw pointers, no
// out-parameters) and POSIX BSD sockets, whose real API needs
// `struct sockaddr`/`struct addrinfo`/`socklen_t *` — none of which fit
// Plum's extern type surface, same reason `raylib_shim.c` exists (see
// examples/asteroids/native/raylib_shim.c's own doc comment for the
// general pattern). Every function here takes/returns only `long long`
// (Plum `Int`) or `const char *` (Plum `CStr`), building the real
// `sockaddr`/`addrinfo` structs internally before calling through.
//
// **Unix-only (Linux/macOS) — a deliberate, documented v1 scope
// boundary, not an oversight.** Windows' Winsock is a genuinely
// different API in the details that matter here (needs `WSAStartup`,
// `SOCKET` isn't `int`, `closesocket()` not `close()`) — see DESIGN.md's
// "TCP sockets" section for the full reasoning, which mirrors the
// existing extern-symbol-resolution Unix-only precedent exactly.
//
// This shim is compiled into TWO separate places, unlike a per-project
// shim (raylib's): once into `plum-interp`/`plumc`'s OWN process (via
// their `build.rs`, alongside how `libm` is force-linked in — see
// those files' doc comments) so `plum run`'s extern-call resolution
// against the CURRENT PROCESS's symbol table finds these symbols too;
// and once into every `plumc build` output binary unconditionally
// (`clang_compile` in `codegen_cli.rs`), the same way `-lm` is passed
// unconditionally — both backends need to agree these functions exist,
// same "both backends behave identically" story as everything else.
//
// **`tcp_recv`'s own real scope trade**: it returns a `CStr` (a
// NUL-terminated, shim-owned buffer) rather than an `Int` count, so
// received data can become a usable Plum `String` via `.as_string()`
// (see DESIGN.md's "CStr -> String" note — `Type::CStr` otherwise has
// no way back into a real Plum value). That makes this NUL-terminated,
// i.e. NOT binary-safe: an embedded `\0` byte in a response silently
// truncates it. Acceptable, honestly-documented for a v1 scoped at
// line-oriented HTTP/1.1 (headers + mostly-text bodies), not something
// that would hold up for arbitrary binary payloads. `tcp_recv` also
// deliberately returns `""` — an empty, non-null `CStr` — on BOTH a
// clean peer-close (`recv` returns 0) AND a hard socket error (`recv`
// returns < 0): a null `CStr` return is a hard runtime abort under
// Plum's existing FFI semantics (see DESIGN.md's `CStr` null-return
// note), which would crash the whole program on an ordinary connection
// close — not acceptable for a `Result`-shaped Net API where "stop
// reading" (from either cause) should be an ordinary, catchable
// outcome, not a language-level abort. The real distinction (clean
// close vs. genuine error) is discarded — a real, documented v1
// limitation, not silently swept.
//
// **`tcp_recv_n`/`tcp_recv_data` are the binary-safe pair**, added
// alongside rather than by changing `tcp_recv`, whose contract callers
// depend on. They split the answer across two calls because Plum's
// extern surface has no out-parameter and no multi-value return: the
// first returns a COUNT (and tells a clean close from an error, which
// `tcp_recv` cannot), the second hands back the buffer those bytes are
// in. Together they carry a length, so nothing has to `strlen` a
// payload that may legitimately contain a zero byte.
#include <string.h>
#include <stdlib.h>
#include <stdio.h>

// Winsock is a different API in the details, not in the shape: the same
// `socket`/`bind`/`listen`/`accept`/`send`/`recv` calls with a
// different handle type, a different close, a different error
// sentinel, and a mandatory one-time initialisation. Those four
// differences are abstracted here and NOTHING below is written twice --
// the alternative, a whole second copy of each function, is how the two
// halves drift apart.
//
// **Untested.** No Windows toolchain has compiled this. See PORTING.md.
#if defined(_WIN32)
// winsock2.h must precede windows.h, which ws2tcpip.h pulls in.
#include <winsock2.h>
#include <ws2tcpip.h>

typedef SOCKET plum_sock;
#define PLUM_BAD_SOCK       INVALID_SOCKET
#define PLUM_CLOSESOCK(s)   closesocket(s)
#define PLUM_SOCKOPT(v)     ((const char *)(v))
#define PLUM_ADDRLEN(n)     ((int)(n))

// Winsock refuses every call until the library is initialised, and
// there is no natural place to do it once -- this shim has no
// initialiser and Plum has no module-load hook. Doing it lazily on each
// entry point is the standard answer; `WSAStartup` is reference-counted
// and cheap to call repeatedly, and the matching `WSACleanup` is
// deliberately never called, because the only correct time to do it is
// at process exit, when it buys nothing.
static int plum_net_init(void) {
    static int started = 0;
    WSADATA wsa;
    if (started) return 0;
    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) return -1;
    started = 1;
    return 0;
}
#else
#include <sys/types.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <netdb.h>
#include <arpa/inet.h>
#include <unistd.h>

typedef int plum_sock;
#define PLUM_BAD_SOCK       (-1)
#define PLUM_CLOSESOCK(s)   close(s)
#define PLUM_SOCKOPT(v)     (v)
#define PLUM_ADDRLEN(n)     (n)
#define plum_net_init()     0
#endif

// `INVALID_SOCKET` is `(SOCKET)~0`, which reinterpreted as a signed
// 64-bit value is exactly -1 -- so the -1 the Plum wrappers already
// treat as failure keeps working on both platforms without a second
// sentinel. Checked here rather than assumed.
static long long plum_sock_out(plum_sock s) {
    return (s == PLUM_BAD_SOCK) ? -1 : (long long)s;
}

// Resolves `host`/`port` via `getaddrinfo` (so both hostnames and
// literal IPs work, IPv4 or IPv6) and connects to the first address
// that succeeds. Returns the connected fd, or -1 on any failure —
// callers (the `Net`/`tcp_connect_to` Plum wrapper) turn that sentinel
// into a `Result::Err`, no extern-level error detail is lost here since
// there IS no richer detail to lose (a DNS failure and a refused
// connection are indistinguishable at this level either way).
long long tcp_connect(const char *host, long long port) {
    struct addrinfo hints;
    if (plum_net_init() != 0) return -1;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;

    char port_str[16];
    snprintf(port_str, sizeof(port_str), "%lld", port);

    struct addrinfo *res = NULL;
    if (getaddrinfo(host, port_str, &hints, &res) != 0) {
        return -1;
    }

    long long fd = -1;
    for (struct addrinfo *rp = res; rp != NULL; rp = rp->ai_next) {
        plum_sock s = socket(rp->ai_family, rp->ai_socktype, rp->ai_protocol);
        if (s == PLUM_BAD_SOCK) {
            continue;
        }
        if (connect(s, rp->ai_addr, PLUM_ADDRLEN(rp->ai_addrlen)) == 0) {
            fd = (long long)s;
            break;
        }
        PLUM_CLOSESOCK(s);
    }
    freeaddrinfo(res);
    return fd;
}

// Binds+listens on `port` across all local interfaces (`INADDR_ANY`,
// IPv4 only — a real, deliberate v1 scope trade, matches `tcp_connect`
// not being IPv6-exclusive-only either way since `getaddrinfo` there
// picks whatever resolves). `SO_REUSEADDR` is set so restarting a
// server immediately after it exits doesn't hit `EADDRINUSE` from the
// kernel's own `TIME_WAIT` hold on the port — standard practice for any
// long-lived listener. Returns the listening fd, or -1 on failure.
long long tcp_listen(long long port) {
    plum_sock s;
    if (plum_net_init() != 0) return -1;
    s = socket(AF_INET, SOCK_STREAM, 0);
    if (s == PLUM_BAD_SOCK) {
        return -1;
    }
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, PLUM_SOCKOPT(&one), sizeof(one));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = INADDR_ANY;
    addr.sin_port = htons((unsigned short)port);

    if (bind(s, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        PLUM_CLOSESOCK(s);
        return -1;
    }
    if (listen(s, 128) != 0) {
        PLUM_CLOSESOCK(s);
        return -1;
    }
    return (long long)s;
}

// `accept(2)`, discarding the peer's own address (Plum has no way to
// receive it back — same "no multi-value FFI return" limit that ruled
// UDP's `recvfrom` out of this v1 pass entirely, see DESIGN.md). Blocks
// until a connection arrives. Returns the new connection's fd, or -1.
long long tcp_accept(long long fd) {
    return plum_sock_out(accept((plum_sock)fd, NULL, NULL));
}

// Blocking send of exactly `len` bytes of `buf`. Returns the number of
// bytes actually sent, or -1 on error — a short write (fewer bytes sent
// than `len`, without an outright error) is possible under real POSIX
// semantics and is NOT retried here; the Plum-level wrapper is
// responsible for looping if it cares (matches this shim's own
// "thin adapter, no retry policy of its own" scope elsewhere).
long long tcp_send(long long fd, const char *buf, long long len) {
    // Winsock's length is `int`, POSIX's is `size_t`; the macro keeps
    // the one narrowing cast in a single place.
    return (long long)send((plum_sock)fd, buf, PLUM_ADDRLEN(len), 0);
}

// --- The receive buffer ---
//
// **Owned HERE**, grown on demand and reused across calls, valid until
// the next call to either receive function. Same convention as
// `io_shim.c`, and adopted for the same reason its header gives: the
// extern boundary hands Plum no way to free a caller-owned buffer, so
// `malloc`-and-hand-over is a leak of one buffer per read.
//
// `tcp_recv` DID that -- it `malloc`ed a fresh buffer per call and
// returned it, and nothing ever freed one. A server looping on reads
// leaked its whole traffic volume. Nothing caught it because
// `net-smoke` is the only harness that runs socket code and it does not
// run under ASan. Both receive functions share this buffer now, which
// removes the leak as a side effect of needing the buffer anyway.
//
// The contract change for `tcp_recv` is real but narrow: its result is
// now only valid until the next receive on any socket. Its one caller,
// `Net.read`, does `.as_string()` immediately -- which COPIES, because
// whatever C handed back may be a static buffer, which is exactly the
// case the copy exists for.
//
// **Thread-local**, which `io_shim.c`'s buffers are not and do not need
// to be -- a language server reads its requests on one thread, in order.
// Sockets are the opposite: `Http.serve_once` runs inside a `spawn`
// while the client reads on another thread, so a shared buffer is two
// threads writing one array.
//
// Measured, not anticipated. A single shared buffer failed `net-smoke`'s
// HTTP round trip 1 run in 10, against 10 for 10 on the previous
// `malloc`-per-call version -- the old code was leaky AND thread-safe,
// and the first version of this fix traded one for the other. One
// buffer per reading thread keeps both.
//
// Never freed, deliberately, and now bounded by the number of threads
// that read from a socket rather than by the number of reads -- which
// is the actual difference from what this replaced. There is no
// portable thread-exit hook to free it from (`pthread_key_t` with a
// destructor is POSIX-only, and this file already compiles for
// Winsock), and process exit reclaims it.
#if defined(_MSC_VER)
#define PLUM_TLS __declspec(thread)
#else
#define PLUM_TLS _Thread_local
#endif

static PLUM_TLS char *recv_buf = NULL;
static PLUM_TLS size_t recv_cap = 0;

static int recv_ensure(size_t need) {
    if (recv_cap >= need) return 1;
    size_t next = recv_cap ? recv_cap : 4096;
    while (next < need) next *= 2;
    char *bigger = (char *)realloc(recv_buf, next);
    if (bigger == NULL) return 0;
    recv_buf = bigger;
    recv_cap = next;
    return 1;
}

// See this file's own top doc comment for the full "why CStr, why
// always non-null" reasoning. Never returns NULL: an allocation failure
// falls back to a real (still non-null) empty-string static buffer
// rather than ever tripping Plum's null-CStr-return abort.
//
// Still NUL-terminated and still not binary-safe. That is deliberate
// and its contract is unchanged: `Net.read` returns a `String`, and a
// `String` is a promise about encoding that arbitrary socket bytes do
// not keep. `tcp_recv_n` below is the binary-safe path, added ALONGSIDE
// rather than by changing this one.
const char *tcp_recv(long long fd, long long max_len) {
    if (max_len < 0) {
        max_len = 0;
    }
    if (!recv_ensure((size_t)max_len + 1)) {
        return "";
    }
    long long n = (long long)recv((plum_sock)fd, recv_buf, PLUM_ADDRLEN(max_len), 0);
    if (n < 0) {
        n = 0;
    }
    recv_buf[n] = '\0';
    return recv_buf;
}

// The binary-safe receive: up to `max_len` bytes into the shared buffer,
// returning the COUNT rather than a pointer.
//
// A count is the whole point. A `CStr` return cannot carry a length, so
// `tcp_recv` has to NUL-terminate and the caller has to `strlen` -- which
// silently truncates any payload containing a zero byte. Splitting the
// answer across two calls (`tcp_recv_n` then `tcp_recv_data`) is what
// Plum's extern surface allows: no out-parameters, no multi-value
// return, and no raw pointers with a separate length.
//
// Three outcomes, kept DISTINCT, which is the other thing `tcp_recv`
// cannot express:
//
//   > 0   that many bytes are in the buffer
//   = 0   the peer closed cleanly -- end of stream, not a failure
//   < 0   a real socket error
//
// `tcp_recv` collapses the last two into `""`, documented as a v1 trade.
// A binary reader cannot live with that collapse: an empty read is an
// ordinary event in the middle of a length-framed protocol.
long long tcp_recv_n(long long fd, long long max_len) {
    if (max_len < 0) {
        max_len = 0;
    }
    if (!recv_ensure((size_t)max_len + 1)) {
        return -1;
    }
    long long n = (long long)recv((plum_sock)fd, recv_buf, PLUM_ADDRLEN(max_len), 0);
    if (n < 0) {
        return -1;
    }
    recv_buf[n] = '\0';
    return n;
}

// The bytes `tcp_recv_n` just read. Valid until the next receive, and
// meaningful only together with the count that call returned -- the
// buffer is NOT NUL-delimited data, and reading it with `strlen` is
// exactly the bug this pair exists to avoid.
//
// Never NULL, for the same reason nothing else here is: a null `CStr`
// return is a hard runtime abort under Plum's FFI semantics.
const char *tcp_recv_data(void) {
    return recv_buf ? recv_buf : "";
}

void tcp_close(long long fd) {
    PLUM_CLOSESOCK((plum_sock)fd);
}
