// Hold borrowed results across other threads' native calls before copying.
// This checks the lifetime that the Plum FFI's immediate copy relies on.
#include <assert.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#if defined(__linux__)
#include <unistd.h>
static int link_mode;
static ssize_t test_readlink(const char *path, char *buf, size_t size) {
    if (!link_mode) return readlink(path, buf, size);
    if (link_mode < 0) return -1;
    size_t n = link_mode == 1 ? size - 1 : size;
    memset(buf, 'x', n);
    return (ssize_t)n;
}
#define readlink test_readlink
#endif

// Path scratch must never turn into a per-call allocation without a matching
// ownership change at the FFI. Reject direct shim allocations in these tests.
static void *unexpected_malloc(size_t size) {
    (void)size;
    fputs("unexpected native path allocation\n", stderr);
    abort();
}
#define malloc unexpected_malloc
#include "../native_stdlib/os_shim.c"
#undef malloc
#if defined(__linux__)
#undef readlink
#endif

enum { WORKERS = 8, ROUNDS = 32 };
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t ready = PTHREAD_COND_INITIALIZER;
static unsigned arrived, generation;
static const char *borrowed[WORKERS];
static char copies[WORKERS][PLUM_PATH_MAX];
static char expected_exe[PLUM_PATH_MAX], expected_cwd[PLUM_PATH_MAX];

// pthread_barrier_t is unavailable on macOS.
static void rendezvous(void) {
    assert(pthread_mutex_lock(&lock) == 0);
    unsigned previous = generation;
    if (++arrived == WORKERS) {
        arrived = 0;
        generation++;
        assert(pthread_cond_broadcast(&ready) == 0);
    } else {
        while (generation == previous)
            assert(pthread_cond_wait(&ready, &lock) == 0);
    }
    assert(pthread_mutex_unlock(&lock) == 0);
}

static void *worker(void *arg) {
    size_t id = *(size_t *)arg;
    for (int round = 0; round < ROUNDS; round++) {
        for (int phase = 0; phase < 4; phase++) {
            int kind = phase == 3 ? (int)(id % 3) : phase;
            const char *p = kind == 0 ? os_temp_dir()
                          : kind == 1 ? os_self_exe() : os_cwd();
            borrowed[id] = p;
            rendezvous();
            // Every other thread has now returned from its call. Their
            // calls must not have overwritten this thread's borrowed result.
            assert(p != NULL && p[0] != '\0');
            for (size_t other = 0; other < WORKERS; other++)
                if (other != id) assert(p != borrowed[other]);
            strcpy(copies[id], p);
            if (kind == 1) assert(strcmp(p, expected_exe) == 0);
            if (kind == 2) assert(strcmp(p, expected_cwd) == 0);
            rendezvous();
            if (kind == 0) {
                struct stat st;
                assert(stat(p, &st) == 0 && S_ISDIR(st.st_mode));
                for (size_t other = 0; other < WORKERS; other++) {
                    int other_kind = phase == 3 ? (int)(other % 3) : phase;
                    if (other != id && other_kind == 0)
                        assert(strcmp(p, copies[other]) != 0);
                }
                assert(PLUM_RMDIR(p) == 0);
            }
            rendezvous();
        }
    }
    return NULL;
}

int main(void) {
#if defined(__linux__)
    link_mode = -1;
    assert(os_self_exe()[0] == '\0');
    link_mode = 2;
    assert(os_self_exe()[0] == '\0'); // full capacity: possibly truncated
    link_mode = 1;
    const char *p = os_self_exe();
    assert(strlen(p) == PLUM_PATH_MAX - 2);
    assert(p[PLUM_PATH_MAX - 3] == 'x');
    link_mode = 0;
#endif
    strcpy(expected_exe, os_self_exe());
    strcpy(expected_cwd, os_cwd());
    assert(expected_exe[0] && expected_cwd[0]);
    // cwd has its own buffer: querying it does not invalidate self_exe.
    const char *exe = os_self_exe();
    assert(strcmp(os_cwd(), expected_cwd) == 0);
    assert(strcmp(exe, expected_exe) == 0);
    pthread_t threads[WORKERS];
    size_t ids[WORKERS];
    for (size_t i = 0; i < WORKERS; i++) {
        ids[i] = i;
        assert(pthread_create(&threads[i], NULL, worker, &ids[i]) == 0);
    }
    for (size_t i = 0; i < WORKERS; i++)
        assert(pthread_join(threads[i], NULL) == 0);
    assert(pthread_cond_destroy(&ready) == 0);
    assert(pthread_mutex_destroy(&lock) == 0);
    puts("native OS paths: thread-local lifetime, unique directories, no scratch allocations");
    return 0;
}
