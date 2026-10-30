// Deterministic isolation and lifetime checks for the real shadow stack.
#include <assert.h>
#include <pthread.h>
#include <string.h>
#include "../native_stdlib/io_shim.c"

static pthread_mutex_t gate = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;
static int ready, proceed;

static void *worker(void *unused) {
    (void)unused;
    assert(plum_depth == 0);
    plum_frame_pop(); // Empty pops must not underflow.
    assert(plum_depth == 0);
    plum_frame_push("worker outer");
    plum_frame_push("worker inner");
    pthread_mutex_lock(&gate);
    ready = 1;
    pthread_cond_broadcast(&changed);
    while (!proceed) pthread_cond_wait(&changed, &gate);
    pthread_mutex_unlock(&gate);
    plum_trace();
    plum_frame_pop();
    plum_frame_pop();
    assert(plum_depth == 0);
    return NULL;
}

static void *overflow(void *unused) {
    (void)unused;
    assert(plum_depth == 0);
    for (int i = 0; i < PLUM_TRACE_CAP + 17; i++) plum_frame_push("deep");
    assert(plum_depth == PLUM_TRACE_CAP + 17);
    for (int i = 0; i < PLUM_TRACE_CAP; i++) {
        assert(strcmp(plum_frame_names[i], "deep") == 0);
    }
    for (int i = 0; i < PLUM_TRACE_CAP + 17; i++) plum_frame_pop();
    assert(plum_depth == 0);
    // Exit with an active frame: later threads must still start empty.
    plum_frame_push("departed");
    return NULL;
}

int main(void) {
    pthread_t thread;
    plum_trace(); // No trace before the first push.
    plum_frame_push("main");
    assert(pthread_create(&thread, NULL, worker, NULL) == 0);
    pthread_mutex_lock(&gate);
    while (!ready) pthread_cond_wait(&changed, &gate);
    assert(plum_depth == 1);
    assert(strcmp(plum_frame_names[0], "main") == 0);
    proceed = 1;
    pthread_cond_broadcast(&changed);
    pthread_mutex_unlock(&gate);
    assert(pthread_join(thread, NULL) == 0);
    plum_trace();
    for (int i = 0; i < 64; i++) {
        assert(pthread_create(&thread, NULL, overflow, NULL) == 0);
        assert(pthread_join(thread, NULL) == 0);
        assert(plum_depth == 1);
        assert(strcmp(plum_frame_names[0], "main") == 0);
    }
    plum_frame_pop();
    plum_trace();
    printf("trace native checks: isolated stacks, overflow and 64 thread exits; %zu bytes of TLS per thread\n",
           sizeof(plum_frame_names) + sizeof(plum_depth));
}
