/* Exercise the emitted consuming kernel, with independent byte placement. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../native_stdlib/memory_shim.h"

extern void *plum_str_pad(void *, void *, int64_t, int64_t);
extern int64_t plum_str_pad_length(int64_t, int64_t, int64_t, int64_t);
extern void plum_rc_inc(void *), plum_rel_str(void *);
extern size_t malloc_usable_size(void *);
extern int64_t plum_heap_snapshot(void), plum_heap_snapshot_field(int64_t, int64_t);
static int attempts, fail_allocation;
static int64_t request;
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "string-pad: %s at %d\n", #x, __LINE__); exit(2); } } while (0)
void *pad_test_malloc(int64_t n) {
    attempts++; request = n;
    return fail_allocation ? NULL : malloc((size_t)n);
}
static int64_t live(void) {
    int64_t t = plum_heap_snapshot();
    return t ? plum_heap_snapshot_field(t, 3) : 0;
}
static void *cell(const unsigned char *bytes, int64_t n, int64_t capacity) {
    if (capacity < n + 17) capacity = n + 17;
    void *p = malloc((size_t)capacity); CHECK(p);
    ((int64_t *)p)[0] = 1; ((int64_t *)p)[1] = n;
    memcpy((char *)p + 16, bytes, (size_t)n); ((char *)p)[16 + n] = 0;
#ifdef PLUM_HEAP_STATS
    plum_heap_adopt_origin(p, capacity, PLUM_ORIGIN_BYTES);
#endif
    return p;
}
static void equal(void *p, const unsigned char *bytes, int64_t n) {
    CHECK(((int64_t *)p)[1] == n);
    CHECK(!memcmp((char *)p + 16, bytes, (size_t)n));
    CHECK(((unsigned char *)p)[16 + n] == 0);
}
static void exercise(const unsigned char *s, int64_t n, const unsigned char *fill,
                     int64_t f, int64_t left, int64_t right, int64_t capacity, int shared) {
    int64_t baseline = live(), length = n + f * (left + right);
    unsigned char *want = malloc((size_t)length + 1); CHECK(want);
    for (int64_t i = 0; i < left; i++) memcpy(want + i * f, fill, (size_t)f);
    memcpy(want + left * f, s, (size_t)n);
    for (int64_t i = 0; i < right; i++) memcpy(want + left * f + n + i * f, fill, (size_t)f);
    want[length] = 0;
    void *input = cell(s, n, capacity), *pattern = cell(fill, f, f + 17);
    if (shared) plum_rc_inc(input);
    int reuse = left + right == 0 || (!shared && malloc_usable_size(input) >= (uint64_t)(length + 17));
    uintptr_t input_id = (uintptr_t)input;
    attempts = 0; request = 0;
    void *out = plum_str_pad(input, pattern, left, right);
    equal(out, want, length); equal(pattern, fill, f);
    CHECK(((int64_t *)pattern)[0] == 1); /* The kernel borrows fill. */
    CHECK(((uintptr_t)out == input_id) == reuse);
    CHECK(attempts == (reuse ? 0 : 1));
    if (!reuse) CHECK(request == length + 17);
#ifdef PLUM_HEAP_STATS
    int64_t original = capacity < n + 17 ? n + 17 : capacity;
    int64_t expected_live = baseline + f + 17 + (reuse ? original : length + 17 + (shared ? original : 0));
    CHECK(live() == expected_live); /* Reuse preserves original requested size. */
#endif
    if (shared) equal(input, s, n);
    plum_rel_str(pattern); plum_rel_str(out);
    if (shared) plum_rel_str(input);
    CHECK(live() == baseline); free(want);
}
static void alias(int shared) {
    int64_t baseline = live();
    void *input = cell((const unsigned char *)"ab", 2, 4096);
    if (shared) plum_rc_inc(input);
    uintptr_t input_id = (uintptr_t)input;
    attempts = 0;
    /* A native caller may borrow fill from the same owned input. It must
       copy before releasing that input, never overwrite the fill source. */
    void *out = plum_str_pad(input, input, 2, 1);
    equal(out, (const unsigned char *)"abababab", 8);
    CHECK((uintptr_t)out != input_id && attempts == 1 && request == 25);
    if (shared) { equal(input, (const unsigned char *)"ab", 2); plum_rel_str(input); }
    plum_rel_str(out); CHECK(live() == baseline);
}
static void statics(void) {
    static struct { int64_t rc, len; unsigned char text[3]; } input = {-1, 2, "ab"};
    int64_t baseline = live(); attempts = 0;
    void *out = plum_str_pad(&input, &input, 1, 2);
    equal(out, (const unsigned char *)"abababab", 8);
    equal(&input, (const unsigned char *)"ab", 2);
    CHECK(input.rc == -1 && attempts == 1); plum_rel_str(out);
    CHECK(live() == baseline);
}
static void fatal(const char *kind) {
    /* Header-only inputs make overflow/OOM-before-payload-read observable. */
    int64_t s[2] = {2, 4}, f[2] = {-1, 1}, left = 1, right = 1;
    if (!strcmp(kind, "source-negative")) s[1] = -1;
    else if (!strcmp(kind, "source-large")) s[1] = INT64_MAX - 16;
    else if (!strcmp(kind, "fill-negative")) f[1] = -1;
    else if (!strcmp(kind, "fill-large")) f[1] = INT64_MAX - 16;
    else if (!strcmp(kind, "left-negative")) left = -1;
    else if (!strcmp(kind, "right-negative")) right = -1;
    else if (!strcmp(kind, "count-overflow")) left = INT64_MAX;
    else if (!strcmp(kind, "product-overflow")) { f[1] = 3; left = INT64_MAX / 2; right = 0; }
    else if (!strcmp(kind, "header-overflow")) { s[1] = 0; left = INT64_MAX - 16; right = 0; }
    else if (!strcmp(kind, "combined-overflow")) { s[1] = INT64_MAX - 17; }
    else if (!strcmp(kind, "combined-signed-overflow")) { s[1] = INT64_MAX - 17; left = 100; right = 0; }
    else if (!strcmp(kind, "empty-fill")) f[1] = 0;
    else if (!strcmp(kind, "oom")) fail_allocation = 1;
    else exit(3);
    plum_str_pad(s, f, left, right); exit(4);
}
int main(int argc, char **argv) {
    if (argc == 2) fatal(argv[1]);
    CHECK(plum_str_pad_length(0, 1, INT64_MAX - 17, 0) == INT64_MAX - 17);
    CHECK(plum_str_pad_length(0, 2, (INT64_MAX - 17) / 2, 0) == INT64_MAX - 17);
    CHECK(plum_str_pad_length(INT64_MAX - 17, 0, 0, 0) == INT64_MAX - 17);
    const unsigned char text[] = {'a', 0, 0xc3, 0xa9, 0xf0, 0x9f, 0x99, 0x82};
    const unsigned char multi[] = {0xe6, 0x97, 0xa5};
    const unsigned char odd[] = {0x80, 0x81, 0x82, 0x83, 0x84};
    for (int shared = 0; shared < 2; shared++) {
        for (int64_t n = 0; n <= 64; n++) {
            exercise(text, sizeof text, (const unsigned char *)".", 1, n / 2, n - n / 2, 32, shared);
            exercise(text, sizeof text, multi, sizeof multi, n, 1, 4096, shared);
            exercise(text, sizeof text, odd, sizeof odd, 1, n, 32, shared);
            exercise((const unsigned char *)"", 0, (const unsigned char *)"\0", 1, n, 0, 32, shared);
        }
        exercise(text, sizeof text, multi, sizeof multi, 65536, 65537, 32, shared);
        /* A malformed one-unit fill can contain arbitrarily many continuation
           bytes. It is not bounded to UTF-8's usual four-byte scalar width. */
        unsigned char *long_fill = malloc(65536); CHECK(long_fill); memset(long_fill, 0x80, 65536);
        exercise(text, sizeof text, long_fill, 65536, 2, 1, 32, shared); free(long_fill);
    }
    alias(0); alias(1); statics();
    puts("string-pad: exact copies, unique capacity, alias/static protection, binary fills and final releases pass");
    return 0;
}
