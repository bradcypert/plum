// Cleanup for handles the program never got to release (issue #1).
//
// A `handle` runs its `on_drop` when the value dies, which covers every
// ordinary exit: a normal return, a block ending, an `Err` being
// returned. Plum has no early `return`, so those really are all the
// paths a function can take -- except one.
//
// **A panic does not unwind.** `panic_raw` prints and calls `exit`, so
// nothing between the failure and the process ending runs, and a
// `Child` outlived its parent's panic and kept working. That is not
// untidiness the operating system tidies up: the OS reclaims file
// descriptors and memory, and does not kill your grandchildren, remove
// your lock file, or take the terminal out of raw mode.
//
// So live handles are tracked, and released on the way out.
//
// --- Why `atexit` and not a hook in `plum_panic` ---
//
// Every exit this needs to cover goes through `exit()`: `panic_raw`
// does, `Os.exit_with` does, and returning from `main` does. One
// `atexit` handler therefore covers all three, and covers any future
// path that exits properly, without the panic code knowing this exists.
//
// Only declared native handles are registered, not ordinary heap values.
// The registry borrows cell identities; it never retains a cell or changes
// its reference count. Geometric growth removes the old 256-handle limit.
// Sixteen inline entries avoid heap allocation for small programs. Extra
// storage shrinks after a burst and is freed when the last entry leaves.
// Registration/removal and exit cleanup share one mutex. Drop callbacks
// run outside it: they may release or create other handles, or call exit.

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    void *cell;
    void (*on_drop)(long long);
    long long value;
} PlumTrackedHandle;

static pthread_mutex_t plum_tracked_lock = PTHREAD_MUTEX_INITIALIZER;
static PlumTrackedHandle plum_tracked_inline[16];
static PlumTrackedHandle *plum_tracked = plum_tracked_inline;
static size_t plum_tracked_n, plum_tracked_capacity = 16;
static int plum_tracked_armed, plum_tracked_running, plum_tracked_closed;

// Called with the lock held. Shrinking is optional; failure keeps the
// existing allocation and all registrations intact.
static void plum_handle_trim(void) {
    if (plum_tracked_n == 0) {
        if (plum_tracked != plum_tracked_inline) free(plum_tracked);
        plum_tracked = plum_tracked_inline;
        plum_tracked_capacity = 16;
    } else if (plum_tracked_capacity > 16 &&
               plum_tracked_n <= plum_tracked_capacity / 4) {
        size_t capacity = plum_tracked_capacity / 2;
        PlumTrackedHandle *next;
        if (capacity == 16) {
            next = plum_tracked_inline;
            memcpy(next, plum_tracked, plum_tracked_n * sizeof(*next));
            free(plum_tracked);
        } else next = realloc(plum_tracked, capacity * sizeof(*next));
        if (next) {
            plum_tracked = next;
            plum_tracked_capacity = capacity;
        }
    }
}

// LIFO among entries still registered. Claim a record under the lock,
// then use a COPY outside it, since callbacks can resize the registry.
// Ordinary release uses the same claim, so only one path calls on_drop.
static void plum_handle_cleanup_all(void) {
    pthread_mutex_lock(&plum_tracked_lock);
    if (plum_tracked_running || plum_tracked_closed) {
        pthread_mutex_unlock(&plum_tracked_lock);
        return;
    }
    plum_tracked_running = 1;
    while (plum_tracked_n > 0) {
        PlumTrackedHandle t = plum_tracked[--plum_tracked_n];
        plum_handle_trim();
        pthread_mutex_unlock(&plum_tracked_lock);
        if (t.on_drop) t.on_drop(t.value);
        pthread_mutex_lock(&plum_tracked_lock);
    }
    plum_tracked_closed = 1;
    plum_tracked_running = 0;
    pthread_mutex_unlock(&plum_tracked_lock);
}

// No error channel exists in the generated from_raw ABI. Never return an
// unregistered resource on failure: release it, report why, and exit via
// the already-armed cleanup for older registrations. No allocation is
// needed on this path. The callback runs without the registry lock.
static void plum_handle_track_fail(void (*on_drop)(long long), long long value,
                                   const char *reason) {
    fprintf(stderr, "panic: handle tracking failed: %s\n", reason);
    if (on_drop) on_drop(value);
    exit(1);
}

void plum_handle_track(void *cell, void (*on_drop)(long long), long long value) {
    const char *failure = NULL;
    pthread_mutex_lock(&plum_tracked_lock);
    if (plum_tracked_closed) failure = "process cleanup already completed";
    else if (!plum_tracked_armed) {
        if (atexit(plum_handle_cleanup_all) != 0)
            failure = "cannot register exit cleanup";
        else plum_tracked_armed = 1;
    }
    if (!failure && plum_tracked_n == plum_tracked_capacity) {
        size_t capacity = plum_tracked_capacity * 2;
        if (capacity < plum_tracked_capacity || capacity > SIZE_MAX / sizeof(*plum_tracked))
            failure = "registry size overflow";
        else {
            PlumTrackedHandle *next = realloc(
                plum_tracked == plum_tracked_inline ? NULL : plum_tracked,
                capacity * sizeof(*next));
            if (!next) failure = "out of memory";
            else {
                if (plum_tracked == plum_tracked_inline)
                    memcpy(next, plum_tracked_inline, plum_tracked_n * sizeof(*next));
                plum_tracked = next;
                plum_tracked_capacity = capacity;
            }
        }
    }
    if (!failure)
        plum_tracked[plum_tracked_n++] = (PlumTrackedHandle){cell, on_drop, value};
    pthread_mutex_unlock(&plum_tracked_lock);
    if (failure) plum_handle_track_fail(on_drop, value, failure);
}

// Return whether ordinary release owns the callback. A missing entry
// has already been claimed by exit cleanup; the caller still frees the
// refcounted cell but must not drop the underlying resource again.
int plum_handle_untrack(void *cell) {
    pthread_mutex_lock(&plum_tracked_lock);
    for (size_t i = plum_tracked_n; i > 0; i--) {
        if (plum_tracked[i - 1].cell == cell) {
            memmove(&plum_tracked[i - 1], &plum_tracked[i],
                    (plum_tracked_n - i) * sizeof(*plum_tracked));
            plum_tracked_n--;
            plum_handle_trim();
            pthread_mutex_unlock(&plum_tracked_lock);
            return 1;
        }
    }
    pthread_mutex_unlock(&plum_tracked_lock);
    return 0;
}
