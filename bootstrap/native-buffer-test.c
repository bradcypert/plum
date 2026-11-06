/* Exercise real native ownership, including worker exit. TLS supplies a
 * deterministic SSL-read adapter; session setup/verification are untouched. */
#include <assert.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
#include "../native_stdlib/native_buffer.h"
#define mbedtls_ssl_read buffer_test_ssl_read
#include "../native_stdlib/tls_shim.c"
#undef mbedtls_ssl_read
static _Thread_local int again;
static _Atomic int completed, started, proceed;
int buffer_test_ssl_read(mbedtls_ssl_context *ssl, unsigned char *data, size_t capacity) {
    (void)ssl;
    if (!again++) return MBEDTLS_ERR_SSL_WANT_READ;
    if (capacity < 4) return -1;
    memcpy(data, "a\0bc", 4);
    return 4;
}
extern long long tcp_recv_into(long long, unsigned char *, long long);
static long long field(int n) {
    assert(plum_buffer_snapshot() == 1);
    return plum_buffer_snapshot_field(1, n);
}
static void *worker(void *arg) {
    (void)arg;
    long long b = plum_buffer_create(4);
    assert(b > 0);
    long long lease = plum_buffer_acquire(b, 1);
    assert(lease == b);
    int pair[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
    assert(write(pair[0], "a\0bc", 4) == 4);
    unsigned char *data = plum_buffer_data(lease);
    assert(tcp_recv_into(pair[1], data, 4) == 4);
    assert(plum_buffer_commit(lease, 4) == 0);
    plum_buffer_end(lease);
    close(pair[0]); close(pair[1]);
    lease = plum_buffer_acquire(b, 1);
    plum_tls tls = {0};
    assert(tls_recv_into((long long)(intptr_t)&tls, plum_buffer_data(lease), 4) == 4);
    assert(plum_buffer_commit(lease, 4) == 0);
    assert(memcmp(plum_buffer_data(lease), "a\0bc", 4) == 0);
    assert(plum_buffer_reserve(b, 32) == -2);
    assert(plum_buffer_acquire(b, 0) == -2);
    atomic_fetch_add(&started, 1);
    while (!atomic_load(&proceed)) sched_yield();
    /* The lease must independently keep storage alive after owner release. */
    plum_buffer_release(b);
    assert(memcmp(plum_buffer_data(lease), "a\0bc", 4) == 0);
    plum_buffer_end(lease);
    atomic_fetch_add(&completed, 1);
    return NULL;
}
int main(void) {
    assert(field(2) == 0 && field(3) == 0);
    for (int batch = 0; batch < 8; batch++) {
        pthread_t threads[8];
        atomic_store(&completed, 0);
        atomic_store(&started, 0);
        atomic_store(&proceed, 0);
        for (int i = 0; i < 8; i++) assert(!pthread_create(&threads[i], NULL, worker, NULL));
        while (atomic_load(&started) < 8) sched_yield();
        assert(field(2) == 8 && field(3) == 32);
        atomic_store(&proceed, 1);
        do {
            assert(plum_buffer_snapshot() == 1);
            long long count = plum_buffer_snapshot_field(1, 2);
            long long capacity = plum_buffer_snapshot_field(1, 3);
            assert(capacity == count * 4);
            assert(plum_buffer_snapshot_field(1, 4) >= count);
            assert(plum_buffer_snapshot_field(1, 5) >= capacity);
        } while (atomic_load(&completed) < 8);
        for (int i = 0; i < 8; i++) assert(!pthread_join(threads[i], NULL));
        assert(field(2) == 0 && field(3) == 0);
    }
    assert(field(0) == 64 && field(1) == 256);
    assert(field(4) >= 1 && field(5) >= 4);
    assert(tcp_recv_into(-1, NULL, 0) == -1);
    assert(tls_recv_into(0, NULL, 0) == -1);
    puts("native-buffer-test: 64 TCP/TLS workers release every buffer and lease");
    return 0;
}
