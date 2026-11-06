#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

extern void *plum_alloc(int64_t);
extern void plum_free(void *);
extern void *plum_str_new(const char *, int64_t);
extern void *plum_array_new_cap(int64_t, int64_t);
extern void plum_rc_inc(void *);
extern void plum_rel_str(void *);
extern _Bool plum_reuse_ok(void *, int64_t);
extern int64_t plum_heap_snapshot(void);
extern int64_t plum_heap_snapshot_field(int64_t, int64_t);
struct sample { int64_t n, bytes, live, live_bytes, peak, peak_bytes; };
static struct sample snapshot(void) {
    int64_t t = plum_heap_snapshot();
    return (struct sample){plum_heap_snapshot_field(t, 0), plum_heap_snapshot_field(t, 1),
        plum_heap_snapshot_field(t, 2), plum_heap_snapshot_field(t, 3),
        plum_heap_snapshot_field(t, 4), plum_heap_snapshot_field(t, 5)};
}
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "memory registry check failed at line %d\n", __LINE__); return 0; } } while (0)

static int aliases_and_capacity(void) {
    struct sample before = snapshot();
    void *s = plum_str_new("abc", 3);
    plum_rc_inc(s);
    plum_rel_str(s);
    struct sample shared = snapshot();
    CHECK(shared.live == before.live + 1 && shared.live_bytes == before.live_bytes + 20);
    plum_rel_str(s);
    struct sample gone = snapshot();
    CHECK(gone.live == before.live && gone.live_bytes == before.live_bytes);
    void *a = plum_array_new_cap(0, 80);
    struct sample capacity = snapshot();
    CHECK(capacity.live_bytes == before.live_bytes + 656);
    CHECK(plum_reuse_ok(a, 32));
    struct sample reused = snapshot();
    CHECK(reused.n == capacity.n && reused.live_bytes == capacity.live_bytes);
    plum_free(a);
    void *native = malloc(88);
    if (!native) abort();
    plum_free(native);
    struct sample after = snapshot();
    CHECK(after.live == before.live && after.live_bytes == before.live_bytes);
    CHECK(after.n == before.n + 2 && after.bytes == before.bytes + 676);
    return 1;
}
static void *freeze_on_worker(void *unused) {
    (void)unused;
    void *cell = plum_alloc(32);
    (void)snapshot();
    plum_free(cell);
    return NULL;
}
static int frozen_copy(void) {
    int64_t token = plum_heap_snapshot();
    int64_t old_count = plum_heap_snapshot_field(token, 0);
    pthread_t id;
    if (pthread_create(&id, NULL, freeze_on_worker, NULL) || pthread_join(id, NULL)) abort();
    CHECK(plum_heap_snapshot_field(token, 0) == old_count);
    return 1;
}
static int grow_and_churn(void) {
    struct sample before = snapshot();
    void *cells[4096];
    for (int round = 0; round < 3; ++round) {
        for (int i = 0; i < 4096; ++i) cells[i] = plum_alloc(24);
        struct sample full = snapshot();
        CHECK(full.live == before.live + 4096 && full.live_bytes == before.live_bytes + 4096 * 24);
        for (int i = 0; i < 4096; ++i) plum_free(cells[i]);
        struct sample empty = snapshot();
        CHECK(empty.live == before.live && empty.live_bytes == before.live_bytes);
        CHECK(empty.peak >= full.live && empty.peak_bytes >= full.live_bytes);
    }
    return 1;
}
static atomic_int ready, start, finished;
enum { workers = 8, iterations = 10000 };
static void *work(void *unused) {
    (void)unused;
    atomic_fetch_add_explicit(&ready, 1, memory_order_relaxed);
    while (!atomic_load_explicit(&start, memory_order_acquire)) {}
    for (int i = 0; i < iterations; ++i) {
        void *cell = plum_alloc(32);
        plum_free(cell);
    }
    atomic_fetch_add_explicit(&finished, 1, memory_order_release);
    return NULL;
}
static int threaded(void) {
    pthread_t ids[workers];
    struct sample before = snapshot(), previous = before;
    atomic_store(&ready, 0); atomic_store(&start, 0); atomic_store(&finished, 0);
    for (int i = 0; i < workers; ++i)
        if (pthread_create(&ids[i], NULL, work, NULL)) abort();
    while (atomic_load_explicit(&ready, memory_order_relaxed) != workers) {}
    atomic_store_explicit(&start, 1, memory_order_release);
    int valid = 1;
    do {
        struct sample s = snapshot();
        valid &= s.bytes - before.bytes == (s.n - before.n) * 32;
        valid &= s.live_bytes - before.live_bytes == (s.live - before.live) * 32;
        valid &= s.peak >= previous.peak && s.peak_bytes >= previous.peak_bytes;
        valid &= s.live >= before.live && s.peak >= s.live && s.peak_bytes >= s.live_bytes;
        previous = s;
    } while (atomic_load_explicit(&finished, memory_order_acquire) != workers);
    for (int i = 0; i < workers; ++i) if (pthread_join(ids[i], NULL)) abort();
    struct sample after = snapshot();
    CHECK(valid);
    CHECK(after.n - before.n == workers * iterations);
    CHECK(after.bytes - before.bytes == workers * iterations * 32);
    CHECK(after.live == before.live && after.live_bytes == before.live_bytes);
    return 1;
}
int64_t check_memory_registry(void) {
    return aliases_and_capacity() && frozen_copy() && grow_and_churn() && threaded();
}
