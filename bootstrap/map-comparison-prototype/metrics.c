// THROWAWAY, single-threaded measurement. Registry storage excluded from payload.
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <time.h>
extern long long plum_n_alloc, plum_b_alloc;
static size_t live_bytes, peak_bytes;
#ifdef TRACK
#define CAP (1u << 20)
static void *ptrs[CAP];
static size_t sizes[CAP];
void *__real_malloc(size_t);
void __real_free(void *);
static unsigned slot(void *p) { return ((uintptr_t)p >> 4) * 2654435761u & (CAP - 1); }
void *__wrap_malloc(size_t n) {
    void *p = __real_malloc(n);
    if (!p) abort();
    unsigned i = slot(p), start = i;
    while (ptrs[i] && ptrs[i] != (void *)1) { i = (i + 1) & (CAP - 1); if (i == start) abort(); }
    ptrs[i] = p; sizes[i] = n;
    live_bytes += n; if (live_bytes > peak_bytes) peak_bytes = live_bytes;
    return p;
}
void __wrap_free(void *p) {
    if (!p) return;
    unsigned i = slot(p), start = i;
    while (ptrs[i]) {
        if (ptrs[i] == p) { live_bytes -= sizes[i]; ptrs[i] = (void *)1; break; }
        i = (i + 1) & (CAP - 1); if (i == start) break;
    }
    __real_free(p);
}
#endif
long long proto_config(long long which) {
    const char *s = getenv(which == 0 ? "PROTO_N" : "PROTO_MODE");
    return s ? atoll(s) : (which == 0 ? 1000 : 0);
}
void proto_mark(long long phase) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    fprintf(stderr, "METRIC %lld %lld %lld %lld %zu %zu\n", phase,
        (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec,
        plum_n_alloc, plum_b_alloc, live_bytes, peak_bytes);
}
