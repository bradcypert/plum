#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
extern void *plum_alloc(int64_t);
extern void *plum_alloc_preferred(int64_t, int64_t, int64_t);
static void *lose_on_worker(void *unused) {
    (void)unused;
    volatile char *ordinary = plum_alloc(32);
    volatile char *reserved = plum_alloc_preferred(32, 80, 1);
    ordinary[8] = 7;
    reserved[8] = 9;
    return NULL;
}
void lose_tracked_cell(void) {
    /* LeakSanitizer scans live stacks conservatively. A returned allocator
     * frame can leave a stale pointer in the main thread's scanned stack.
     * Terminating this worker removes that accidental root, without disabling
     * stack/register scanning or weakening the registry's leak oracle. */
    pthread_t worker;
    if (pthread_create(&worker, NULL, lose_on_worker, NULL) ||
        pthread_join(worker, NULL)) abort();
}
