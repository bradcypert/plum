/* Opt-in Plum-cell accounting. Metadata/native buffers are outside the totals.
 * Cells retain their allocator pointer and layout, including through reuse. */
#include <stdint.h>
#include <stdlib.h>

#ifdef PLUM_HEAP_STATS
#include <limits.h>
#include <pthread.h>
#include <stdio.h>

/* Complemented keys keep the diagnostic index from becoming an accidental
 * conservative LeakSanitizer root for otherwise unreachable Plum cells. */
struct entry { uintptr_t key; int64_t size; };
struct totals {
    int64_t allocations, allocated_bytes, live_allocations, live_bytes;
    int64_t peak_allocations, peak_bytes;
};
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static struct entry *entries;
static size_t capacity, occupied, live;
static struct totals totals;
/* A frozen copy belongs to the calling thread until its next snapshot. No
 * Plum allocation/callback occurs between freezing and reading its fields. */
static _Thread_local struct totals frozen;
static _Thread_local int frozen_valid;
#define TOMBSTONE ((uintptr_t)1)

static void fail(const char *message) {
    fputs(message, stderr);
    abort();
}
static size_t bucket(uintptr_t key, size_t count) {
    uint64_t x = (uint64_t)key;
    x ^= x >> 33; x *= UINT64_C(0xff51afd7ed558ccd);
    x ^= x >> 33;
    return (size_t)x & (count - 1);
}
static void insert_into(struct entry *table, size_t count, struct entry value) {
    size_t i = bucket(value.key, count);
    while (table[i].key) i = (i + 1) & (count - 1);
    table[i] = value;
}
/* Called with lock held. Compact tombstones without increasing capacity for
 * bounded churn. Metadata allocation uses libc directly and is not tracked. */
static void make_room(void) {
    if (capacity && occupied < capacity - capacity / 4) return;
    size_t next = capacity ? capacity : 256;
    if (live >= next / 2) {
        if (next > SIZE_MAX / 2) fail("Plum memory diagnostics: registry capacity overflow\n");
        next *= 2;
    }
    if (next > SIZE_MAX / sizeof(*entries)) fail("Plum memory diagnostics: registry size overflow\n");
    struct entry *table = calloc(next, sizeof(*table));
    if (!table) fail("Plum memory diagnostics: cannot allocate registry\n");
    for (size_t i = 0; i < capacity; ++i)
        if (entries[i].key && entries[i].key != TOMBSTONE)
            insert_into(table, next, entries[i]);
    free(entries);
    entries = table; capacity = next; occupied = live;
}

void *plum_heap_alloc(int64_t size) {
    if (size < 0) fail("Plum memory diagnostics: invalid allocation size\n");
    void *pointer = malloc((size_t)size);
    if (!pointer) fail("Plum memory diagnostics: allocation failed\n");
    uintptr_t key = ~(uintptr_t)pointer;
    pthread_mutex_lock(&lock);
    if (totals.allocations == INT64_MAX || totals.allocated_bytes > INT64_MAX - size)
        fail("Plum memory diagnostics: counter overflow\n");
    make_room();
    size_t i = bucket(key, capacity), hole = SIZE_MAX;
    while (entries[i].key) {
        if (entries[i].key == key) fail("Plum memory diagnostics: duplicate live cell\n");
        if (entries[i].key == TOMBSTONE && hole == SIZE_MAX) hole = i;
        i = (i + 1) & (capacity - 1);
    }
    if (hole != SIZE_MAX) i = hole;
    else ++occupied;
    entries[i] = (struct entry){key, size};
    ++live;
    ++totals.allocations; totals.allocated_bytes += size;
    ++totals.live_allocations; totals.live_bytes += size;
    if (totals.live_allocations > totals.peak_allocations) totals.peak_allocations = totals.live_allocations;
    if (totals.live_bytes > totals.peak_bytes) totals.peak_bytes = totals.live_bytes;
    pthread_mutex_unlock(&lock);
    return pointer;
}

void plum_heap_free(void *pointer) {
    if (!pointer) return;
    uintptr_t key = ~(uintptr_t)pointer;
    pthread_mutex_lock(&lock);
    if (capacity) {
        size_t i = bucket(key, capacity);
        while (entries[i].key) {
            if (entries[i].key == key) {
                --totals.live_allocations; totals.live_bytes -= entries[i].size;
                entries[i].key = TOMBSTONE; entries[i].size = 0;
                --live;
                break;
            }
            i = (i + 1) & (capacity - 1);
        }
    }
    pthread_mutex_unlock(&lock);
    /* Native buffers may pass through this boundary too. Unknown pointers
     * are freed normally, without claiming they were Plum-cell allocations. */
    free(pointer);
}

int64_t plum_heap_snapshot(void) {
    pthread_mutex_lock(&lock);
    frozen = totals;
    pthread_mutex_unlock(&lock);
    frozen_valid = 1;
    return (int64_t)(intptr_t)&frozen;
}
int64_t plum_heap_snapshot_field(int64_t token, int64_t field) {
    if (!frozen_valid || token != (int64_t)(intptr_t)&frozen)
        fail("Plum memory diagnostics: snapshot belongs to another thread\n");
    switch (field) {
        case 0: return frozen.allocations;
        case 1: return frozen.allocated_bytes;
        case 2: return frozen.live_allocations;
        case 3: return frozen.live_bytes;
        case 4: return frozen.peak_allocations;
        case 5: return frozen.peak_bytes;
        default: fail("Plum memory diagnostics: invalid snapshot field\n"); return 0;
    }
}
#else
/* Ordinary builds contain no registry, mutex, TLS snapshot or cell metadata. */
/* Private allocation hooks deliberately have no disabled stubs: tracked IR
 * linked without the matching native flag must fail rather than undercount. */
int64_t plum_heap_snapshot(void) { return 0; }
int64_t plum_heap_snapshot_field(int64_t token, int64_t field) {
    (void)token; (void)field; return 0;
}
#endif
