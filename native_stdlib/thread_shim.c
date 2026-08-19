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
#include <stdlib.h>

// --- Threads ---

typedef struct {
    void *(*fn)(void *);
    void *arg;
    void *result;
    pthread_t id;
} plum_task;

static void *plum_task_trampoline(void *raw) {
    plum_task *t = (plum_task *)raw;
    t->result = t->fn(t->arg);
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
    if (pthread_create(&t->id, NULL, plum_task_trampoline, t) != 0) {
        free(t);
        return 0;
    }
    return (long long)(intptr_t)t;
}

// Joining twice would be undefined; the Plum side is what guarantees it
// happens once, because `.join()` consumes the task.
void *thread_join(long long handle) {
    plum_task *t = (plum_task *)(intptr_t)handle;
    if (!t) return NULL;
    pthread_join(t->id, NULL);
    void *r = t->result;
    free(t);
    return r;
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

typedef struct {
    pthread_mutex_t lock;
    pthread_cond_t ready;
    plum_chan_node *head;
    plum_chan_node *tail;
} plum_chan;

long long channel_new(void) {
    plum_chan *c = (plum_chan *)malloc(sizeof(plum_chan));
    if (!c) return 0;
    pthread_mutex_init(&c->lock, NULL);
    pthread_cond_init(&c->ready, NULL);
    c->head = NULL;
    c->tail = NULL;
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
