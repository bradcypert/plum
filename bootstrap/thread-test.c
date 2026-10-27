// Fault injection for the real shim. Also linked to generated Plum code
// by thread-smoke to verify compiler/shim ownership at the ABI boundary.
#include <assert.h>
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static int fault, allocation, expect_closure, closure_releases;
static atomic_int live, locks, conditions, drops;
static void *shim_malloc(size_t size) {
    if ((fault == 1 && ++allocation == 1) ||
        (fault == 2 && ++allocation == 2)) return NULL;
    void *p = malloc(size);
    if (p) atomic_fetch_add(&live, 1);
    return p;
}
static void shim_free(void *p) {
    if (p) {
        free(p);
        atomic_fetch_sub(&live, 1);
    }
}
static int mutex_init(pthread_mutex_t *m, const pthread_mutexattr_t *a) {
    if (fault == 3) return EAGAIN;
    int r = pthread_mutex_init(m, a);
    if (!r) atomic_fetch_add(&locks, 1);
    return r;
}
static int mutex_destroy(pthread_mutex_t *m) {
    assert(atomic_fetch_sub(&locks, 1) > 0);
    return pthread_mutex_destroy(m);
}
static int cond_init(pthread_cond_t *c, const pthread_condattr_t *a) {
    if (fault == 4) return EAGAIN;
    int r = pthread_cond_init(c, a);
    if (!r) atomic_fetch_add(&conditions, 1);
    return r;
}
static int cond_destroy(pthread_cond_t *c) {
    assert(atomic_fetch_sub(&conditions, 1) > 0);
    return pthread_cond_destroy(c);
}
static int create(pthread_t *id, const pthread_attr_t *a,
                  void *(*fn)(void *), void *arg) {
    return fault == 5 ? EAGAIN : pthread_create(id, a, fn, arg);
}
#define malloc shim_malloc
#define free shim_free
#define pthread_mutex_init mutex_init
#define pthread_mutex_destroy mutex_destroy
#define pthread_cond_init cond_init
#define pthread_cond_destroy cond_destroy
#define pthread_create create
#include "../native_stdlib/thread_shim.c"
#undef malloc
#undef free
#undef pthread_mutex_init
#undef pthread_mutex_destroy
#undef pthread_cond_init
#undef pthread_cond_destroy
#undef pthread_create

// Only the generated IR's malloc calls are redirected here. Failure 6
// targets a send's payload box independently of the shim's queue node.
void *probe_malloc(size_t size) {
    if (fault == 6 && size == 8) return NULL;
    return malloc(size);
}
void probe_drop(long long h) {
    assert(h == 42);
    assert(atomic_fetch_add(&drops, 1) == 0);
}
static void verify_exit(void) {
    assert(atomic_load(&drops) == 1);
    if (expect_closure) assert(closure_releases == 1);
    assert(atomic_load(&live) == 0);
    assert(atomic_load(&locks) == 0);
    assert(atomic_load(&conditions) == 0);
}
void probe_closure_release(void) { closure_releases++; }
void probe_arm(long long mode) {
    expect_closure = mode >= 10;
    fault = (int)(mode % 10);
    allocation = 0;
    assert(atexit(verify_exit) == 0);
}

#ifdef THREAD_NATIVE_TEST
static pthread_mutex_t gate = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t ready = PTHREAD_COND_INITIALIZER;
static int proceed, complete;
static void result_release(void *p) {
    assert(*(int *)p == 42);
    free(p);
    atomic_fetch_add(&drops, 1);
}
static void worker(void *arg, void *box) {
    (void)arg;
    pthread_mutex_lock(&gate);
    while (!proceed) pthread_cond_wait(&ready, &gate);
    pthread_mutex_unlock(&gate);
    int *result = malloc(sizeof(int));
    assert(result);
    *result = 42;
    *(void **)box = result;
}
// Wait for all shim storage to be freed, not just the destructor call.
static void wait_released(void) {
    for (int i = 0; i < 10000 && atomic_load(&live); i++) {
        struct timespec delay = {0, 1000000};
        nanosleep(&delay, NULL);
    }
    assert(atomic_load(&live) == 0);
    assert(atomic_load(&locks) == 0);
}
int main(void) {
    for (int f = 1; f <= 5; f++) {
        if (f == 4) continue;
        fault = f;
        allocation = 0;
        assert(thread_spawn(worker, NULL, result_release) == 0);
        assert(atomic_load(&live) == 0);
        assert(atomic_load(&locks) == 0);
    }
    for (int f = 1; f <= 4; f++) {
        if (f == 2) continue;
        fault = f;
        allocation = 0;
        assert(channel_new() == 0);
        assert(atomic_load(&live) == 0);
        assert(atomic_load(&locks) == 0);
        assert(atomic_load(&conditions) == 0);
    }
    fault = 0;
    long long ch = channel_new();
    assert(ch);
    int *value = malloc(sizeof(int));
    *value = 42;
    fault = 1;
    allocation = 0;
    assert(channel_send(ch, value) == 0);
    assert(*(int *)value == 42); // Failure leaves ownership with caller.
    assert(plum_chan_try_recv(ch) == NULL);
    fault = 0;
    assert(channel_send(ch, value) == 1);
    assert(channel_recv(ch) == value);
    free(value);
    channel_release(ch, NULL);
    channel_release(ch, NULL);
    assert(atomic_load(&live) == 0);

    // Force handle-first completion, then worker-first, then joined.
    for (int iteration = 0; iteration < 300; iteration++) {
        int order = iteration % 3;
        proceed = order != 0;
        atomic_store(&drops, 0);
        long long task = thread_spawn(worker, NULL, result_release);
        assert(task);
        if (order == 0) {
            task_release(task);
            pthread_mutex_lock(&gate);
            proceed = 1;
            pthread_cond_broadcast(&ready);
            pthread_mutex_unlock(&gate);
        } else if (order == 1) {
            // Hold the handle while waiting for the worker's reference.
            plum_task *t = (plum_task *)(intptr_t)task;
            for (;;) {
                pthread_mutex_lock(&t->lock);
                complete = t->refs == 1;
                pthread_mutex_unlock(&t->lock);
                if (complete) break;
            }
            task_release(task);
        } else {
            void *box = thread_join(task);
            assert(atomic_load(&drops) == 0);
            result_release(*(void **)box);
            shim_free(box);
            task_release(task);
        }
        wait_released();
        assert(atomic_load(&drops) == 1);
    }
    printf("thread native checks: 300 task lifetimes balanced; task record %zu bytes, result box 8 bytes\n", sizeof(plum_task));
}
#endif
