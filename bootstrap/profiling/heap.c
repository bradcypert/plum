// Linux/GNU ld measurement of direct linked malloc/calloc/realloc/free calls.
// Registry memory and allocator-internal allocations are excluded.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#define CAP (1u << 20)
static void *pointers[CAP];
static size_t sizes[CAP], live, peak;
void *__real_malloc(size_t);
void *__real_calloc(size_t, size_t);
void *__real_realloc(void *, size_t);
void __real_free(void *);
static unsigned slot(void *p) { return ((uintptr_t)p >> 4) * 2654435761u & (CAP - 1); }
static unsigned find(void *p) {
    unsigned i = slot(p), start = i;
    while (pointers[i]) {
        if (pointers[i] == p) return i;
        i = (i + 1) & (CAP - 1);
        if (i == start) break;
    }
    return CAP;
}
static void add(void *p, size_t n) {
    if (!p) return;
    unsigned i = slot(p), start = i;
    while (pointers[i] && pointers[i] != (void *)1) {
        i = (i + 1) & (CAP - 1);
        if (i == start) abort();
    }
    pointers[i] = p; sizes[i] = n;
    live += n; if (live > peak) peak = live;
}
static void remove_slot(unsigned i) {
    if (i != CAP) { live -= sizes[i]; pointers[i] = (void *)1; }
}
void *__wrap_malloc(size_t n) { void *p = __real_malloc(n); add(p, n); return p; }
void *__wrap_calloc(size_t n, size_t s) {
    void *p = __real_calloc(n, s); add(p, n * s); return p;
}
void *__wrap_realloc(void *p, size_t n) {
    unsigned i = p ? find(p) : CAP;
    void *out = __real_realloc(p, n);
    if (out || n == 0) { remove_slot(i); add(out, n); }
    return out;
}
void __wrap_free(void *p) { if (p) remove_slot(find(p)); __real_free(p); }
__attribute__((destructor)) static void report(void) {
    fprintf(stderr, "compiler-heap: peak_bytes=%zu exit_bytes=%zu\n", peak, live);
}
