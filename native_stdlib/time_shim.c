// Sleeping, and clocks with sub-second resolution (issue #19).
//
// `Time.now()` reads libc `time()` straight from the emitted runtime and
// needs no shim, because seconds since the epoch is the one thing every
// platform spells the same way. Everything here is where they stop
// agreeing: POSIX has `clock_gettime` and `nanosleep`, Windows has
// `GetSystemTimeAsFileTime`, `QueryPerformanceCounter` and `Sleep`, and
// the differences are in units, origins and resolutions rather than in
// shape.
//
// Same ABI conventions as its neighbours: every function takes and
// returns only `long long` (Plum `Int`).
//
// **Nanoseconds throughout**, which is what `Duration` stores. A signed
// 64-bit count of nanoseconds spans about 292 years either side of its
// origin -- far more than a duration needs, and enough for a wall clock
// until 2262. Go and Rust both made this trade; the alternative,
// milliseconds, buys range nobody wants and loses precision that timing
// code does.

#include <stdint.h>

#if defined(_WIN32)
#include <windows.h>
#else
#include <time.h>
#include <errno.h>
#endif

// Wall clock: nanoseconds since the Unix epoch. Moves when the system
// clock is set, which is exactly why it must not be used for measuring
// elapsed time -- see `time_monotonic_nanos`.
long long time_now_nanos(void) {
#if defined(_WIN32)
    // A FILETIME counts 100-nanosecond ticks since 1601-01-01. The
    // constant is the number of such ticks between then and the Unix
    // epoch, and is exact rather than approximate.
    FILETIME ft;
    ULARGE_INTEGER t;
    GetSystemTimeAsFileTime(&ft);
    t.LowPart = ft.dwLowDateTime;
    t.HighPart = ft.dwHighDateTime;
    return (long long)((t.QuadPart - 116444736000000000ULL) * 100ULL);
#else
    struct timespec ts;
    if (clock_gettime(CLOCK_REALTIME, &ts) != 0) return 0;
    return (long long)ts.tv_sec * 1000000000LL + (long long)ts.tv_nsec;
#endif
}

// Monotonic: nanoseconds since an unspecified origin, which never moves
// backwards and is unaffected by the system clock being set.
//
// The origin is deliberately meaningless. Two readings are only ever
// subtracted from each other, which is what `Time.since` does and the
// only thing an `Instant` is for -- so exposing the origin would invite
// treating it as a date, which it is not.
long long time_monotonic_nanos(void) {
#if defined(_WIN32)
    // `QueryPerformanceCounter` counts in units of its own frequency, so
    // the conversion has to divide before multiplying to avoid
    // overflowing at a high frequency -- seconds and remainder
    // separately, rather than ticks * 1e9 which overflows an int64 after
    // a couple of hours at 10MHz.
    LARGE_INTEGER freq, ticks;
    if (!QueryPerformanceFrequency(&freq) || freq.QuadPart == 0) return 0;
    QueryPerformanceCounter(&ticks);
    long long secs = (long long)(ticks.QuadPart / freq.QuadPart);
    long long rem = (long long)(ticks.QuadPart % freq.QuadPart);
    return secs * 1000000000LL + (rem * 1000000000LL) / (long long)freq.QuadPart;
#else
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) return 0;
    return (long long)ts.tv_sec * 1000000000LL + (long long)ts.tv_nsec;
#endif
}

// Sleeps for at least `nanos`, and returns 0.
//
// AT LEAST, never at most: a sleep may overshoot by whatever the
// scheduler decides, and no caller can be promised otherwise. A
// non-positive duration returns immediately rather than yielding, so
// `sleep(zero)` is free.
//
// `nanosleep` is resumed across a signal with the remaining time rather
// than returning early. Without the loop, a program that installs any
// handler -- or runs under a profiler that uses timers -- gets a sleep
// that silently ends early, which is the kind of bug that only shows up
// on someone else's machine.
//
// Windows sleeps in whole MILLISECONDS, so a sub-millisecond request
// rounds up to one rather than down to zero: returning immediately from
// `sleep(micros(500))` would break a caller pacing a loop.
long long time_sleep_nanos(long long nanos) {
    if (nanos <= 0) return 0;
#if defined(_WIN32)
    {
        long long ms = nanos / 1000000LL;
        if (nanos % 1000000LL != 0) ms++;
        while (ms > 0) {
            DWORD chunk = ms > 0x7FFFFFFFLL ? 0x7FFFFFFF : (DWORD)ms;
            Sleep(chunk);
            ms -= (long long)chunk;
        }
        return 0;
    }
#else
    {
        struct timespec req;
        req.tv_sec = (time_t)(nanos / 1000000000LL);
        req.tv_nsec = (long)(nanos % 1000000000LL);
        while (nanosleep(&req, &req) != 0) {
            if (errno != EINTR) return -1;
        }
        return 0;
    }
#endif
}
