// Exercise the real registry with allocator/atexit fault injection.
#include <assert.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int fail_alloc, fail_exit;
static size_t registry_bytes, peak_bytes;
static void *test_realloc(void *p, size_t size) {
    if (fail_alloc) return NULL;
    void *next = realloc(p, size);
    if (next) {
        registry_bytes = size;
        if (size > peak_bytes) peak_bytes = size;
    }
    return next;
}
static void test_free(void *p) { free(p); registry_bytes = 0; }
static int test_atexit(void (*fn)(void)) { return fail_exit ? -1 : atexit(fn); }
#define realloc test_realloc
#define free test_free
#define atexit test_atexit
#include "../native_stdlib/handle_shim.c"
#undef realloc
#undef free
#undef atexit

#define N 4096
static int cells[N + 1];
static atomic_int drops[N + 1];
static int expected_drops;
static void drop(long long id) {
    assert(id >= 0 && id <= N);
    assert(atomic_fetch_add(&drops[id], 1) == 0);
}
static void track(int id) { plum_handle_track(&cells[id], drop, id); }
static void release(int id) { if (plum_handle_untrack(&cells[id])) drop(id); }
static void verify_exit(void) {
    int count = 0;
    for (int i = 0; i <= N; i++) count += atomic_load(&drops[i]);
    assert(count == expected_drops);
    assert(registry_bytes == 0);
}
static int lifo_next;
static void lifo_drop(long long id) { assert(id == --lifo_next); drop(id); }
static void *churn(void *arg) {
    int start = *(int *)arg;
    for (int round = 0; round < 10; round++)
        for (int i = start; i < N; i += 8) {
            // Repeated rounds use a separate counter reset only after release.
            atomic_store(&drops[i], 0);
            track(i);
            release(i);
        }
    return NULL;
}
static void *race_release(void *arg) {
    int start = *(int *)arg;
    for (int i = start; i < N; i += 8) release(i);
    return NULL;
}
static int starts[8] = {0, 1, 2, 3, 4, 5, 6, 7};
static void launch(pthread_t *threads, void *(*fn)(void *)) {
    for (int i = 0; i < 8; i++) assert(pthread_create(&threads[i], NULL, fn, &starts[i]) == 0);
}
static void join(pthread_t *threads) {
    for (int i = 0; i < 8; i++) assert(pthread_join(threads[i], NULL) == 0);
}
static void reentrant_drop(long long id) {
    drop(id);
    release((int)id); // The exit claim already owns this callback.
    release(0); // A callback may release another registered resource.
    track(2);   // Or grow an empty registry while cleanup is draining it.
    plum_handle_cleanup_all(); // Reentrant cleanup must not deadlock.
}
int main(int argc, char **argv) {
    assert(argc == 2);
    assert(atexit(verify_exit) == 0); // Runs after the registry's handler.
    if (!strcmp(argv[1], "lifo")) {
        expected_drops = N;
        lifo_next = N;
        for (int i = 0; i < N; i++) plum_handle_track(&cells[i], lifo_drop, i);
        printf("%d handles: peak registry storage %zu bytes (%zu per entry)\n",
               N, peak_bytes, sizeof(PlumTrackedHandle));
        return 0; // Real atexit drain, well beyond the old cap.
    }
    if (!strcmp(argv[1], "reuse")) {
        expected_drops = N + 1;
        for (int i = 0; i < 16; i++) track(i);
        assert(registry_bytes == 0); // Common case uses only inline storage.
        for (int i = 16; i < N; i++) track(i);
        // Non-LIFO removals must preserve the remaining registrations.
        for (int i = 0; i < N; i += 2) release(i);
        for (int i = 1; i < N - 1; i += 2) release(i);
        assert(registry_bytes <= 16 * sizeof(PlumTrackedHandle));
        track(N);
        release(N - 1);
        release(N);
        assert(registry_bytes == 0);
        return 0;
    }
    if (!strcmp(argv[1], "threads")) {
        pthread_t threads[8];
        launch(threads, churn);
        join(threads);
        assert(registry_bytes == 0);
        for (int i = 0; i < N; i++) { atomic_store(&drops[i], 0); track(i); }
        expected_drops = N;
        launch(threads, race_release);
        plum_handle_cleanup_all(); // Compete with ordinary final releases.
        join(threads);
        verify_exit();
        return 0;
    }
    if (!strcmp(argv[1], "reentrant")) {
        expected_drops = 3;
        track(0);
        plum_handle_track(&cells[1], reentrant_drop, 1);
        return 0;
    }
    if (!strcmp(argv[1], "shrink-failure")) {
        expected_drops = N;
        for (int i = 0; i < N; i++) track(i);
        fail_alloc = 1;
        for (int i = 0; i < N - 1; i++) release(i);
        assert(registry_bytes == N * sizeof(PlumTrackedHandle));
        release(N - 1);
        assert(registry_bytes == 0); // Reclaim even if optional shrink failed.
        return 0;
    }
    if (!strcmp(argv[1], "oom") || !strcmp(argv[1], "oom-first")) {
        int n = !strcmp(argv[1], "oom") ? 32 : 16;
        for (int i = 0; i < n; i++) track(i);
        expected_drops = n + 1;
        fail_alloc = 1;
        track(n); // Must drop the new resource AND all older ones, then exit 1.
    } else if (!strcmp(argv[1], "atexit")) {
        expected_drops = 1;
        fail_exit = 1;
        track(0);
    } else if (!strcmp(argv[1], "closed")) {
        expected_drops = 2;
        track(0);
        plum_handle_cleanup_all();
        track(1); // Never publish a handle after the exit drain has finished.
    }
    abort(); // Failure branches must not return.
}
