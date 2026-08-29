// Threads and channels for the SELF-HOSTED backend.
//
// The real backend emits its own pthread calls and a channel queue
// directly as LLVM IR (`plum_codegen::emit_channel_runtime`). The
// self-hosted backend reaches the same primitives through this shim
// instead, for the same reason it reaches directories and processes
// through `dir_shim.c`/`process_shim.c`: writing a mutex/condvar queue
// by hand in LLVM IR is a great deal of fiddly text to get right, and
// none of it is Plum-specific.
//
// Handles are opaque `long long`s rather than pointers so the Plum side
// can hold them in an ordinary `Int`-shaped slot with no lifetime of
// its own. The values happen to BE pointers; nothing on the Plum side
// may assume that.
//
// Values crossing a channel are `void *`. What they point at is the
// caller's business -- the self-hosted backend boxes each sent value
// into one malloc'd word, because a channel is typed on the Plum side
// and the box's contents are therefore always the same shape for a
// given channel.

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

// --- Threads ---

// `refs` is 2 at birth: one for the running thread, one for the Plum
// `Task` cell. Whichever lets go LAST frees the struct, which is what
// makes "the task outlives the handle" and "the handle outlives the
// task" both work without either side knowing which happened.
//
// `joined` says `.join()` has already taken the result. It is what
// turns a second join from undefined behaviour into a clean abort, now
// that a refcounted `Task` cell can be copied and joined twice.
typedef struct {
    void *(*fn)(void *);
    void *arg;
    void *result;
    pthread_t id;
    pthread_mutex_t lock;
    int refs;
    int joined;
    int detached;
} plum_task;

// Drops one reference and frees at zero. The caller must NOT hold
// `t->lock`.
static void plum_task_unref(plum_task *t, void (*rel)(void *)) {
    pthread_mutex_lock(&t->lock);
    t->refs--;
    int dead = (t->refs == 0);
    pthread_mutex_unlock(&t->lock);
    if (!dead) return;
    // Nobody joined, so the boxed result is nobody's -- release what it
    // holds and free the box, or it leaks exactly the way the whole
    // change is meant to stop.
    if (t->result && !t->joined) {
        if (rel) rel(*(void **)t->result);
        free(t->result);
    }
    pthread_mutex_destroy(&t->lock);
    free(t);
}

static void *plum_task_trampoline(void *raw) {
    plum_task *t = (plum_task *)raw;
    t->result = t->fn(t->arg);
    // The thread's own reference. The release function is null here on
    // purpose: a thread that finishes while the handle is still alive
    // must leave the result for `.join()`, and this drop can only reach
    // zero when the handle is already gone -- in which case the handle
    // side passed the real one.
    plum_task_unref(t, NULL);
    return NULL;
}

// Returns 0 on failure, which the caller treats as a spawn that never
// ran -- there is no error channel back into Plum here.
long long thread_spawn(void *(*fn)(void *), void *arg) {
    plum_task *t = (plum_task *)malloc(sizeof(plum_task));
    if (!t) return 0;
    t->fn = fn;
    t->arg = arg;
    t->result = NULL;
    pthread_mutex_init(&t->lock, NULL);
    t->refs = 2;
    t->joined = 0;
    t->detached = 0;
    if (pthread_create(&t->id, NULL, plum_task_trampoline, t) != 0) {
        pthread_mutex_destroy(&t->lock);
        free(t);
        return 0;
    }
    return (long long)(intptr_t)t;
}

// A second join is an ERROR, not undefined. It used to be prevented by
// `.join()` consuming the task; a refcounted `Task` cell can be copied,
// so the guarantee moved here where it can actually be enforced.
//
// The struct is NOT freed here -- the Plum cell still holds a
// reference, and `task_release` frees it when that goes.
void *thread_join(long long handle) {
    plum_task *t = (plum_task *)(intptr_t)handle;
    if (!t) return NULL;
    pthread_mutex_lock(&t->lock);
    if (t->joined) {
        pthread_mutex_unlock(&t->lock);
        // Same shape as the runtime's own `@plum_bounds_fail`: say
        // what happened on stdout and stop. There is no error channel
        // back into Plum from a shim.
        printf("panic: task already joined\n");
        exit(1);
        return NULL;
    }
    t->joined = 1;
    pthread_mutex_unlock(&t->lock);
    pthread_join(t->id, NULL);
    return t->result;
}

// The Plum `Task` cell's last reference is gone.
//
// If nobody joined, the thread is DETACHED rather than waited for: a
// scope exit that blocked for an unbounded time with nothing at the
// call site saying so would be worse than the leak this replaces. The
// body keeps running and its result is dropped.
void task_release(long long handle, void (*rel)(void *)) {
    plum_task *t = (plum_task *)(intptr_t)handle;
    if (!t) return;
    pthread_mutex_lock(&t->lock);
    int detach = (!t->joined && !t->detached);
    if (detach) t->detached = 1;
    pthread_mutex_unlock(&t->lock);
    if (detach) pthread_detach(t->id);
    plum_task_unref(t, rel);
}

// --- Channels ---
//
// An unbounded queue with a mutex and a condition variable. Unbounded
// because a bounded one needs a second condvar and a policy for what a
// full channel does to a sender, and neither compiler's surface syntax
// exposes a capacity to choose one with.

typedef struct plum_chan_node {
    void *value;
    struct plum_chan_node *next;
} plum_chan_node;

// `refs` is 2 at birth, one for each end's Plum cell. The queue
// outlives whichever end is dropped first and is freed when the second
// goes -- which is the whole reason the two ends are separate cells
// over one handle rather than one cell shared.
typedef struct {
    pthread_mutex_t lock;
    pthread_cond_t ready;
    plum_chan_node *head;
    plum_chan_node *tail;
    int refs;
} plum_chan;

static pthread_mutex_t plum_sel_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t plum_sel_ready = PTHREAD_COND_INITIALIZER;

long long channel_new(void) {
    plum_chan *c = (plum_chan *)malloc(sizeof(plum_chan));
    if (!c) return 0;
    pthread_mutex_init(&c->lock, NULL);
    pthread_cond_init(&c->ready, NULL);
    c->head = NULL;
    c->tail = NULL;
    c->refs = 2;
    return (long long)(intptr_t)c;
}

void channel_send(long long handle, void *value) {
    plum_chan *c = (plum_chan *)(intptr_t)handle;
    if (!c) return;
    plum_chan_node *n = (plum_chan_node *)malloc(sizeof(plum_chan_node));
    if (!n) return;
    n->value = value;
    n->next = NULL;
    pthread_mutex_lock(&c->lock);
    if (c->tail) c->tail->next = n; else c->head = n;
    c->tail = n;
    // Signalled while the lock is HELD: a receiver woken before the
    // queue pointers were published would find an empty queue and, with
    // the predicate loop below, simply wait again -- correct but
    // wasteful. Holding it also keeps the wakeup ordered with the
    // enqueue for anyone reasoning about this later.
    pthread_cond_signal(&c->ready);
    pthread_mutex_unlock(&c->lock);
    // Woken AFTER this channel's lock is dropped, which is what keeps
    // the lock order one-way -- see `plum_sel_lock`. Broadcast rather
    // than signal because the waiters are selecting on different sets
    // of channels, so there is no such thing as "the right one" to
    // wake; each rechecks its own arms.
    pthread_mutex_lock(&plum_sel_lock);
    pthread_cond_broadcast(&plum_sel_ready);
    pthread_mutex_unlock(&plum_sel_lock);
}

// --- Selecting over several channels ---
//
// A channel's own condvar cannot express "wait until ANY of these has
// something", so there is one process-wide condvar that every send
// broadcasts on, and a `select` that finds nothing sleeps on it.
//
// The cost is one uncontended mutex per send, paid even by programs
// that never select. The alternative that avoids it -- registering a
// waiter on each channel -- is the only version with cross-channel lock
// ordering to get wrong, and this queue is unbounded and mutex-based
// already; it is not the place to spend that risk.
//
// LOCK ORDER is `sel_lock` then a channel's `lock`, and only ever that
// way round. `channel_select` holds `sel_lock` across its sweep, and
// the sweep takes each channel lock in turn. `channel_send` takes its
// channel lock and RELEASES it before taking `sel_lock`, so it never
// holds both in the other order and there is no cycle.
// Non-blocking: the queue's head, or NULL when there is nothing. NULL
// is unambiguous because a sent value is always a `malloc`'d box and
// so never null itself.
static void *plum_chan_try_recv(long long handle) {
    plum_chan *c = (plum_chan *)(intptr_t)handle;
    if (!c) return NULL;
    pthread_mutex_lock(&c->lock);
    plum_chan_node *n = c->head;
    if (n) {
        c->head = n->next;
        if (!c->head) c->tail = NULL;
    }
    pthread_mutex_unlock(&c->lock);
    if (!n) return NULL;
    void *v = n->value;
    free(n);
    return v;
}

// Blocks until one of `handles` has a value. Returns the INDEX of the
// arm that won and stores its boxed value through `out`.
//
// The whole loop is here rather than in generated code because every
// payload crosses as a `void *` box, so C can do all of it, and because
// the sweep has to happen with `sel_lock` HELD. That is what closes the
// missed-wakeup window: a send that arrives after the sweep looked at
// its channel cannot broadcast until this thread is already waiting.
//
// Arms are swept in index order, so an earlier arm wins a tie. That is
// a real fairness choice -- a hot arm 0 can starve arm 1 -- and it is
// the same order the retired backend used.
long long channel_select(const long long *handles, long long n, void **out) {
    if (n <= 0) return -1;
    pthread_mutex_lock(&plum_sel_lock);
    for (;;) {
        for (long long i = 0; i < n; i++) {
            void *v = plum_chan_try_recv(handles[i]);
            if (v) {
                pthread_mutex_unlock(&plum_sel_lock);
                *out = v;
                return i;
            }
        }
        pthread_cond_wait(&plum_sel_ready, &plum_sel_lock);
    }
}

// Blocks until a value is available. The `while` (not `if`) is the
// standard guard against spurious wakeups, which pthread condition
// variables are explicitly permitted to produce.
void *channel_recv(long long handle) {
    plum_chan *c = (plum_chan *)(intptr_t)handle;
    if (!c) return NULL;
    pthread_mutex_lock(&c->lock);
    while (!c->head) {
        pthread_cond_wait(&c->ready, &c->lock);
    }
    plum_chan_node *n = c->head;
    c->head = n->next;
    if (!c->head) c->tail = NULL;
    pthread_mutex_unlock(&c->lock);
    void *v = n->value;
    free(n);
    return v;
}

// One end of the channel has let go.
//
// Anything still QUEUED when the last end goes was sent and never
// received, and it holds references of its own -- so the drain
// releases each value through `rel` rather than just freeing boxes.
// `rel` is null for an element type that owns nothing.
void channel_release(long long handle, void (*rel)(void *)) {
    plum_chan *c = (plum_chan *)(intptr_t)handle;
    if (!c) return;
    pthread_mutex_lock(&c->lock);
    c->refs--;
    int dead = (c->refs == 0);
    plum_chan_node *n = dead ? c->head : NULL;
    if (dead) { c->head = NULL; c->tail = NULL; }
    pthread_mutex_unlock(&c->lock);
    if (!dead) return;
    while (n) {
        plum_chan_node *next = n->next;
        if (rel) rel(*(void **)n->value);
        free(n->value);
        free(n);
        n = next;
    }
    pthread_mutex_destroy(&c->lock);
    pthread_cond_destroy(&c->ready);
    free(c);
}
