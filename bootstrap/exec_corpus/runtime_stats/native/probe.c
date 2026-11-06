// Exercise each counter-producing runtime helper, independently of the
// allocator's own native bookkeeping. Read totals only after joining workers.
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

extern _Atomic int64_t plum_n_alloc, plum_b_alloc;
extern _Atomic int64_t plum_n_str, plum_n_concat, plum_n_array;
extern void *plum_alloc(int64_t);
extern void plum_free(void *);
extern void *plum_str_new(const char *, int64_t);
extern void *plum_alloc_str(int64_t);
extern void *plum_str_concat(void *, void *);
extern void *plum_array_new(int64_t);
extern void *plum_array_new_cap(int64_t, int64_t);

struct totals { int64_t allocations, bytes, strings, concats, arrays; };
static struct totals snapshot(void) {
    return (struct totals){
        atomic_load_explicit(&plum_n_alloc, memory_order_relaxed),
        atomic_load_explicit(&plum_b_alloc, memory_order_relaxed),
        atomic_load_explicit(&plum_n_str, memory_order_relaxed),
        atomic_load_explicit(&plum_n_concat, memory_order_relaxed),
        atomic_load_explicit(&plum_n_array, memory_order_relaxed)
    };
}
static atomic_int ready, start;
enum { iterations = 10000, workers = 8 };
static void *work(void *barrier) {
    if (barrier) {
        atomic_fetch_add_explicit(&ready, 1, memory_order_relaxed);
        while (!atomic_load_explicit(&start, memory_order_acquire)) {}
    }
    for (int i = 0; i < iterations; ++i) {
        void *raw = plum_alloc(32);
        void *a = plum_str_new("abc", 3);
        void *b = plum_alloc_str(3);
        memcpy((char *)b + 16, "abc", 4);
        void *joined = plum_str_concat(a, b);
        void *exact = plum_array_new(1);
        void *capacity = plum_array_new_cap(1, 3);
        plum_free(capacity); plum_free(exact); plum_free(joined); plum_free(b); plum_free(a); plum_free(raw);
    }
    return NULL;
}
static int check(int parallel) {
    struct totals before = snapshot();
    if (parallel) {
        pthread_t ids[workers];
        atomic_store_explicit(&ready, 0, memory_order_relaxed);
        atomic_store_explicit(&start, 0, memory_order_relaxed);
        for (int i = 0; i < workers; ++i)
            if (pthread_create(&ids[i], NULL, work, &start)) abort();
        while (atomic_load_explicit(&ready, memory_order_relaxed) != workers) {}
        atomic_store_explicit(&start, 1, memory_order_release);
        for (int i = 0; i < workers; ++i)
            if (pthread_join(ids[i], NULL)) abort();
    } else { work(NULL); }
    struct totals after = snapshot();
    int64_t n = iterations * (parallel ? workers : 1);
    int ok = after.allocations - before.allocations == 6 * n
        && after.bytes - before.bytes == 159 * n
        && after.strings - before.strings == 2 * n
        && after.concats - before.concats == n
        && after.arrays - before.arrays == 2 * n;
    if (!ok) fprintf(stderr,
        "counter delta: alloc=%lld bytes=%lld str=%lld concat=%lld array=%lld (iterations=%lld)\n",
        (long long)(after.allocations - before.allocations),
        (long long)(after.bytes - before.bytes),
        (long long)(after.strings - before.strings),
        (long long)(after.concats - before.concats),
        (long long)(after.arrays - before.arrays), (long long)n);
    return ok;
}
int64_t counter_check(void) {
    if (!check(0)) return 0;
    for (int i = 0; i < 3; ++i) if (!check(1)) return 0;
    return 1;
}
