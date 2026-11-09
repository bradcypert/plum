/* Exercises actual emitted runtime functions; no duplicate growth policy. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../native_stdlib/memory_shim.h"
#if defined(__APPLE__)
#include <malloc/malloc.h>
uint64_t malloc_usable_size(void *p) { return malloc_size(p); }
#elif defined(_WIN32)
#include <malloc.h>
uint64_t malloc_usable_size(void *p) { return _msize(p); }
#else
#include <malloc.h>
#endif
extern void *plum_alloc_preferred(int64_t, int64_t, int64_t);
extern int64_t plum_str_concat_length(int64_t, int64_t);
extern int64_t plum_str_growth_bytes(int64_t, int64_t);
extern void *plum_str_concat(void *, void *);
extern void *plum_str_repeat(void *, int64_t);
extern void *plum_bytes_to_hex(void *);
extern int64_t plum_str_repeat_length(int64_t, int64_t);
extern void *plum_str_concat_reuse(void *, void *);
extern void plum_rel_str(void *);
extern void plum_rc_inc(void *);
extern int64_t plum_n_alloc, plum_b_alloc;
extern int64_t plum_heap_snapshot(void);
extern int64_t plum_heap_snapshot_field(int64_t, int64_t);
extern int64_t plum_heap_origin_snapshot(void);
extern int64_t plum_heap_origin_field(int64_t, int64_t, int64_t);
static int failures, attempts;
static int64_t requests[8];
void *growth_test_malloc(int64_t size) {
    if (attempts < 8) requests[attempts] = size;
    ++attempts;
    if (failures) { --failures; return NULL; }
    return malloc((size_t)size);
}
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "String growth check failed: %s (line %d)\n", #x, __LINE__); exit(2); } } while (0)
static int64_t field(int f) { return plum_heap_snapshot_field(plum_heap_snapshot(), f); }
static int64_t origin_field(int f) {
    return plum_heap_origin_field(plum_heap_origin_snapshot(), PLUM_ORIGIN_BYTES, f);
}
static void *text(const char *s, int64_t length, int64_t capacity) {
    void *p = plum_alloc_preferred(capacity + 17, capacity + 17, PLUM_ORIGIN_BYTES);
    ((int64_t *)p)[1] = length;
    memcpy((char *)p + 16, s, (size_t)length);
    ((char *)p)[16 + length] = 0;
    return p;
}
static void value(void *p, const char *s, int64_t length) {
    CHECK(((int64_t *)p)[1] == length);
    CHECK(!memcmp((char *)p + 16, s, (size_t)length));
    CHECK(((char *)p)[16 + length] == 0);
}
static void arithmetic(void) {
    const int64_t max = INT64_MAX - 17;
    CHECK(plum_str_concat_length(0, 0) == 0);
    CHECK(plum_str_concat_length(max, 0) == max);
    CHECK(plum_str_concat_length(max - 1, 1) == max);
    CHECK(plum_str_growth_bytes(50, 40) == 51);
    CHECK(plum_str_growth_bytes(1000, 40) == 1000); /* large suffix */
    CHECK(plum_str_growth_bytes(INT64_MAX, INT64_MAX - 1) == INT64_MAX);
    CHECK(plum_str_growth_bytes(30, 0) == 30);
    CHECK(plum_str_growth_bytes(30, -1) == 30);
    /* Bounded scalar calls cover near-limit growth without huge allocations. */
    uint64_t state = 478;
    for (int i = 0; i < 10000; ++i) {
        state = state * UINT64_C(6364136223846793005) + 1;
        int64_t old = (int64_t)(state % (uint64_t)max);
        state = state * UINT64_C(6364136223846793005) + 1;
        int64_t need = old + 1 + (int64_t)(state % (uint64_t)(max - old));
        int64_t want = need;
        if (old / 2 <= max - old && old + old / 2 > want) want = old + old / 2;
        CHECK(plum_str_growth_bytes(need + 17, old + 17) == want + 17);
    }
}
static void fallback(void) {
    int64_t n = plum_n_alloc, bytes = plum_b_alloc;
    int64_t tracked_n = field(0), tracked_bytes = field(1), live = field(3);
    int64_t origin_n = origin_field(0), origin_bytes = origin_field(1);
    failures = 1; attempts = 0;
    void *p = plum_alloc_preferred(33, 80, PLUM_ORIGIN_BYTES);
    CHECK(attempts == 2 && requests[0] == 80 && requests[1] == 33);
    CHECK(*(int64_t *)p == 1);
    CHECK(plum_n_alloc == n + 1 && plum_b_alloc == bytes + 33);
#ifdef PLUM_HEAP_STATS
    CHECK(field(0) == tracked_n + 1 && field(1) == tracked_bytes + 33);
    CHECK(field(3) == live + 33);
    CHECK(origin_field(0) == origin_n + 1 && origin_field(1) == origin_bytes + 33);
#else
    (void)tracked_n; (void)tracked_bytes; (void)live; (void)origin_n; (void)origin_bytes;
#endif
    plum_rel_str(p);
#ifdef PLUM_HEAP_STATS
    CHECK(field(3) == live);
#endif
    attempts = 0;
    p = plum_alloc_preferred(33, 80, PLUM_ORIGIN_BYTES);
    CHECK(attempts == 1 && requests[0] == 80);
    plum_rel_str(p);
}
static void ownership(void) {
    void *a = text("abc", 3, 100);
    void *b = text("Z\0Q", 3, 3);
    int64_t n = plum_n_alloc, live = field(3);
    void *r = plum_str_concat_reuse(a, b);
    CHECK(r == a && plum_n_alloc == n);
    value(r, "abcZ\0Q", 6);
    CHECK(field(3) == live); /* in-place keeps original request */
    plum_rc_inc(r); /* alias prevents both mutation and reserve replication */
    attempts = 0;
    void *copy = plum_str_concat_reuse(r, b);
    CHECK(copy != r && attempts == 1 && requests[0] == 26);
    value(r, "abcZ\0Q", 6); value(copy, "abcZ\0QZ\0Q", 9);
    plum_rel_str(copy);
    attempts = 0;
    copy = plum_str_concat(r, b); /* borrow-both remains exact even at rc=1 */
    CHECK(attempts == 1 && requests[0] == 26);
    value(r, "abcZ\0Q", 6);
    plum_rel_str(copy); plum_rel_str(r); plum_rel_str(b);
    a = text("abc", 3, 100);
    r = plum_str_concat_reuse(a, a); /* disjoint ranges, source survives */
    CHECK(r == a); value(r, "abcabc", 6); plum_rel_str(r);
    a = text("abcdefghijklmnop", 16, 16);
    r = plum_str_concat_reuse(a, a); /* growth self-alias copies before release */
    value(r, "abcdefghijklmnopabcdefghijklmnop", 32); plum_rel_str(r);
    a = text("", 0, 0); b = text("", 0, 0);
    r = plum_str_concat_reuse(a, b); CHECK(r == a); value(r, "", 0);
    plum_rel_str(r); plum_rel_str(b);
}
static void growth_fallback(void) {
    char data[128]; memset(data, 'x', sizeof(data));
    void *a = text(data, 128, 128), *b = text("z", 1, 1);
    /* Fill allocator slack first so the very next append must allocate. */
    int64_t length = (int64_t)malloc_usable_size(a) - 17;
    memset((char *)a + 16, 'x', (size_t)length);
    ((char *)a)[16 + length] = 0; ((int64_t *)a)[1] = length;
    failures = 1; attempts = 0;
    int64_t n = plum_n_alloc, bytes = plum_b_alloc, live = field(3);
    int64_t old_request = 145;
    void *r = plum_str_concat_reuse(a, b);
    int64_t required = length + 18;
    CHECK(attempts == 2 && requests[0] > required && requests[1] == required);
    CHECK(plum_n_alloc == n + 1 && plum_b_alloc == bytes + required);
    for (int64_t i = 0; i < length; ++i) CHECK(((char *)r)[16+i] == 'x');
    CHECK(((int64_t *)r)[1] == length + 1 && ((char *)r)[16+length] == 'z');
    CHECK(((char *)r)[17+length] == 0);
#ifdef PLUM_HEAP_STATS
    CHECK(field(3) == live - old_request + required);
    CHECK(field(5) == live + required); /* old/new overlap before release */
#else
    (void)live; (void)old_request;
#endif
    plum_rel_str(r); plum_rel_str(b);
}
static void snapshots(void) {
    void *s = text("", 0, 0), *b = text("z", 1, 1), *saved[128];
    for (int i = 0; i < 128; ++i) {
        saved[i] = s; plum_rc_inc(s);
        attempts = 0;
        s = plum_str_concat_reuse(s, b);
        CHECK(attempts == 1 && requests[0] == i + 18);
    }
    for (int i = 0; i < 128; ++i) {
        CHECK(((int64_t *)saved[i])[1] == i);
        for (int j = 0; j < i; ++j) CHECK(((char *)saved[i])[16+j] == 'z');
        CHECK(((char *)saved[i])[16+i] == 0);
        plum_rel_str(saved[i]);
    }
    plum_rel_str(s); plum_rel_str(b);
}
static void traffic(int steps) {
    const char chunk[] = "a\xc3\xa9\0\xf0\x9f\x99\x82";
    void *b = text(chunk, 8, 8), *s = text("", 0, 0);
    int64_t n = plum_n_alloc, bytes = plum_b_alloc;
    for (int i = 0; i < steps; ++i) s = plum_str_concat_reuse(s, b);
    int64_t length = 8 * steps;
    CHECK(((int64_t *)s)[1] == length && ((char *)s)[16 + length] == 0);
    for (int i = 0; i < steps; ++i) CHECK(!memcmp((char *)s + 16 + 8*i, chunk, 8));
    /* Bounds allow allocator rounding/platform differences while rejecting
     * exact-size growth's quadratic traffic and reserve on every append. */
    CHECK(plum_n_alloc - n < 40 && plum_b_alloc - bytes < 6 * length + 4096);
#ifdef PLUM_HEAP_STATS
    CHECK(field(3) < 2 * length + 256);
    CHECK(origin_field(3) == field(3));
    CHECK(field(5) < 120000);
#endif
    printf("String growth n=%d: allocations=%lld bytes=%lld\n", steps, (long long)(plum_n_alloc-n), (long long)(plum_b_alloc-bytes));
    plum_rel_str(s); plum_rel_str(b);
#ifdef PLUM_HEAP_STATS
    CHECK(field(2) == 0 && field(3) == 0 && origin_field(3) == 0);
#endif
}
static void repetition(void) {
    const char chunk[] = "a\xc3\xa9\0\xf0\x9f\x99\x82";
    void *input = text(chunk, 8, 128);
    int64_t n = plum_n_alloc;
    for (int64_t count = -2; count <= 0; ++count) {
        void *empty = plum_str_repeat(input, count);
        value(empty, "", 0); plum_rel_str(empty);
    }
    void *empty = plum_str_repeat(input, INT64_MIN);
    value(empty, "", 0); plum_rel_str(empty);
    void *same = plum_str_repeat(input, 1);
    CHECK(same == input && *(int64_t *)input == 2);
    plum_rel_str(same);
    CHECK(plum_n_alloc == n && *(int64_t *)input == 1);
    void *blank = text("", 0, 128);
    n = plum_n_alloc;
    empty = plum_str_repeat(blank, INT64_MAX);
    value(empty, "", 0);
    CHECK(plum_n_alloc == n && *(int64_t *)blank == 1);
    plum_rel_str(empty); plum_rel_str(blank);
    CHECK(plum_str_repeat_length(0, INT64_MAX) == 0);
    CHECK(plum_str_repeat_length(-1, -1) == 0);
    CHECK(plum_str_repeat_length(1, INT64_MAX - 17) == INT64_MAX - 17);
    CHECK(plum_str_repeat_length((INT64_MAX - 17) / 3, 3) == ((INT64_MAX - 17) / 3) * 3);
    for (int64_t count = 2; count <= 4096; count = count * 2 - 1) {
        int64_t length = 8 * count, required = length + 17;
        n = plum_n_alloc;
        int64_t bytes = plum_b_alloc, live = field(3), origins = origin_field(3);
        attempts = 0;
        void *out = plum_str_repeat(input, count);
        CHECK(out != input && plum_n_alloc == n + 1 && plum_b_alloc == bytes + required);
        CHECK(attempts == 1 && requests[0] == required);
        CHECK(((int64_t *)out)[1] == length && ((char *)out)[16 + length] == 0);
        for (int64_t i = 0; i < length; ++i) CHECK(((char *)out)[16 + i] == chunk[i % 8]);
        value(input, chunk, 8);
#ifdef PLUM_HEAP_STATS
        CHECK(field(3) == live + required && origin_field(3) == origins + required);
#else
        (void)live; (void)origins;
#endif
        plum_rel_str(out);
        CHECK(field(3) == live);
    }
    plum_rel_str(input);
#ifdef PLUM_HEAP_STATS
    CHECK(field(2) == 0 && field(3) == 0);
#endif
}
static void hexadecimal(void) {
    unsigned char input[256];
    char expected[513];
    const char digits[] = "0123456789abcdef";
    for (int i = 0; i < 256; ++i) {
        input[i] = (unsigned char)i;
        expected[2*i] = digits[i / 16]; expected[2*i+1] = digits[i % 16];
    }
    expected[512] = 0;
    /* Unique input with enough capacity must still remain unchanged: borrowed. */
    void *source = text((char *)input, 256, 1024);
    int64_t n = plum_n_alloc, bytes = plum_b_alloc, live = field(3), origins = origin_field(3);
    attempts = 0;
    void *out = plum_bytes_to_hex(source);
    CHECK(out != source && *(int64_t *)source == 1 && *(int64_t *)out == 1);
    CHECK(attempts == 1 && requests[0] == 529);
    CHECK(plum_n_alloc == n + 1 && plum_b_alloc == bytes + 529);
    value(out, expected, 512); value(source, (char *)input, 256);
#ifdef PLUM_HEAP_STATS
    CHECK(field(3) == live + 529 && origin_field(3) == origins + 529);
#else
    (void)origins;
#endif
    plum_rel_str(out); CHECK(field(3) == live); plum_rel_str(source);
    source = text("", 0, 128);
    n = plum_n_alloc; attempts = 0;
    out = plum_bytes_to_hex(source);
    value(out, "", 0);
    CHECK(plum_n_alloc == n && attempts == 0 && *(int64_t *)source == 1);
    plum_rel_str(out); plum_rel_str(source);
    CHECK(plum_str_repeat_length((INT64_MAX - 17) / 2, 2) == INT64_MAX - 17);
#ifdef PLUM_HEAP_STATS
    CHECK(field(2) == 0 && field(3) == 0);
#endif
}
int main(int argc, char **argv) {
    if (argc > 1) {
        if (!strcmp(argv[1], "negative")) plum_str_concat_length(-1, 0);
        else if (!strcmp(argv[1], "negative-b")) plum_str_concat_length(0, -1);
        else if (!strcmp(argv[1], "overflow")) plum_str_concat_length(INT64_MAX, 1);
        else if (!strcmp(argv[1], "header-overflow")) plum_str_concat_length(INT64_MAX - 17, 1);
        else if (!strcmp(argv[1], "concat-negative")) {
            int64_t a[] = {-1, -1}, b[] = {-1, 0}; plum_str_concat(a, b);
        }
        else if (!strcmp(argv[1], "reuse-overflow")) {
            int64_t a[] = {1, INT64_MAX - 17}, b[] = {-1, 1}; plum_str_concat_reuse(a, b);
        }
        else if (!strcmp(argv[1], "repeat-negative-length")) {
            int64_t input[] = {1, -1}; plum_str_repeat(input, 2);
        }
        else if (!strcmp(argv[1], "repeat-overflow")) plum_str_repeat_length(INT64_MAX, 2);
        else if (!strcmp(argv[1], "repeat-header-overflow")) plum_str_repeat_length(INT64_MAX - 16, 1);
        else if (!strcmp(argv[1], "repeat-oom")) {
            struct { int64_t rc, length; char data[4]; } input = {-1, 3, "abc"};
            failures = 1; plum_str_repeat(&input, 5);
        }
        else if (!strcmp(argv[1], "hex-negative-length")) {
            int64_t input[] = {1, -1}; plum_bytes_to_hex(input);
        }
        else if (!strcmp(argv[1], "hex-overflow")) {
            int64_t input[] = {1, INT64_MAX}; plum_bytes_to_hex(input);
        }
        else if (!strcmp(argv[1], "hex-header-overflow")) {
            int64_t input[] = {1, (INT64_MAX - 17) / 2 + 1}; plum_bytes_to_hex(input);
        }
        else if (!strcmp(argv[1], "hex-oom")) {
            /* Header only: allocation failure must precede every input read. */
            int64_t input[] = {1, 3}; failures = 1; plum_bytes_to_hex(input);
        }
        else if (!strcmp(argv[1], "oom")) { failures = 2; plum_alloc_preferred(33, 80, 1); }
        else if (!strcmp(argv[1], "exact-oom")) { failures = 1; plum_alloc_preferred(33, 33, 1); }
        else return 3;
        return 4;
    }
    arithmetic(); fallback(); ownership(); growth_fallback(); snapshots();
    for (int steps = 512; steps <= 4096; steps *= 2) traffic(steps);
    repetition(); hexadecimal();
    return 0;
}
