// Native state and allocation checks complement the public API pipe tests.
// Including the shim lets us check its retained buffers, which Plum's
// allocation counters do not track. Run by bootstrap/stdin-smoke.
#include <assert.h>
#include <limits.h>
#include "../native_stdlib/io_shim.c"

static void empty_reads(void) {
    const long long maxima[] = {0, -1, LLONG_MIN};
    const long long durations[] = {0, -1, LLONG_MAX};
    for (size_t i = 0; i < sizeof(maxima) / sizeof(maxima[0]); i++) {
        for (size_t j = 0; j < sizeof(durations) / sizeof(durations[0]); j++) {
            assert(stdin_bytes_timed(maxima[i], durations[j]) == 0);
            assert(strcmp(stdin_bytes_data(), "") == 0);
        }
        assert(stdin_bytes_n(maxima[i]) == 0);
        assert(strcmp(stdin_bytes_data(), "") == 0);
    }
}

int main(void) {
    // A zero-byte operation must not even initialize the native buffers
    // or discover EOF. Repetition must not grow retained native memory.
    for (int i = 0; i < 1000; i++) empty_reads();
    assert(plum_in_buf == NULL && plum_in_cap == 0 && plum_in_len == 0);
    assert(plum_out_buf == NULL && plum_out_cap == 0 && plum_out_len == 0);
    assert(plum_in_eof == 0);

    assert(plum_grow(&plum_in_buf, &plum_in_cap, 4));
    memcpy(plum_in_buf, "a\0b\n", 4);
    plum_in_len = 4;
    assert(stdin_bytes_n(1) == 1);
    assert(strcmp(stdin_bytes_data(), "a") == 0);
    size_t in_cap = plum_in_cap, out_cap = plum_out_cap;
    empty_reads();
    assert(plum_in_len == 3 && memcmp(plum_in_buf, "\0b\n", 3) == 0);
    assert(plum_in_eof == 0 && plum_out_len == 0);
    assert(plum_in_cap == in_cap && plum_out_cap == out_cap);
    assert(stdin_line_timed(LLONG_MAX) == 2);
    assert(memcmp(stdin_line_data(), "\0b", 2) == 0);

    // Nonpositive reads are still Got(empty) after EOF is already known.
    plum_in_eof = 1;
    empty_reads();
    assert(plum_in_eof == 1 && plum_in_len == 0);
    assert(stdin_bytes_timed(1, 0) == -1);

    // A huge finite duration must stay finite; UBSan also checks both
    // public timed entry points above for signed deadline overflow.
    assert(plum_stdin_deadline(-1) == -1);
    assert(plum_stdin_deadline(LLONG_MIN) == -1);
    long long before = plum_now_ns();
    long long zero = plum_stdin_deadline(0);
    long long short_wait = plum_stdin_deadline(1000000);
    long long after = plum_now_ns();
    assert(zero >= before && zero <= after);
    assert(short_wait >= before + 1000000 && short_wait <= after + 1000000);
    assert(plum_stdin_deadline(LLONG_MAX) == LLONG_MAX);

    free(plum_in_buf);
    free(plum_out_buf);
    puts("stdin shim: empty reads preserve input without allocating; deadlines saturate");
    return 0;
}
