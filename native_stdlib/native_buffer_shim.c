#include "native_buffer.h"
#include <limits.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    pthread_mutex_t lock;
    unsigned char *data;
    size_t length, capacity;
    unsigned refs;
    int mode, committed; /* mode -1: idle; otherwise one exclusive lease */
} PlumNativeBuffer;

#ifdef PLUM_HEAP_STATS
/* A separate ledger of this module's native payload capacity. Never charge
 * these allocations to Plum cells. Lock order: buffer, then statistics.
 * Snapshot readers take only the statistics lock; callbacks take neither. */
static pthread_mutex_t stats_lock = PTHREAD_MUTEX_INITIALIZER;
static long long totals[6];
static _Thread_local long long frozen[6];
static _Thread_local int frozen_valid;
static long long checked_add(long long a, long long b) {
    if (b > LLONG_MAX - a) {
        fputs("panic: native buffer diagnostic counter overflow\n", stderr);
        abort();
    }
    return a + b;
}
static void record_capacity(size_t old, size_t next, int create, int destroy) {
    pthread_mutex_lock(&stats_lock);
    if (!destroy) {
        totals[0] = checked_add(totals[0], 1);
        totals[1] = checked_add(totals[1], (long long)next);
    }
    if (create) totals[2] = checked_add(totals[2], 1);
    if (destroy) totals[2]--;
    totals[3] -= (long long)old;
    totals[3] = checked_add(totals[3], (long long)next);
    if (totals[2] > totals[4]) totals[4] = totals[2];
    if (totals[3] > totals[5]) totals[5] = totals[3];
    pthread_mutex_unlock(&stats_lock);
}
long long plum_buffer_snapshot(void) {
    pthread_mutex_lock(&stats_lock);
    memcpy(frozen, totals, sizeof frozen);
    pthread_mutex_unlock(&stats_lock);
    frozen_valid = 1;
    return 1;
}
long long plum_buffer_snapshot_field(long long token, long long field) {
    return token == 1 && frozen_valid && field >= 0 && field < 6 ? frozen[field] : 0;
}
#else
#define record_capacity(old, next, create, destroy) \
    ((void)(old), (void)(next), (void)(create), (void)(destroy))
long long plum_buffer_snapshot(void) { return 0; }
long long plum_buffer_snapshot_field(long long token, long long field) {
    (void)token; (void)field; return 0;
}
#endif

static PlumNativeBuffer *buffer_of(long long token) {
    return (PlumNativeBuffer *)(intptr_t)token;
}
static int valid_capacity(long long capacity) {
    return capacity >= 0 && (uint64_t)capacity <= SIZE_MAX;
}
static void destroy_buffer(PlumNativeBuffer *b) {
    record_capacity(b->capacity, 0, 0, 1);
    pthread_mutex_destroy(&b->lock);
    free(b->data);
    free(b);
}
long long plum_buffer_create(long long capacity) {
    if (!valid_capacity(capacity)) return 0;
    PlumNativeBuffer *b = malloc(sizeof *b);
    if (!b) return 0;
    /* A non-null sentinel for zero capacity; it is not writable payload. */
    b->data = calloc(capacity ? (size_t)capacity : 1, 1);
    if (!b->data) { free(b); return 0; }
    if (pthread_mutex_init(&b->lock, NULL) != 0) {
        free(b->data); free(b); return 0;
    }
    b->data[0] = 0;
    b->length = 0; b->capacity = (size_t)capacity;
    b->refs = 1; b->mode = -1; b->committed = 1;
    record_capacity(0, b->capacity, 1, 0);
    return (long long)(intptr_t)b;
}
void plum_buffer_release(long long token) {
    PlumNativeBuffer *b = buffer_of(token);
    if (!b) return;
    pthread_mutex_lock(&b->lock);
    int dead = --b->refs == 0;
    pthread_mutex_unlock(&b->lock);
    if (dead) destroy_buffer(b);
}
long long plum_buffer_length(long long token) {
    PlumNativeBuffer *b = buffer_of(token);
    pthread_mutex_lock(&b->lock);
    long long result = (long long)b->length;
    pthread_mutex_unlock(&b->lock);
    return result;
}
long long plum_buffer_capacity(long long token) {
    PlumNativeBuffer *b = buffer_of(token);
    pthread_mutex_lock(&b->lock);
    long long result = (long long)b->capacity;
    pthread_mutex_unlock(&b->lock);
    return result;
}
long long plum_buffer_reserve(long long token, long long capacity) {
    if (!valid_capacity(capacity)) return -1;
    PlumNativeBuffer *b = buffer_of(token);
    pthread_mutex_lock(&b->lock);
    int status = 0;
    if (b->mode != -1) status = -2;
    else if ((size_t)capacity > b->capacity) {
        unsigned char *next = realloc(b->data, (size_t)capacity);
        if (!next) status = -3;
        else {
            size_t old = b->capacity;
            b->data = next; b->capacity = (size_t)capacity;
            memset(b->data + old, 0, b->capacity - old);
            b->data[b->length] = 0; /* growth leaves room for this terminator */
            record_capacity(old, b->capacity, 0, 0);
        }
    }
    pthread_mutex_unlock(&b->lock);
    return status;
}
long long plum_buffer_write(long long token, const unsigned char *data, long long length) {
    if (length < 0) return -1;
    PlumNativeBuffer *b = buffer_of(token);
    pthread_mutex_lock(&b->lock);
    int status = 0;
    if (b->mode != -1) status = -2;
    else if ((uint64_t)length > b->capacity) status = -1;
    else {
        if (length) memcpy(b->data, data, (size_t)length);
        b->length = (size_t)length;
        if (b->length < b->capacity) b->data[b->length] = 0;
    }
    pthread_mutex_unlock(&b->lock);
    return status;
}
long long plum_buffer_acquire(long long token, long long mode) {
    if (mode < 0 || mode > 2) return -1;
    PlumNativeBuffer *b = buffer_of(token);
    pthread_mutex_lock(&b->lock);
    long long result = token;
    if (b->mode != -1) result = -2;
    else if (mode == 2 && (!b->capacity || !memchr(b->data, 0,
                  b->length < b->capacity ? b->length + 1 : b->capacity))) result = -1;
    else {
        b->refs++; /* the lease independently owns the native resource */
        b->mode = (int)mode;
        b->committed = mode == 0;
    }
    pthread_mutex_unlock(&b->lock);
    return result;
}
unsigned char *plum_buffer_data(long long token) {
    PlumNativeBuffer *b = buffer_of(token);
    pthread_mutex_lock(&b->lock);
    unsigned char *data = b->data;
    pthread_mutex_unlock(&b->lock);
    return data;
}
long long plum_buffer_commit(long long token, long long length) {
    PlumNativeBuffer *b = buffer_of(token);
    pthread_mutex_lock(&b->lock);
    int status = -1;
    if ((b->mode == 1 || b->mode == 2) && length >= 0 && (uint64_t)length <= b->capacity
        && (b->mode != 2 || ((size_t)length < b->capacity && b->data[length] == 0))) {
        b->length = (size_t)length;
        if (b->length < b->capacity) b->data[b->length] = 0;
        b->committed = 1; status = 0;
    }
    pthread_mutex_unlock(&b->lock);
    return status;
}
long long plum_buffer_text_length(long long token) {
    PlumNativeBuffer *b = buffer_of(token);
    pthread_mutex_lock(&b->lock);
    unsigned char *end = b->mode == 2 ? memchr(b->data, 0, b->capacity) : NULL;
    long long length = end ? (long long)(end - b->data) : -1;
    pthread_mutex_unlock(&b->lock);
    return length;
}

void plum_buffer_end(long long token) {
    PlumNativeBuffer *b = buffer_of(token);
    if (!b) return;
    pthread_mutex_lock(&b->lock);
    if (!b->committed) {
        b->length = 0;
        if (b->capacity) b->data[0] = 0;
    }
    b->mode = -1;
    int dead = --b->refs == 0;
    pthread_mutex_unlock(&b->lock);
    if (dead) destroy_buffer(b);
}
