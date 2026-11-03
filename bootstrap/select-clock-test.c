// Virtual clocks exercise the real selector without changing the host clock.
#include <assert.h>
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static long long elapsed, wall = 100000000000000LL, jump;
static int fallback, wakeups, waits, sleeps, deliver;
static clockid_t cond_clock = CLOCK_REALTIME;
static long long channel;
static int payload = 42;
static int test_clock_gettime(clockid_t id, struct timespec *ts) {
    long long value = id == CLOCK_MONOTONIC ? elapsed : wall + elapsed;
    ts->tv_sec = (time_t)(value / 1000000000LL);
    ts->tv_nsec = (long)(value % 1000000000LL);
    return 0;
}
#if defined(__linux__) || defined(_WIN32)
static int test_setclock(pthread_condattr_t *attr, clockid_t id) {
    if (fallback) return EINVAL;
    // Model capability explicitly. Some winpthreads versions reject the
    // clock; using the host result makes the requested test mode ambiguous.
    (void)attr;
    assert(id == CLOCK_MONOTONIC);
    cond_clock = id;
    return 0;
}
#endif
static int test_cond_init(pthread_cond_t *cond, const pthread_condattr_t *attr) {
    // Timed waits are virtual, so the underlying untimed condvar can keep
    // its default clock. Actual platform capability is covered by the
    // native lifetime checks and source-built select fixtures.
    (void)attr;
    return pthread_cond_init(cond, NULL);
}
long long channel_send(long long, void *);
static void event(pthread_mutex_t *lock) {
    wall += jump; // A correction while blocked, in either direction.
    if (deliver) {
        deliver = 0;
        pthread_mutex_unlock(lock);
        assert(channel_send(channel, &payload) == 1);
        pthread_mutex_lock(lock);
    }
}
static int test_timedwait(pthread_cond_t *cond, pthread_mutex_t *lock,
                          const struct timespec *deadline) {
    (void)cond;
    waits++;
    event(lock);
    long long target = deadline->tv_sec * 1000000000LL + deadline->tv_nsec;
    if (cond_clock == CLOCK_REALTIME) target -= wall;
    if (wakeups-- > 0) {
        elapsed += 3000000;
        return 0; // Spurious/unrelated wakeup must not renew the budget.
    }
    if (target > elapsed) elapsed = target;
    return ETIMEDOUT;
}
static int test_nanosleep(const struct timespec *delay, struct timespec *remaining) {
    (void)remaining;
    assert(delay->tv_sec == 0 && delay->tv_nsec <= 1000000);
    sleeps++;
    wall += jump;
    // Alternate interrupted and completed sleeps. The caller must recheck
    // monotonic time after EINTR rather than restarting its whole budget.
    elapsed += sleeps % 2 ? delay->tv_nsec / 2 : delay->tv_nsec;
    if (deliver) {
        deliver = 0;
        assert(channel_send(channel, &payload) == 1);
    }
    errno = EINTR;
    return sleeps % 2 ? -1 : 0;
}
#define clock_gettime test_clock_gettime
#if defined(__linux__) || defined(_WIN32)
#define pthread_condattr_setclock test_setclock
#endif
#define pthread_cond_init test_cond_init
#define pthread_cond_timedwait test_timedwait
#define nanosleep test_nanosleep
#include "../native_stdlib/thread_shim.c"
#undef clock_gettime
#undef pthread_condattr_setclock
#undef pthread_cond_init
#undef pthread_cond_timedwait
#undef nanosleep

int main(int argc, char **argv) {
    assert(argc == 3);
    fallback = atoi(argv[1]);
#if !defined(__linux__) && !defined(_WIN32)
    fallback = 1;
#endif
    jump = atoll(argv[2]) * 3600000000000LL;
    elapsed = 999500000; // Cross a seconds boundary during the wait.
    long long start = elapsed;
    channel = channel_new();
    assert(channel);
    void *out = (void *)1;
    wakeups = 3;
    assert(channel_select_timeout(&channel, 1, &out, 20000000) == -1);
    assert(out == NULL);
    assert(elapsed == start + 20000000);
    assert(fallback ? sleeps > 0 && waits == 0 : waits == 4 && sleeps == 0);
    long long before = elapsed;
    assert(channel_select_timeout(&channel, 1, &out, -1) == -1);
    assert(elapsed == before);
    assert(channel_send(channel, &payload) == 1);
    assert(channel_select_timeout(&channel, 1, &out, 0) == 0);
    assert(out == &payload && elapsed == before);
    // Delivery during the wait, including the final timeout sweep.
    deliver = 1;
    assert(channel_select_timeout(&channel, 1, &out, 20000000) == 0);
    assert(out == &payload);
    channel_release(channel, 0, NULL);
    assert(channel_select_timeout(&channel, 1, &out, 30000000000LL) == 0);
    assert(out == NULL);
    channel_release(channel, 1, NULL);
    puts("select clock: elapsed budget, clock jumps, wakeups, delivery and EOF: ok");
}
