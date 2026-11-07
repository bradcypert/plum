#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include "../../../../native_stdlib/memory_shim.h"

extern int64_t plum_heap_origin_snapshot(void);
extern int64_t plum_heap_snapshot_field(int64_t, int64_t);
extern int64_t plum_heap_origin_field(int64_t, int64_t, int64_t);
int64_t origin_input(void) { return 1; }
int64_t origin_live(int64_t origin) {
    int64_t t = plum_heap_origin_snapshot();
    return t ? plum_heap_origin_field(t, origin, 2) : -1;
}
int64_t origin_allocations(int64_t origin) {
    int64_t t = plum_heap_origin_snapshot();
    return t ? plum_heap_origin_field(t, origin, 0) : -1;
}

#ifdef PLUM_HEAP_STATS
extern void *plum_alloc_origin(int64_t, int64_t);
extern void plum_rc_inc(void *);
extern void plum_rel_str(void *);
extern void *plum_str_new(const char *, int64_t);
extern void *plum_array_new_cap(int64_t, int64_t);
extern _Bool plum_reuse_ok(void *, int64_t);
struct profile { int64_t total[6], rows[PLUM_ORIGIN_COUNT][6]; };
static struct profile capture(void) {
    struct profile p;
    int64_t token = plum_heap_origin_snapshot();
    for (int f = 0; f < 6; f++) {
        p.total[f] = plum_heap_snapshot_field(token, f);
        for (int k = 0; k < PLUM_ORIGIN_COUNT; k++)
            p.rows[k][f] = plum_heap_origin_field(token, k, f);
    }
    return p;
}
static int consistent(struct profile p) {
    for (int f = 0; f < 4; f++) {
        int64_t sum = 0;
        for (int k = 0; k < PLUM_ORIGIN_COUNT; k++) sum += p.rows[k][f];
        if (sum != p.total[f]) return 0;
    }
    for (int k = 0; k < PLUM_ORIGIN_COUNT; k++) {
        if (p.rows[k][4] < p.rows[k][2] || p.rows[k][5] < p.rows[k][3]) return 0;
    }
    return p.total[4] >= p.total[2] && p.total[5] >= p.total[3];
}
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "origin registry failure at line %d\n", __LINE__); return 0; } } while (0)
static int lifetime(void) {
    CHECK(PLUM_ORIGIN_COUNT == 8);
    struct profile before = capture();
    void *cells[PLUM_ORIGIN_COUNT];
    for (int k = 0; k < PLUM_ORIGIN_COUNT; k++) cells[k] = plum_alloc_origin(24 + 8 * k, k);
    struct profile full = capture();
    CHECK(consistent(full));
    for (int k = 0; k < PLUM_ORIGIN_COUNT; k++) {
        CHECK(full.rows[k][0] == before.rows[k][0] + 1);
        CHECK(full.rows[k][1] == before.rows[k][1] + 24 + 8 * k);
        CHECK(full.rows[k][2] == before.rows[k][2] + 1);
        CHECK(full.rows[k][3] == before.rows[k][3] + 24 + 8 * k);
        CHECK(plum_reuse_ok(cells[k], 16));
    }
    // Aliases do not add an allocation; a nonfinal release keeps its origin.
    plum_rc_inc(cells[PLUM_ORIGIN_BYTES]);
    plum_rel_str(cells[PLUM_ORIGIN_BYTES]);
    struct profile alias = capture();
    for (int f = 0; f < 4; f++) CHECK(alias.total[f] == full.total[f]);
    for (int k = 0; k < PLUM_ORIGIN_COUNT; k++) plum_free(cells[k]);
    void *native = plum_alloc(32);
    struct profile fallback = capture();
    CHECK(fallback.rows[0][2] == before.rows[0][2] + 1);
    plum_free(native);
    native = malloc(31); if (!native) abort(); plum_free(native);
    void *text = plum_str_new("abc", 3);
    void *array = plum_array_new_cap(0, 80);
    struct profile capacity = capture();
    CHECK(capacity.rows[PLUM_ORIGIN_BYTES][3] == before.rows[PLUM_ORIGIN_BYTES][3] + 20);
    CHECK(capacity.rows[PLUM_ORIGIN_ARRAY][3] == before.rows[PLUM_ORIGIN_ARRAY][3] + 656);
    void *grown = plum_array_new_cap(0, 160);
    struct profile overlap = capture();
    CHECK(overlap.rows[PLUM_ORIGIN_ARRAY][2] == before.rows[PLUM_ORIGIN_ARRAY][2] + 2);
    CHECK(overlap.rows[PLUM_ORIGIN_ARRAY][3] == before.rows[PLUM_ORIGIN_ARRAY][3] + 656 + 1296);
    plum_free(array); plum_free(grown); plum_rel_str(text);
    struct profile after = capture();
    CHECK(consistent(after));
    for (int k = 0; k < PLUM_ORIGIN_COUNT; k++) {
        CHECK(after.rows[k][2] == before.rows[k][2]);
        CHECK(after.rows[k][3] == before.rows[k][3]);
    }
    // Sequential peaks occur at different times; adding them invents a
    // process peak that never existed. Use a size exceeding prior activity.
    int64_t large = after.total[5] + 65536;
    for (int k = 0; k < PLUM_ORIGIN_COUNT; k++) {
        void *p = plum_alloc_origin(large, k); plum_free(p);
    }
    struct profile peaks = capture();
    int64_t sum = 0;
    for (int k = 0; k < PLUM_ORIGIN_COUNT; k++) sum += peaks.rows[k][5];
    CHECK(sum > peaks.total[5]);
    return 1;
}
enum { workers = 8, iterations = 4000 };
static int grow_and_churn(void) {
    struct profile before = capture();
    void *cells[4096];
    for (int round = 0; round < 3; round++) {
        for (int i = 0; i < 4096; i++) cells[i] = plum_alloc_origin(24 + 8 * (i % 8), i % 8);
        struct profile full = capture();
        CHECK(consistent(full));
        for (int k = 0; k < PLUM_ORIGIN_COUNT; k++) {
            CHECK(full.rows[k][2] == before.rows[k][2] + 512);
            CHECK(full.rows[k][3] == before.rows[k][3] + 512 * (24 + 8 * k));
        }
        for (int i = 0; i < 4096; i++) plum_free(cells[i]);
        struct profile empty = capture();
        for (int k = 0; k < PLUM_ORIGIN_COUNT; k++) {
            CHECK(empty.rows[k][2] == before.rows[k][2]);
            CHECK(empty.rows[k][3] == before.rows[k][3]);
        }
    }
    return 1;
}
static atomic_int ready, start, finished;
static void *worker(void *raw) {
    int kind = (int)(intptr_t)raw;
    atomic_fetch_add_explicit(&ready, 1, memory_order_relaxed);
    while (!atomic_load_explicit(&start, memory_order_acquire)) {}
    for (int i = 0; i < iterations; i++) {
        void *p = plum_alloc_origin(24 + 8 * kind, kind);
        if (!consistent(capture())) abort();
        plum_free(p);
    }
    atomic_fetch_add_explicit(&finished, 1, memory_order_release);
    return NULL;
}
static int threaded(void) {
    struct profile before = capture(), previous = before;
    int64_t token = plum_heap_origin_snapshot();
    int64_t frozen = plum_heap_origin_field(token, PLUM_ORIGIN_RECORD, 0);
    pthread_t ids[workers];
    atomic_store(&ready, 0); atomic_store(&start, 0); atomic_store(&finished, 0);
    for (int k = 0; k < workers; k++)
        if (pthread_create(&ids[k], NULL, worker, (void *)(intptr_t)k)) abort();
    while (atomic_load_explicit(&ready, memory_order_relaxed) != workers) {}
    atomic_store_explicit(&start, 1, memory_order_release);
    // Workers may replace their own frozen samples, never this thread's.
    while (atomic_load_explicit(&finished, memory_order_acquire) != workers) {
        if (plum_heap_origin_field(token, PLUM_ORIGIN_RECORD, 0) != frozen) abort();
    }
    for (int k = 0; k < workers; k++) if (pthread_join(ids[k], NULL)) abort();
    struct profile after = capture();
    CHECK(consistent(after));
    for (int k = 0; k < workers; k++) {
        CHECK(after.rows[k][0] - before.rows[k][0] == iterations);
        CHECK(after.rows[k][1] - before.rows[k][1] == iterations * (24 + 8 * k));
        CHECK(after.rows[k][2] == before.rows[k][2]);
        CHECK(after.rows[k][3] == before.rows[k][3]);
        CHECK(after.rows[k][4] >= previous.rows[k][4] && after.rows[k][5] >= previous.rows[k][5]);
    }
    return 1;
}
int64_t check_origin_registry(void) { return lifetime() && grow_and_churn() && threaded(); }
#else
int64_t check_origin_registry(void) { return 0; }
#endif
