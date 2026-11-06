// TCP native adapter. Receive storage belongs to the caller (NativeBuffer's
// exclusive lease); native code borrows its pointer only for the syscall.
// POSIX/Winsock differences are confined to socket/count adapters below.

#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#include <limits.h>

// Winsock is a different API in the details, not in the shape: the same
// `socket`/`bind`/`listen`/`accept`/`send`/`recv` calls with a
// different handle type, a different close, a different error
// sentinel, and a mandatory one-time initialisation. Those four
// differences are abstracted here and NOTHING below is written twice --
// the alternative, a whole second copy of each function, is how the two
// halves drift apart.
//
#if defined(_WIN32)
// winsock2.h must precede windows.h, which ws2tcpip.h pulls in.
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>

typedef SOCKET plum_sock;
#define PLUM_BAD_SOCK       INVALID_SOCKET
#define PLUM_CLOSESOCK(s)   closesocket(s)
#define PLUM_SOCKOPT(v)     ((const char *)(v))
// Winsock takes an `int` byte count. Preserve the socket API's existing
// partial-I/O behavior by limiting each call to its largest representable
// count instead of allowing a large Plum Int to wrap when narrowed.
#define PLUM_IO_LEN(n)      ((n) > INT_MAX ? INT_MAX : (n))
#define PLUM_ADDRLEN(n)     ((int)PLUM_IO_LEN(n))

// Serialize lazy startup and publish success to all calling threads.
// FALSE leaves the once object uninitialized: this caller gets an error,
// and a waiting or later caller may retry. Never loop on startup failure.
// Keep exactly one successful Winsock reference for this adapter's process
// lifetime; per-socket cleanup must not shut down other threads' sockets.
static INIT_ONCE plum_net_once = INIT_ONCE_STATIC_INIT;

static BOOL CALLBACK plum_net_start(PINIT_ONCE once, PVOID param, PVOID *context) {
    (void)once;
    (void)param;
    (void)context;
    WSADATA wsa;
    return WSAStartup(MAKEWORD(2, 2), &wsa) == 0;
}

static int plum_net_init(void) {
    return InitOnceExecuteOnce(&plum_net_once, plum_net_start, NULL, NULL) ? 0 : -1;
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
#define PLUM_IO_LEN(n)      (n)
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
// picks whatever resolves). POSIX uses `SO_REUSEADDR` for server restarts
// across TIME_WAIT. Winsock gives that option different semantics: another
// process can forcibly bind the same port. Require `SO_EXCLUSIVEADDRUSE`
// there before binding, even though accepted connections can delay reuse
// after closure. Never publish a listener if its binding policy failed.
// Returns the listening fd, or -1 on failure.
long long tcp_listen(long long port) {
    plum_sock s;
    if (plum_net_init() != 0) return -1;
    s = socket(AF_INET, SOCK_STREAM, 0);
    if (s == PLUM_BAD_SOCK) {
        return -1;
    }
    int one = 1;
#if defined(_WIN32)
    int option = SO_EXCLUSIVEADDRUSE;
#else
    int option = SO_REUSEADDR;
#endif
    if (setsockopt(s, SOL_SOCKET, option, PLUM_SOCKOPT(&one), sizeof(one)) != 0) {
        PLUM_CLOSESOCK(s);
        return -1;
    }

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

// Receives only into caller-owned, leased storage. No hidden buffer survives
// the call or a worker thread's exit. Positive capacity distinguishes an
// actual EOF from an invalid zero-size request. Windows limits each syscall
// to its representable count, preserving partial-I/O semantics.
long long tcp_recv_into(long long fd, unsigned char *data, long long capacity) {
    if (!data || capacity <= 0) return -1;
    return (long long)recv((plum_sock)fd, (char *)data, PLUM_ADDRLEN(capacity), 0);
}

void tcp_close(long long fd) {
    PLUM_CLOSESOCK((plum_sock)fd);
}
