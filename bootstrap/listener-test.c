// Real listener setup, with failure injection at the OS boundary to prove
// every partially initialized socket is closed exactly once.
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef _WIN32
#include <winsock2.h>
#include <ws2tcpip.h>
#else
#include <sys/types.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <unistd.h>
#endif

static int fail_stage, closes, binds;
#ifdef _WIN32
static int test_option(SOCKET s, int level, int name, const char *value, int size) {
#else
static int test_option(int s, int level, int name, const void *value, socklen_t size) {
#endif
    if (fail_stage == 1) return -1;
    return setsockopt(s, level, name, value, size);
}
#ifdef _WIN32
static int test_bind(SOCKET s, const struct sockaddr *addr, int size) {
#else
static int test_bind(int s, const struct sockaddr *addr, socklen_t size) {
#endif
    binds++;
    if (fail_stage == 2) return -1;
    return bind(s, addr, size);
}
#ifdef _WIN32
static int test_listen(SOCKET s, int backlog) {
#else
static int test_listen(int s, int backlog) {
#endif
    if (fail_stage == 3) return -1;
    return listen(s, backlog);
}
#ifdef _WIN32
static int test_close(SOCKET s) {
    closes++;
    return closesocket(s);
}
#define closesocket test_close
#else
static int test_close(int s) {
    closes++;
    return close(s);
}
#define close test_close
#endif
#define setsockopt test_option
#define bind test_bind
#define listen test_listen
#include "../native_stdlib/net_shim.c"
#undef setsockopt
#undef bind
#undef listen
#ifdef _WIN32
#undef closesocket
#else
#undef close
#endif

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--hold") == 0) {
        long long fd = tcp_listen(0);
        assert(fd != -1);
        struct sockaddr_in addr;
#ifdef _WIN32
        int size = sizeof(addr);
        int option_size = sizeof(int);
        const int option = SO_EXCLUSIVEADDRUSE;
#else
        socklen_t size = sizeof(addr);
        socklen_t option_size = sizeof(int);
        const int option = SO_REUSEADDR;
#endif
        int enabled = 0;
        assert(getsockopt((plum_sock)fd, SOL_SOCKET, option,
                         (void *)&enabled, &option_size) == 0);
        assert(enabled != 0);
        assert(getsockname((plum_sock)fd, (struct sockaddr *)&addr, &size) == 0);
        printf("%u\n", (unsigned)ntohs(addr.sin_port));
        fflush(stdout);
        assert(getchar() == 'x');
        assert(PLUM_CLOSESOCK((plum_sock)fd) == 0);
        return 0;
    }
    for (int stage = 1; stage <= 3; stage++) {
        for (int i = 0; i < 100; i++) {
            fail_stage = stage;
            closes = binds = 0;
            assert(tcp_listen(0) == -1);
            assert(closes == 1);
            assert(binds == (stage == 1 ? 0 : 1));
        }
    }
    puts("listener failure paths close their socket before returning");
    return 0;
}
