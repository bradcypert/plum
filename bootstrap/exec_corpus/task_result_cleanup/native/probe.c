#include <assert.h>
#include <pthread.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t ready = PTHREAD_COND_INITIALIZER;
static pthread_key_t key;
static pthread_once_t once = PTHREAD_ONCE_INIT;
static int proceed, exited, drops, round_number;

// A TLS destructor runs after the shim trampoline has released the
// worker's reference, so the main thread can inspect actual completion.
static void worker_exit(void *unused) {
    (void)unused;
    pthread_mutex_lock(&lock);
    if (round_number == 0) assert(drops == 1);
    exited = 1;
    pthread_cond_broadcast(&ready);
    pthread_mutex_unlock(&lock);
}
static void make_key(void) { assert(pthread_key_create(&key, worker_exit) == 0); }
void result_gate(void) {
    assert(pthread_once(&once, make_key) == 0);
    assert(pthread_setspecific(key, (void *)1) == 0);
    pthread_mutex_lock(&lock);
    while (!proceed) pthread_cond_wait(&ready, &lock);
    pthread_mutex_unlock(&lock);
}
void result_resume(void) {
    pthread_mutex_lock(&lock);
    proceed = 1;
    pthread_cond_broadcast(&ready);
    pthread_mutex_unlock(&lock);
}
void result_wait(void) {
    pthread_mutex_lock(&lock);
    while (!exited) pthread_cond_wait(&ready, &lock);
    pthread_mutex_unlock(&lock);
}
void result_reset(void) {
    pthread_mutex_lock(&lock);
    assert(drops == 1);
    drops = proceed = exited = 0;
    round_number++;
    pthread_mutex_unlock(&lock);
}
void result_drop(long long h) {
    pthread_mutex_lock(&lock);
    assert(h == 42 && drops++ == 0);
    pthread_mutex_unlock(&lock);
}

void result_verify(void) {
    pthread_mutex_lock(&lock);
    assert(exited && drops == 1);
    pthread_mutex_unlock(&lock);
    assert(pthread_key_delete(key) == 0);
}
