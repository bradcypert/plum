#include <assert.h>
#include <pthread.h>
#include <stdatomic.h>
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t ready = PTHREAD_COND_INITIALIZER;
static int entered;
static atomic_int dropped[1024];
void registry_barrier(void) {
    pthread_mutex_lock(&lock);
    entered++;
    pthread_cond_broadcast(&ready);
    while (entered < 8) pthread_cond_wait(&ready, &lock);
    pthread_mutex_unlock(&lock);
}
void registry_drop(long long h) {
    assert(h >= 0 && h < 1024);
    assert(atomic_fetch_add(&dropped[h], 1) == 0);
}
void registry_verify(void) {
    for (int i = 0; i < 1024; i++) assert(atomic_load(&dropped[i]) == 1);
}
