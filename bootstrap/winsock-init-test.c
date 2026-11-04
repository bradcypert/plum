// Compile twice to exercise the plain TCP and bundled HTTPS adapters.
// Only WSAStartup is intercepted; synchronization and sockets use Windows.
#undef NDEBUG
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>

static LONG attempts, successes, active, overlap;
static LONG fail_count;
static HANDLE gate, ready;

static int WSAAPI test_startup(WORD version, LPWSADATA data) {
    LONG call = InterlockedIncrement(&attempts);
    if (InterlockedIncrement(&active) != 1) InterlockedExchange(&overlap, 1);
    // Hold the startup window open while all released workers enter.
    Sleep(100);
    int result = call <= fail_count ? WSASYSNOTREADY : WSAStartup(version, data);
    if (result == 0) InterlockedIncrement(&successes);
    InterlockedDecrement(&active);
    return result;
}

#define WSAStartup test_startup
#ifdef TEST_MBEDTLS
#include "../vendor/mbedtls/library/net_sockets.c"
#undef read
#undef write
#undef close
#else
#include "../native_stdlib/net_shim.c"
#endif
#undef WSAStartup

static long long listen_local(void) {
#ifdef TEST_MBEDTLS
    mbedtls_net_context ctx;
    mbedtls_net_init(&ctx);
    int result = mbedtls_net_bind(&ctx, "127.0.0.1", "0", MBEDTLS_NET_PROTO_TCP);
    if (result != 0) {
        assert(result == MBEDTLS_ERR_NET_SOCKET_FAILED);
        assert(ctx.fd == -1);
        return -1;
    }
    return ctx.fd;
#else
    return tcp_listen(0);
#endif
}

static long long connect_local(int port) {
#ifdef TEST_MBEDTLS
    char service[16];
    snprintf(service, sizeof(service), "%d", port);
    mbedtls_net_context ctx;
    mbedtls_net_init(&ctx);
    int result = mbedtls_net_connect(&ctx, "127.0.0.1", service, MBEDTLS_NET_PROTO_TCP);
    if (result != 0) {
        assert(result == MBEDTLS_ERR_NET_SOCKET_FAILED);
        assert(ctx.fd == -1);
        return -1;
    }
    return ctx.fd;
#else
    return tcp_connect("127.0.0.1", port);
#endif
}

static DWORD WINAPI worker(LPVOID unused) {
    (void)unused;
    assert(ReleaseSemaphore(ready, 1, NULL));
    assert(WaitForSingleObject(gate, 10000) == WAIT_OBJECT_0);
    for (int i = 0; i < 8; i++) {
        long long listener = listen_local();
        // A concurrent failed startup belongs to its caller. A subsequent
        // operation must retry rather than permanently poisoning the adapter.
        if (listener == -1) listener = listen_local();
        assert(listener != -1);
        assert(closesocket((SOCKET)listener) == 0);
    }
    return 0;
}

int main(int argc, char **argv) {
    (void)argv;
    enum { workers = 32 };
    HANDLE threads[workers];
    fail_count = argc > 1 ? 3 : 2;
    // Both public entry points propagate startup failure and permit retry.
    assert(listen_local() == -1);
    assert(connect_local(0) == -1);
    assert(attempts == 2 && successes == 0);

    gate = CreateEventA(NULL, TRUE, FALSE, NULL);
    ready = CreateSemaphoreA(NULL, 0, workers, NULL);
    assert(gate && ready);
    for (int i = 0; i < workers; i++) {
        threads[i] = CreateThread(NULL, 0, worker, NULL, 0, NULL);
        assert(threads[i]);
    }
    for (int i = 0; i < workers; i++)
        assert(WaitForSingleObject(ready, 10000) == WAIT_OBJECT_0);
    assert(SetEvent(gate));
    assert(WaitForMultipleObjects(workers, threads, TRUE, 15000) == WAIT_OBJECT_0);
    for (int i = 0; i < workers; i++) assert(CloseHandle(threads[i]));
    assert(CloseHandle(gate) && CloseHandle(ready));
    assert(overlap == 0 && active == 0);
    assert(attempts == fail_count + 1 && successes == 1);

    // Real loopback connect after the concurrent first-use wave, and no new
    // startup reference after hundreds of listener creations and closes.
    long long listener = listen_local();
    assert(listener != -1);
    struct sockaddr_in address;
    int length = sizeof(address);
    assert(getsockname((SOCKET)listener, (struct sockaddr *)&address, &length) == 0);
    long long client = connect_local(ntohs(address.sin_port));
    assert(client != -1);
    SOCKET peer = accept((SOCKET)listener, NULL, NULL);
    assert(peer != INVALID_SOCKET);
    assert(send((SOCKET)client, "ok", 2, 0) == 2);
    char buffer[2];
    assert(recv(peer, buffer, 2, MSG_WAITALL) == 2);
    assert(buffer[0] == 'o' && buffer[1] == 'k');
    assert(closesocket(peer) == 0);
    assert(closesocket((SOCKET)client) == 0);
    assert(closesocket((SOCKET)listener) == 0);
    assert(attempts == fail_count + 1 && successes == 1);
    // Balance the adapter's sole successful startup after all test sockets
    // and workers are gone. One cleanup must fully release its reference.
    assert(WSACleanup() == 0);
    assert(socket(AF_INET, SOCK_STREAM, 0) == INVALID_SOCKET);
    assert(WSAGetLastError() == WSANOTINITIALISED);
    puts("Winsock startup: retry, concurrent first use, one reference, loopback OK");
    return 0;
}
