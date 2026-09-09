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
// --- Why this is not the refcounting problem again ---
//
// Tracking every VALUE would be ruinous. Handles are not values in that
// sense: a program holds a few files, a socket, maybe a child. The
// registry is a flat array with a linear-scan removal, which is the
// right shape at that size and would be the wrong one at a million.
//
// **Not thread-safe**, matching every other blocking primitive in this
// directory. A handle opened on one thread and dropped on another is
// not something the language offers yet.

#include <stdlib.h>

#define PLUM_HANDLE_MAX 256

typedef struct {
    void *cell;                 // the refcounted cell, used as the identity
    void (*on_drop)(long long);
    long long value;            // the raw handle to hand back to `on_drop`
} PlumTrackedHandle;

static PlumTrackedHandle plum_tracked[PLUM_HANDLE_MAX];
static int plum_tracked_n = 0;
static int plum_tracked_armed = 0;
static int plum_tracked_running = 0;

// LIFO, the same order a scope releases in: a handle opened later may
// depend on one opened earlier, so the later one goes first.
//
// `plum_tracked_running` makes this non-reentrant. A cleanup that fails
// and calls `exit` itself would otherwise re-enter here and run every
// remaining handler a second time, which turns one bad cleanup into a
// double free.
static void plum_handle_cleanup_all(void) {
    if (plum_tracked_running) return;
    plum_tracked_running = 1;
    while (plum_tracked_n > 0) {
        PlumTrackedHandle *t = &plum_tracked[--plum_tracked_n];
        if (t->on_drop != NULL) t->on_drop(t->value);
    }
    plum_tracked_running = 0;
}

// Called where a handle cell is built. `on_drop` is the function the
// `handle` declaration named.
//
// **Silently stops tracking past the cap.** Overflowing is not worth a
// crash: the program still works and still releases every handle on the
// ordinary paths, and only the panic path loses the ones past 256. A
// program holding more than 256 live handles is doing something this
// design did not anticipate, and should say so in a bug report rather
// than through an abort.
void plum_handle_track(void *cell, void (*on_drop)(long long), long long value) {
    if (!plum_tracked_armed) {
        atexit(plum_handle_cleanup_all);
        plum_tracked_armed = 1;
    }
    if (plum_tracked_n >= PLUM_HANDLE_MAX) return;
    plum_tracked[plum_tracked_n].cell = cell;
    plum_tracked[plum_tracked_n].on_drop = on_drop;
    plum_tracked[plum_tracked_n].value = value;
    plum_tracked_n++;
}

// Called when a handle is released the ordinary way, BEFORE its
// `on_drop` runs -- so a cleanup that fails and exits does not find
// itself still registered and run twice.
//
// The scan runs backwards because releases are overwhelmingly LIFO, so
// the entry being removed is usually the last one.
void plum_handle_untrack(void *cell) {
    for (int i = plum_tracked_n - 1; i >= 0; i--) {
        if (plum_tracked[i].cell == cell) {
            for (int j = i; j < plum_tracked_n - 1; j++) {
                plum_tracked[j] = plum_tracked[j + 1];
            }
            plum_tracked_n--;
            return;
        }
    }
}
