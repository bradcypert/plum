// Exercise the actual platform entry point with a synthetic OS clock.
// Renaming the shim symbols keeps the live Time API available alongside it.
#include <assert.h>
#include <limits.h>

#if defined(_WIN32)
#include <windows.h>
static unsigned long long fake_ticks;
static void fake_wall_clock(FILETIME *ft) {
    ULARGE_INTEGER t;
    t.QuadPart = fake_ticks;
    ft->dwLowDateTime = t.LowPart;
    ft->dwHighDateTime = t.HighPart;
}
#define GetSystemTimeAsFileTime fake_wall_clock
#else
#include <time.h>
static struct timespec fake_time;
static int fake_failure;
static int fake_wall_clock(clockid_t clock, struct timespec *ts) {
    assert(clock == CLOCK_REALTIME);
    *ts = fake_time;
    return fake_failure;
}
#define clock_gettime fake_wall_clock
#endif

#define time_now_millis test_time_now_millis
#define time_monotonic_nanos test_time_monotonic_nanos
#define time_sleep_nanos test_time_sleep_nanos
#include "../../../../native_stdlib/time_shim.c"

static void check(long long seconds, long nanos, long long expected) {
#if defined(_WIN32)
    fake_ticks = 116444736000000000ULL;
    if (seconds < 0) fake_ticks -= (unsigned long long)(-seconds) * 10000000ULL;
    else fake_ticks += (unsigned long long)seconds * 10000000ULL;
    fake_ticks += (unsigned long long)nanos / 100ULL;
#else
    fake_time.tv_sec = (time_t)seconds;
    fake_time.tv_nsec = nanos;
#endif
    assert(test_time_now_millis() == expected);
}

long long time_millis_regressions(void) {
    check(0, 0, 0);
    check(0, 999999, 0);
    check(0, 1000000, 1);
    check(1, 999999999, 1999);
    check(-1, 999999900, 0);
    check(-1, 999000000, -1);
    check(-1, 998999900, -1);
    check(-1, 0, -1000);
    check(-2, 123456700, -1876);
    // Either side of the old signed nanosecond boundary in April 2262.
    check(9223372036LL, 854775800, 9223372036854LL);
    check(9223372036LL, 855000000, 9223372036855LL);
    check(9223372037LL, 0, 9223372037000LL);
    check(16725225600LL, 123456700, 16725225600123LL); // year 2500
#if defined(_WIN32)
    fake_ticks = 0; // 1601-01-01
    assert(test_time_now_millis() == -11644473600000LL);
    fake_ticks = ULLONG_MAX;
    assert(test_time_now_millis() == 1833029933770955LL);
#else
    check(-9223372037LL, 145000000, -9223372036855LL);
    // Exact final-value boundaries, including an intermediate product
    // that would underflow if negative seconds were multiplied directly.
    check(LLONG_MAX / 1000, 807000000, LLONG_MAX);
    check(LLONG_MAX / 1000, 808000000, LLONG_MAX);
    check(LLONG_MAX / 1000 + 1, 0, LLONG_MAX);
    check(LLONG_MIN / 1000 - 1, 193000000, LLONG_MIN + 1);
    check(LLONG_MIN / 1000 - 1, 192000000, LLONG_MIN);
    check(LLONG_MIN / 1000 - 1, 191000000, LLONG_MIN);
    check(LLONG_MIN, 0, LLONG_MIN);
    check(LLONG_MAX, 999999999, LLONG_MAX);
    fake_failure = -1;
    assert(test_time_now_millis() == 0);
    fake_failure = 0;
#endif
    return 1;
}

#ifdef PLUM_TIME_TEST_MAIN
int main(void) { return time_millis_regressions() == 1 ? 0 : 1; }
#endif
