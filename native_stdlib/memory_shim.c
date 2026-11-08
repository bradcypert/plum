/* Opt-in Plum-cell accounting. Metadata/native buffers are outside the totals.
 * Cells retain their allocator pointer and layout, including through reuse. */
#include <stdint.h>
#include <stdlib.h>
#include <errno.h>
#ifdef _WIN32
#include <io.h>
#else
#include <unistd.h>
#endif
#include "memory_shim.h"

/* Fatal size/OOM errors must not allocate or run user cleanup. Messages are
 * small static ASCII ranges supplied by the runtime, never format strings. */
_Noreturn void plum_memory_fatal(const char *message, int64_t length) {
    while (length > 0) {
#ifdef _WIN32
        int written = _write(2, message, (unsigned int)length);
#else
        ssize_t written = write(2, message, (size_t)length);
#endif
        if (written < 0 && errno == EINTR) continue;
        if (written <= 0) break;
        message += written;
        length -= written;
    }
    _Exit(1);
}

#ifdef PLUM_HEAP_STATS
#include <limits.h>
#include <pthread.h>
#include <stdio.h>

/* Complemented keys keep the diagnostic index from becoming an accidental
 * conservative LeakSanitizer root for otherwise unreachable Plum cells. */
struct entry { uintptr_t key; int64_t size; int origin; };
struct totals {
    int64_t allocations, allocated_bytes, live_allocations, live_bytes;
    int64_t peak_allocations, peak_bytes;
};
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static struct entry *entries;
static size_t capacity, occupied, live;
static struct totals totals;
static struct totals origins[PLUM_ORIGIN_COUNT];
/* A frozen copy belongs to the calling thread until its next snapshot. No
 * Plum allocation/callback occurs between freezing and reading its fields. */
static _Thread_local struct totals frozen;
static _Thread_local struct totals frozen_origins[PLUM_ORIGIN_COUNT];
static _Thread_local int frozen_valid;
static _Thread_local int frozen_profile;
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

void *plum_heap_alloc_origin(int64_t size, int64_t origin) {
    if (size < 0) fail("Plum memory diagnostics: invalid allocation size\n");
    if (origin < 0 || origin >= PLUM_ORIGIN_COUNT)
        fail("Plum memory diagnostics: invalid allocation origin\n");
    void *pointer = malloc((size_t)size);
    if (!pointer) fail("Plum memory diagnostics: allocation failed\n");
    plum_heap_adopt_origin(pointer, size, origin);
    return pointer;
}

/* Adopt only successful raw allocations. Speculative reserve failures never
 * create a cell/event; both allocators share the same locked insertion. */
void plum_heap_adopt_origin(void *pointer, int64_t size, int64_t origin) {
    if (!pointer || size < 0) fail("Plum memory diagnostics: invalid adopted allocation\n");
    if (origin < 0 || origin >= PLUM_ORIGIN_COUNT)
        fail("Plum memory diagnostics: invalid allocation origin\n");
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
    entries[i] = (struct entry){key, size, (int)origin};
    ++live;
    ++totals.allocations; totals.allocated_bytes += size;
    ++totals.live_allocations; totals.live_bytes += size;
    if (totals.live_allocations > totals.peak_allocations) totals.peak_allocations = totals.live_allocations;
    if (totals.live_bytes > totals.peak_bytes) totals.peak_bytes = totals.live_bytes;
    struct totals *row = &origins[origin];
    ++row->allocations; row->allocated_bytes += size;
    ++row->live_allocations; row->live_bytes += size;
    if (row->live_allocations > row->peak_allocations) row->peak_allocations = row->live_allocations;
    if (row->live_bytes > row->peak_bytes) row->peak_bytes = row->live_bytes;
    pthread_mutex_unlock(&lock);
}

void *plum_heap_alloc(int64_t size) {
    return plum_heap_alloc_origin(size, PLUM_ORIGIN_UNSPECIFIED);
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
                struct totals *row = &origins[entries[i].origin];
                --row->live_allocations; row->live_bytes -= entries[i].size;
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
    frozen_profile = 0;
    return (int64_t)(intptr_t)&frozen;
}
int64_t plum_heap_origin_snapshot(void) {
    pthread_mutex_lock(&lock);
    frozen = totals;
    for (int i = 0; i < PLUM_ORIGIN_COUNT; ++i) frozen_origins[i] = origins[i];
    pthread_mutex_unlock(&lock);
    frozen_valid = frozen_profile = 1;
    return (int64_t)(intptr_t)&frozen;
}
static int64_t read_field(const struct totals *sample, int64_t field) {
    switch (field) {
        case 0: return sample->allocations;
        case 1: return sample->allocated_bytes;
        case 2: return sample->live_allocations;
        case 3: return sample->live_bytes;
        case 4: return sample->peak_allocations;
        case 5: return sample->peak_bytes;
        default: fail("Plum memory diagnostics: invalid snapshot field\n"); return 0;
    }
}
int64_t plum_heap_snapshot_field(int64_t token, int64_t field) {
    if (!frozen_valid || token != (int64_t)(intptr_t)&frozen)
        fail("Plum memory diagnostics: snapshot belongs to another thread\n");
    return read_field(&frozen, field);
}
int64_t plum_heap_origin_field(int64_t token, int64_t origin, int64_t field) {
    if (!frozen_valid || !frozen_profile || token != (int64_t)(intptr_t)&frozen)
        fail("Plum memory diagnostics: origin snapshot belongs to another thread or was replaced\n");
    if (origin < 0 || origin >= PLUM_ORIGIN_COUNT)
        fail("Plum memory diagnostics: invalid allocation origin\n");
    return read_field(&frozen_origins[origin], field);
}
#else
/* Ordinary builds contain no registry, mutex, TLS snapshot or cell metadata. */
/* Private allocation hooks deliberately have no disabled stubs: tracked IR
 * linked without the matching native flag must fail rather than undercount. */
int64_t plum_heap_snapshot(void) { return 0; }
int64_t plum_heap_origin_snapshot(void) { return 0; }
int64_t plum_heap_origin_field(int64_t token, int64_t origin, int64_t field) {
    (void)token; (void)origin; (void)field; return 0;
}
int64_t plum_heap_snapshot_field(int64_t token, int64_t field) {
    (void)token; (void)field; return 0;
}
#endif
