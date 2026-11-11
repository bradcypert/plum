/* Check the compiler's emitted formatters, including their allocation boundaries. */
#include <float.h>
#include <inttypes.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../native_stdlib/memory_shim.h"

#ifndef FORMAT_DESIGN
#define FORMAT_DESIGN 0 /* 0: production; 1: former heap scratch; 2: direct-result prototype */
#endif
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "primitive-format: %s at %d\n", #x, __LINE__); exit(2); } } while (0)
extern void *plum_int_to_string(int64_t), *plum_float_to_string(double);
extern void *plum_float_fixed_raw(double, int64_t), *format_test_bool(int64_t);
extern void plum_rc_inc(void *), plum_rel_str(void *);
extern int64_t plum_n_alloc, plum_b_alloc;
static struct { void *ptr; int64_t size; } active[8];
static int64_t allocations, requested, live, peak;
static int measuring;

static void *remember(void *ptr, int64_t size) {
    CHECK(ptr && size > 0);
    int found = 0;
    for (int i = 0; i < 8; i++) if (!active[i].ptr) {
        active[i].ptr = ptr; active[i].size = size; found = 1; break;
    }
    CHECK(found);
    allocations++; requested += size; live += size;
    if (live > peak) peak = live;
    return ptr;
}
static void forget(void *ptr) {
    int found = 0;
    for (int i = 0; i < 8; i++) if (active[i].ptr == ptr) {
        live -= active[i].size; active[i].ptr = NULL; found = 1; break;
    }
    CHECK(found);
}
/* These seams count requests from emitted runtime code, including untracked
   scratch. Registry indexing and allocations internal to libc are excluded. */
void *format_test_malloc(int64_t size) { return remember(malloc((size_t)size), size); }
void format_test_free(void *ptr) { forget(ptr); free(ptr); }
#ifdef PLUM_HEAP_STATS
extern void *plum_heap_alloc_origin(int64_t, int64_t);
extern void plum_heap_free(void *);
void *format_test_heap_alloc_origin(int64_t size, int64_t origin) {
    return remember(plum_heap_alloc_origin(size, origin), size);
}
void format_test_heap_free(void *ptr) { forget(ptr); plum_heap_free(ptr); }
#endif
static int64_t length(void *ptr) { return ((int64_t *)ptr)[1]; }
static char *data(void *ptr) { return (char *)ptr + 16; }
static void reset(void) { CHECK(live == 0); allocations = requested = peak = 0; }
static void same(void *ptr, const char *text) {
    CHECK(length(ptr) == (int64_t)strlen(text));
    CHECK(memcmp(data(ptr), text, (size_t)length(ptr) + 1) == 0);
}
static void record(void *ptr, const char *kind, int cap, int64_t cells, int64_t bytes) {
    int boolean = cap == 0;
    int immortal = boolean && FORMAT_DESIGN != 1;
    int64_t result = immortal ? 0 : FORMAT_DESIGN == 2 && !boolean ? cap + 16 : length(ptr) + 17;
    CHECK(live == result);
    CHECK(allocations == (immortal ? 0 : FORMAT_DESIGN == 1 && !boolean ? 2 : 1));
    CHECK(requested == result + (FORMAT_DESIGN == 1 && !boolean ? cap : 0));
    CHECK(plum_n_alloc - cells == (immortal ? 0 : 1));
    CHECK(plum_b_alloc - bytes == result);
    CHECK(((int64_t *)ptr)[0] == (immortal ? -1 : 1));
    if (measuring && kind) printf("{\"kind\":\"%s\",\"allocations\":%" PRId64
        ",\"requested_bytes\":%" PRId64 ",\"peak_requested_bytes\":%" PRId64
        ",\"result_live_bytes\":%" PRId64 ",\"plum_allocations\":%" PRId64
        ",\"plum_requested_bytes\":%" PRId64 "}\n",
        kind, allocations, requested, peak, live, plum_n_alloc - cells, plum_b_alloc - bytes);
    /* Retaining, releasing and formatting again must never free or overwrite
       a surviving result. Static cells take the same public ownership path. */
    char saved[512]; CHECK(length(ptr) < (int64_t)sizeof saved);
    memcpy(saved, data(ptr), (size_t)length(ptr) + 1);
    plum_rc_inc(ptr); plum_rel_str(ptr);
    void *other = plum_int_to_string(123456789); same(other, "123456789");
    plum_rel_str(other); same(ptr, saved); plum_rel_str(ptr);
    CHECK(live == 0);
}

static void integers(void) {
    int64_t values[] = {INT64_MIN, INT64_MIN + 1, -1000, -1, 0, 1, 1000, INT64_MAX};
    for (unsigned i = 0; i < sizeof values / sizeof *values; i++) {
        char want[32]; snprintf(want, sizeof want, "%" PRId64, values[i]); reset();
        int64_t cells = plum_n_alloc, bytes = plum_b_alloc;
        void *ptr = plum_int_to_string(values[i]); same(ptr, want);
        record(ptr, i == 0 ? "int-min" : NULL, 32, cells, bytes);
    }
}
static void floats(void) {
    double values[] = {0.0, -0.0, 0.1 + 0.2, 1.0 / 3.0, 1.0, -1.0, DBL_MAX,
        -DBL_MAX, DBL_MIN, -DBL_MIN, 0x1p-1074, -0x1p-1074, INFINITY, -INFINITY, NAN};
    for (unsigned i = 0; i < sizeof values / sizeof *values; i++) {
        reset(); int64_t cells = plum_n_alloc, bytes = plum_b_alloc;
        void *ptr = plum_float_to_string(values[i]);
        CHECK(length(ptr) > 0 && length(ptr) < 64 && data(ptr)[length(ptr)] == 0);
        if (isnan(values[i])) same(ptr, "nan");
        else {
            char *end; double back = strtod(data(ptr), &end);
            CHECK(*end == 0 && back == values[i]);
            if (values[i] == 0.0) CHECK(!!signbit(back) == !!signbit(values[i]));
        }
        if (i == 2) same(ptr, "0.30000000000000004");
        record(ptr, i == 2 ? "float-roundtrip" : NULL, 64, cells, bytes);

        /* Raw expansion deliberately remains the platform's existing formatter;
           public fixtures independently check Plum's half-even rounding. */
        char want[512]; int n = snprintf(want, sizeof want, "%.18f", values[i]);
        CHECK(n >= 0 && n < (int)sizeof want);
        reset(); cells = plum_n_alloc; bytes = plum_b_alloc;
        ptr = plum_float_fixed_raw(values[i], 18); same(ptr, want);
        record(ptr, i == 4 ? "fixed-one" : i == 6 ? "fixed-max" : NULL, 512, cells, bytes);
    }
}
static void booleans(void) {
    for (int i = 0; i < 2; i++) {
        reset(); int64_t cells = plum_n_alloc, bytes = plum_b_alloc;
        void *ptr = format_test_bool(i); same(ptr, i ? "true" : "false");
        record(ptr, i ? "bool-true" : "bool-false", 0, cells, bytes);
    }
}
int main(int argc, char **argv) {
    measuring = argc == 2 && strcmp(argv[1], "--measure") == 0;
    integers(); floats(); booleans();
    /* Repeated calls must return compact independent results at -O0 as well;
       fixed local scratch cannot accumulate with the number of conversions. */
    measuring = 0;
    for (int i = 0; i < 10000; i++) {
        void *ptr = plum_float_fixed_raw(1.5, 18); same(ptr, "1.500000000000000000");
        plum_rel_str(ptr); CHECK(live == 0);
    }
    return 0;
}
