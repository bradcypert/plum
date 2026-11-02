// Run the owned-result protocol, native allocation lifetimes and contention.
// Included by stdin-smoke; Windows also runs this with a real console probe.
#undef NDEBUG
#include <assert.h>
#include <limits.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <pthread.h>

static atomic_int live_blocks, allocation_calls, fail_next;
static void *tracked_malloc(size_t n) {
    atomic_fetch_add(&allocation_calls, 1);
    if (atomic_exchange(&fail_next, 0)) return NULL;
    void *p = malloc(n);
    if (p) atomic_fetch_add(&live_blocks, 1);
    return p;
}
static void *tracked_realloc(void *p, size_t n) {
    atomic_fetch_add(&allocation_calls, 1);
    if (atomic_exchange(&fail_next, 0)) return NULL;
    int fresh = p == NULL;
    void *q = realloc(p, n);
    if (q && fresh) atomic_fetch_add(&live_blocks, 1);
    return q;
}
static void tracked_free(void *p) {
    if (p) assert(atomic_fetch_sub(&live_blocks, 1) > 0);
    free(p);
}
#define malloc tracked_malloc
#define realloc tracked_realloc
#define free tracked_free
#include "../native_stdlib/io_shim.c"
#undef malloc
#undef realloc
#undef free

static void seed(const void *bytes, size_t len) {
    assert(plum_grow(&plum_stdin.bytes, &plum_stdin.cap, len));
    memcpy(plum_stdin.bytes, bytes, len);
    plum_stdin.len = len;
    plum_stdin.eof = 0;
}
static void expect(long long result, const void *bytes, size_t len) {
    assert(result > 0);
    assert(stdin_result_len(result) == (long long)len);
    assert(memcmp(stdin_result_data(result), bytes, len) == 0);
}
static void empty_reads(void) {
    const long long maxima[] = {0, -1, LLONG_MIN};
    const long long durations[] = {0, -1, LLONG_MAX};
    int before = atomic_load(&allocation_calls);
    for (int repeat = 0; repeat < 1000; repeat++) {
        for (size_t i = 0; i < sizeof(maxima) / sizeof(maxima[0]); i++) {
            for (size_t j = 0; j < sizeof(durations) / sizeof(durations[0]); j++) {
                assert(stdin_read_owned(maxima[i], durations[j], 0) == 0);
                assert(stdin_result_len(0) == 0);
                assert(strcmp(stdin_result_data(0), "") == 0);
                stdin_result_release(0);
            }
        }
    }
    assert(atomic_load(&allocation_calls) == before);
}

static atomic_int lock_held, release_lock;
static void *hold_reader(void *unused) {
    (void)unused;
    assert(plum_stdin_lock(&plum_stdin, -1));
    atomic_store(&lock_held, 1);
    while (!atomic_load(&release_lock)) plum_stdin_pause();
    atomic_flag_clear_explicit(&plum_stdin.lock, memory_order_release);
    return NULL;
}
static void contention(void) {
    pthread_t holder;
    assert(pthread_create(&holder, NULL, hold_reader, NULL) == 0);
    while (!atomic_load(&lock_held)) plum_stdin_pause();
    long long start = plum_now_ns();
    assert(stdin_read_owned(1, 20000000, 0) == PLUM_STDIN_TIMED_OUT);
    assert(plum_now_ns() - start < 1000000000LL);
    empty_reads(); // even an untimed empty read must bypass the held lock
    atomic_store(&release_lock, 1);
    assert(pthread_join(holder, NULL) == 0);
}

#define READERS 8
#define LINES 128
static atomic_int lines_read;
static void *read_lines(void *unused) {
    (void)unused;
    for (;;) {
        long long result = stdin_read_owned(0, -1, 1);
        if (result == PLUM_STDIN_EOF) return NULL;
        expect(result, "line", 4);
        // Keep the pointer alive while other readers advance the stream.
        plum_stdin_pause();
        expect(result, "line", 4);
        stdin_result_release(result);
        atomic_fetch_add(&lines_read, 1);
    }
}

int main(void) {
    empty_reads();
    assert(plum_stdin.bytes == NULL && plum_stdin.cap == 0 && plum_stdin.len == 0);
    assert(plum_stdin.eof == 0 && atomic_load(&live_blocks) == 0);
    seed("a\0b\n", 4);
    long long first = stdin_read_owned(1, -1, 0);
    expect(first, "a", 1);
    size_t capacity = plum_stdin.cap;
    empty_reads();
    assert(plum_stdin.len == 3 && memcmp(plum_stdin.bytes, "\0b\n", 3) == 0);
    assert(plum_stdin.cap == capacity);
    long long second = stdin_read_owned(0, LLONG_MAX, 1);
    expect(second, "\0b", 2);
    expect(first, "a", 1);
    stdin_result_release(second);
    stdin_result_release(first);
    assert(atomic_load(&live_blocks) == 1); // only the reusable reader buffer

    // Failure to allocate a result must leave unread data intact.
    seed("retry\n", 6);
    atomic_store(&fail_next, 1);
    assert(stdin_read_owned(0, -1, 1) == PLUM_STDIN_ERROR);
    assert(plum_stdin.len == 6);
    second = stdin_read_owned(0, -1, 1);
    expect(second, "retry", 5);
    stdin_result_release(second);
    plum_stdin.eof = 1;
    empty_reads();
    assert(stdin_read_owned(1, 0, 0) == PLUM_STDIN_EOF);

    assert(plum_stdin_deadline(-1) == -1);
    assert(plum_stdin_deadline(LLONG_MIN) == -1);
    long long before = plum_now_ns();
    long long zero = plum_stdin_deadline(0);
    long long short_wait = plum_stdin_deadline(1000000);
    long long after = plum_now_ns();
    assert(zero >= before && zero <= after);
    assert(short_wait >= before + 1000000 && short_wait <= after + 1000000);
    assert(plum_stdin_deadline(LLONG_MAX) == LLONG_MAX);
    contention();

    char lines[LINES * 5];
    for (int i = 0; i < LINES; i++) memcpy(lines + i * 5, "line\n", 5);
    seed(lines, sizeof(lines));
    plum_stdin.eof = 1;
    pthread_t readers[READERS];
    for (int i = 0; i < READERS; i++) assert(pthread_create(&readers[i], NULL, read_lines, NULL) == 0);
    for (int i = 0; i < READERS; i++) assert(pthread_join(readers[i], NULL) == 0);
    assert(atomic_load(&lines_read) == LINES && plum_stdin.len == 0);
    assert(atomic_load(&live_blocks) == 1);

    tracked_free(plum_stdin.bytes);
    plum_stdin.bytes = NULL;
    assert(atomic_load(&live_blocks) == 0);
    puts("stdin reader: owned snapshots, concurrent reads, bounded contention, allocation cleanup: ok");
}
