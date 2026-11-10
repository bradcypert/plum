/* Exercises actual emitted runtime functions; no duplicate growth policy. */
#include <stdint.h>
#include <stdbool.h>
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
extern int64_t plum_str_replace_length(int64_t, int64_t, int64_t, int64_t);
extern void *plum_str_replace(void *, void *, void *);
extern void *plum_str_repeat(void *, int64_t);
extern void *plum_bytes_to_hex(void *);
extern void *plum_bytes_percent_encode(void *);
extern void *plum_bytes_base64_encode(void *, bool);
extern int64_t plum_base64_length(int64_t, bool);
/* Private LLVM i8 helpers cross into C through an explicit widening bridge. */
extern int64_t growth_test_hex_pair(int64_t, int64_t);
extern int64_t plum_hex_decoded_length(int64_t);
extern void *plum_hex_decode_validated(void *);
extern int64_t plum_percent_decoded_length(int64_t, int64_t);
extern void *plum_percent_decode_validated(void *, int64_t);
extern int64_t plum_base64_decode_value(int64_t);
extern int64_t plum_base64_decoded_length(int64_t, int64_t);
extern void *plum_base64_decode_validated(void *, int64_t);
extern int64_t plum_str_added_length(int64_t, int64_t, const char *, int64_t);
extern void *plum_str_join(void *, void *);
extern void *plum_str_concat_all(void *);
extern int64_t plum_str_join_length(void *, void *);
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
static void joining(void) {
    const char chunk[] = "a\0\xc3\xa9";
    void *source = text(chunk, 4, 1024), *blank = text("", 0, 128);
    struct { int64_t rc, length; void *items[4]; } parts = {1, 1, {source}};
    int64_t n = plum_n_alloc, bytes = plum_b_alloc, live = field(3), origins = origin_field(3);
    attempts = 0;
    void *out = plum_str_join(&parts, source); /* one item ignores its separator */
    CHECK(out != source && attempts == 1 && requests[0] == 21);
    CHECK(plum_n_alloc == n + 1 && plum_b_alloc == bytes + 21);
    CHECK(*(int64_t *)source == 1 && parts.rc == 1 && parts.items[0] == source);
    value(out, chunk, 4); value(source, chunk, 4);
#ifdef PLUM_HEAP_STATS
    CHECK(field(3) == live + 21 && origin_field(3) == origins + 21);
#else
    (void)origins;
#endif
    plum_rel_str(out); CHECK(field(3) == live);
    parts.length = 4; parts.items[1] = blank; parts.items[2] = source; parts.items[3] = blank;
    plum_rc_inc(source); plum_rc_inc(blank); /* duplicate array edges */
    char expected[20]; for (int i = 0; i < 5; ++i) memcpy(expected + 4*i, chunk, 4);
    attempts = 0;
    out = plum_str_join(&parts, source); /* separator aliases an array element */
    CHECK(attempts == 1 && requests[0] == 37);
    CHECK(parts.rc == 1 && parts.length == 4 && *(int64_t *)source == 2 && *(int64_t *)blank == 2);
    value(out, expected, 20); value(source, chunk, 4); value(blank, "", 0);
    plum_rel_str(out);
    out = plum_str_concat_all(&parts); value(out, expected, 8); plum_rel_str(out);
    parts.items[0] = blank; parts.items[2] = blank;
    n = plum_n_alloc; attempts = 0;
    out = plum_str_join(&parts, blank); value(out, "", 0); plum_rel_str(out);
    CHECK(plum_n_alloc == n && attempts == 0);
    parts.length = 0;
    /* Empty array does not inspect an unused separator or item payload. */
    out = plum_str_join(&parts, NULL); value(out, "", 0); plum_rel_str(out);
    out = plum_str_concat_all(&parts); value(out, "", 0); plum_rel_str(out);
    CHECK(plum_n_alloc == n && attempts == 0);
    /* Header-only values test the exact boundary without enormous allocations. */
    int64_t huge[] = {-1, INT64_MAX - 17};
    parts.length = 1; parts.items[0] = huge;
    CHECK(plum_str_join_length(&parts, blank) == INT64_MAX - 17);
    parts.length = 2; parts.items[0] = blank; parts.items[1] = blank;
    CHECK(plum_str_join_length(&parts, huge) == INT64_MAX - 17);
    plum_rel_str(source); plum_rel_str(source); plum_rel_str(blank); plum_rel_str(blank);
#ifdef PLUM_HEAP_STATS
    CHECK(field(2) == 0 && field(3) == 0);
#endif
}
static void percent_encoding(void) {
    char input[256], expected[637];
    const char *allowed = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    int size = 0;
    for (int i = 0; i < 256; ++i) {
        input[i] = (char)i;
        if (i && strchr(allowed, i)) expected[size++] = (char)i;
        else { CHECK(snprintf(expected + size, 4, "%%%02X", i) == 3); size += 3; }
    }
    CHECK(size == 636); expected[size] = 0;
    void *source = text(input, 256, 4096);
    int64_t n = plum_n_alloc, bytes = plum_b_alloc, live = field(3), origins = origin_field(3);
    attempts = 0;
    void *out = plum_bytes_percent_encode(source);
    CHECK(out != source && attempts == 1 && requests[0] == 653);
    CHECK(plum_n_alloc == n + 1 && plum_b_alloc == bytes + 653 && *(int64_t *)source == 1);
    value(out, expected, 636); value(source, input, 256);
#ifdef PLUM_HEAP_STATS
    CHECK(field(3) == live + 653 && origin_field(3) == origins + 653);
#else
    (void)origins;
#endif
    plum_rel_str(out); CHECK(field(3) == live); plum_rel_str(source);
    source = text(allowed, (int64_t)strlen(allowed), 4096);
    n = plum_n_alloc; bytes = plum_b_alloc; live = field(3); attempts = 0;
    out = plum_bytes_percent_encode(source);
    CHECK(out == source && *(int64_t *)source == 2 && attempts == 0);
    CHECK(plum_n_alloc == n && plum_b_alloc == bytes && field(3) == live);
    plum_rel_str(source); /* Result keeps the entire original request alive. */
    CHECK(*(int64_t *)out == 1 && field(3) == live);
    value(out, allowed, (int64_t)strlen(allowed)); plum_rel_str(out);
    source = text("", 0, 4096); n = plum_n_alloc; attempts = 0;
    out = plum_bytes_percent_encode(source);
    CHECK(out != source && *(int64_t *)source == 1 && plum_n_alloc == n && attempts == 0);
    value(out, "", 0); plum_rel_str(source); plum_rel_str(out);
#ifdef PLUM_HEAP_STATS
    CHECK(field(2) == 0 && field(3) == 0);
#endif
}
static int reference_base64(char *out, const unsigned char *input, int length, bool url) {
    const char *alpha = url ? "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
                            : "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    int written = 0;
    for (int i = 0; i < length; i += 3) {
        int count = length - i < 3 ? length - i : 3;
        uint32_t word = (uint32_t)input[i] << 16;
        if (count > 1) word |= (uint32_t)input[i + 1] << 8;
        if (count > 2) word |= input[i + 2];
        int width = url ? count + 1 : 4;
        for (int j = 0; j < width; ++j) out[written++] = j <= count ? alpha[(word >> (18 - 6*j)) & 63] : '=';
    }
    out[written] = 0; return written;
}
static void base64_encoding(void) {
    unsigned char input[258]; char expected[350];
    for (int i = 0; i < 258; ++i) input[i] = (unsigned char)i;
    void *source = text((char *)input, 258, 4096);
    for (int length = 0; length <= 258; ++length) {
        ((int64_t *)source)[1] = length;
        memcpy((char *)source + 16, input, (size_t)length);
        ((char *)source)[16 + length] = 0;
        for (int url = 0; url < 2; ++url) {
            int size = reference_base64(expected, input, length, url != 0);
            CHECK(plum_base64_length(length, url != 0) == size);
            int64_t n = plum_n_alloc, bytes = plum_b_alloc, live = field(3), origins = origin_field(3);
            attempts = 0;
            void *out = plum_bytes_base64_encode(source, url != 0);
            int64_t request = size ? size + 17 : 0;
            CHECK(out != source && *(int64_t *)source == 1);
            CHECK(attempts == (size ? 1 : 0));
            if (size) CHECK(requests[0] == request && *(int64_t *)out == 1);
            CHECK(plum_n_alloc == n + (size ? 1 : 0) && plum_b_alloc == bytes + request);
            value(out, expected, size); value(source, (char *)input, length);
#ifdef PLUM_HEAP_STATS
            CHECK(field(3) == live + request && origin_field(3) == origins + request);
#else
            (void)origins;
#endif
            plum_rel_str(out); CHECK(field(3) == live);
        }
    }
    for (int n = 0; n < 10000; ++n) {
        CHECK(plum_base64_length(n, false) == ((n + 2) / 3) * 4);
        CHECK(plum_base64_length(n, true) == (n / 3) * 4 + (n % 3 ? n % 3 + 1 : 0));
    }
    const int64_t q = (INT64_MAX - 17) / 4;
    CHECK(plum_base64_length(q * 3, false) == q * 4);
    CHECK(plum_base64_length(q * 3, true) == q * 4);
    CHECK(plum_base64_length(q * 3 + 1, true) == INT64_MAX - 17);
    plum_rel_str(source);
#ifdef PLUM_HEAP_STATS
    CHECK(field(2) == 0 && field(3) == 0);
#endif
}
static void base64_decoding(void) {
    const char *alpha = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for (int c = 0; c < 256; ++c) {
        const char *p = c ? strchr(alpha, c) : NULL;
        int64_t expected = c == '-' ? 62 : c == '_' ? 63 : p ? p - alpha : -1;
        CHECK(plum_base64_decode_value(c) == expected);
    }
    CHECK(plum_base64_decode_value(-1) == -1 && plum_base64_decode_value(256) == -1);
    CHECK(plum_base64_decode_value(INT64_MIN) == -1 && plum_base64_decode_value(INT64_MAX) == -1);
    unsigned char bytes[258]; char encoded[350];
    for (int i = 0; i < 258; ++i) bytes[i] = (unsigned char)i;
    for (int length = 0; length <= 258; ++length) {
        for (int url = 0; url < 2; ++url) {
            int encoded_length = reference_base64(encoded, bytes, length, url != 0);
            int count = encoded_length;
            while (count && encoded[count - 1] == '=') --count;
            CHECK(plum_base64_decoded_length(encoded_length, count) == length);
            void *source = text(encoded, encoded_length, 4096);
            int64_t n = plum_n_alloc, allocated = plum_b_alloc, live = field(3), origins = origin_field(3);
            attempts = 0;
            void *out = plum_base64_decode_validated(source, count);
            int64_t request = length ? length + 17 : 0;
            CHECK(out != source && *(int64_t *)source == 1 && attempts == (length ? 1 : 0));
            if (length) CHECK(requests[0] == request && *(int64_t *)out == 1);
            CHECK(plum_n_alloc == n + (length ? 1 : 0) && plum_b_alloc == allocated + request);
            value(out, (char *)bytes, length); value(source, encoded, encoded_length);
#ifdef PLUM_HEAP_STATS
            CHECK(field(3) == live + request && origin_field(3) == origins + request);
#else
            (void)origins;
#endif
            plum_rel_str(out); CHECK(field(3) == live); plum_rel_str(source);
        }
    }
    for (int count = 0; count < 10000; ++count) {
        if (count % 4 == 1) continue;
        int64_t expected = (count / 4) * 3 + (count % 4 ? count % 4 - 1 : 0);
        CHECK(plum_base64_decoded_length(count, count) == expected);
        CHECK(plum_base64_decoded_length(((count + 3) / 4) * 4, count) == expected);
    }
    const int64_t max = INT64_MAX - 17, q = max / 4;
    CHECK(plum_base64_decoded_length(max, max) == q * 3 + 1);
    CHECK(plum_base64_decoded_length(max - 2, max - 2) == q * 3);
    CHECK(plum_base64_decoded_length(max - 2, max - 3) == q * 3 - 1);
    CHECK(plum_base64_decoded_length(max - 2, max - 4) == q * 3 - 2);
#ifdef PLUM_HEAP_STATS
    CHECK(field(2) == 0 && field(3) == 0);
#endif
}

static void percent_decoding(void) {
    const char *digits[] = {"0123456789ABCDEF", "0123456789abcdef"};
    for (int lower = 0; lower < 2; ++lower) {
        for (int literal = 0; literal < 2; ++literal) {
            unsigned char encoded[1024], expected[512];
            int n = 0, length = 0, escapes = 0;
            for (int c = 0; c < 256; ++c) {
                if (literal && c != '%') encoded[n++] = (unsigned char)c;
                else {
                    encoded[n++] = '%'; encoded[n++] = digits[lower][c / 16];
                    encoded[n++] = digits[lower][c % 16]; ++escapes;
                }
                expected[length++] = (unsigned char)c;
            }
            void *source = text((char *)encoded, n, 4096);
            int64_t allocs = plum_n_alloc, bytes = plum_b_alloc, live = field(3), origin = origin_field(3);
            attempts = 0;
            void *out = plum_percent_decode_validated(source, escapes);
            CHECK(plum_percent_decoded_length(n, escapes) == length);
            CHECK(out != source && *(int64_t *)source == 1 && *(int64_t *)out == 1);
            CHECK(attempts == 1 && requests[0] == length + 17);
            CHECK(plum_n_alloc == allocs + 1 && plum_b_alloc == bytes + length + 17);
            value(source, (char *)encoded, n); value(out, (char *)expected, length);
#ifdef PLUM_HEAP_STATS
            CHECK(field(3) == live + length + 17 && origin_field(3) == origin + length + 17);
#else
            (void)origin;
#endif
            plum_rel_str(out); CHECK(field(3) == live); plum_rel_str(source);
        }
    }
    /* Identity shares the existing requested capacity, including NUL/high bytes. */
    const char plain[] = "a+\0\xc3\xa9";
    void *source = text(plain, 5, 4096);
    int64_t allocs = plum_n_alloc, live = field(3);
    attempts = 0;
    void *out = plum_percent_decode_validated(source, 0);
    CHECK(out == source && *(int64_t *)source == 2 && attempts == 0);
    CHECK(plum_n_alloc == allocs && field(3) == live);
    value(out, plain, 5); plum_rel_str(source); value(out, plain, 5); plum_rel_str(out);
    source = text("", 0, 4096); live = field(3); attempts = 0;
    out = plum_percent_decode_validated(source, 0);
    CHECK(out != source && *(int64_t *)source == 1 && attempts == 0);
    value(out, "", 0); plum_rel_str(out); CHECK(field(3) == live); plum_rel_str(source);
    for (int64_t n = 0; n < 10000; ++n) {
        CHECK(plum_percent_decoded_length(n, 0) == n);
        CHECK(plum_percent_decoded_length(n, n / 6) == n - 2 * (n / 6));
        CHECK(plum_percent_decoded_length(n, n / 3) == n - 2 * (n / 3));
    }
    const int64_t max = INT64_MAX - 17;
    CHECK(plum_percent_decoded_length(max, 0) == max);
    CHECK(plum_percent_decoded_length(max, max / 3) == max - 2 * (max / 3));
#ifdef PLUM_HEAP_STATS
    CHECK(field(2) == 0 && field(3) == 0);
#endif
}


static void hex_decoding(void) {
    const char *digits[] = {"0123456789ABCDEF", "0123456789abcdef"};
    unsigned char input[516], expected[258];
    for (int form = 0; form < 4; ++form) {
        for (int hi = 0; hi < 16; ++hi)
            for (int lo = 0; lo < 16; ++lo)
                CHECK(growth_test_hex_pair(digits[form & 1][hi], digits[form >> 1][lo]) == hi * 16 + lo);
        for (int length = 0; length <= 258; ++length) {
            for (int i = 0; i < length; ++i) {
                expected[i] = (unsigned char)i;
                input[2*i] = digits[form & 1][expected[i] / 16];
                input[2*i+1] = digits[form >> 1][expected[i] % 16];
            }
            void *source = text((char *)input, 2 * length, 4096);
            int64_t allocs = plum_n_alloc, bytes = plum_b_alloc, live = field(3), origin = origin_field(3);
            attempts = 0;
            void *out = plum_hex_decode_validated(source);
            int64_t request = length ? length + 17 : 0;
            CHECK(plum_hex_decoded_length(2 * length) == length);
            CHECK(out != source && *(int64_t *)source == 1);
            CHECK(attempts == (length ? 1 : 0));
            if (length) CHECK(requests[0] == request && *(int64_t *)out == 1);
            CHECK(plum_n_alloc == allocs + (length ? 1 : 0) && plum_b_alloc == bytes + request);
            value(source, (char *)input, 2 * length); value(out, (char *)expected, length);
#ifdef PLUM_HEAP_STATS
            CHECK(field(3) == live + request && origin_field(3) == origin + request);
#else
            (void)origin;
#endif
            plum_rel_str(out); CHECK(field(3) == live); plum_rel_str(source);
        }
    }
    for (int64_t n = 0; n < 10000; n += 2) CHECK(plum_hex_decoded_length(n) == n / 2);
    const int64_t max = INT64_MAX - 17;
    CHECK(plum_hex_decoded_length(max) == max / 2);
    CHECK(plum_hex_decoded_length(max - 2) == max / 2 - 1);
#ifdef PLUM_HEAP_STATS
    CHECK(field(2) == 0 && field(3) == 0);
#endif
}


static int replace_reference(char *out, const char *src, int n, const char *from, int m, const char *to, int t, int *matches) {
    int i = 0, j = 0; *matches = 0;
    while (i < n) {
        if (m && m <= n-i && !memcmp(src+i, from, (size_t)m)) {
            memcpy(out+j, to, (size_t)t); j += t; i += m; ++*matches;
        } else out[j++] = src[i++];
    }
    return j;
}
static void replacement(void) {
    uint64_t state = 492;
    for (int trial = 0; trial < 2048; ++trial) {
        state = state * UINT64_C(6364136223846793005) + 1;
        int n = (int)(state % 65), m = (int)((state >> 8) % 9), t = (int)((state >> 16) % 9);
        char src[65], from[9], to[9], expected[1024];
        for (int i = 0; i < n; ++i) src[i] = (char)((state >> (i % 32)) % 5);
        for (int i = 0; i < m; ++i) from[i] = (char)((state >> ((i+3) % 32)) % 5);
        for (int i = 0; i < t; ++i) to[i] = (char)((state >> ((i+9) % 32)) % 5);
        if (trial % 3 == 0 && m <= n) memcpy(from, src, (size_t)m);
        if (trial % 4 == 0) { t = m; memcpy(to, from, (size_t)m); }
        int count, length = replace_reference(expected, src, n, from, m, to, t, &count);
        CHECK(plum_str_replace_length(n, m, t, count) == length);
        void *source = text(src, n, 4096), *pattern = text(from, m, m), *target = text(to, t, t);
        int identity = n && (!m || !count || (m == t && !memcmp(from, to, (size_t)m)));
        int changed = !identity && length;
        int64_t allocs = plum_n_alloc, bytes = plum_b_alloc, live = field(3), origin = origin_field(3);
        attempts = 0;
        void *out = plum_str_replace(source, pattern, target);
        int64_t request = changed ? length + 17 : 0;
        CHECK(attempts == changed && plum_n_alloc == allocs + changed && plum_b_alloc == bytes + request);
        if (changed) CHECK(requests[0] == request && *(int64_t *)out == 1);
        CHECK((out == source) == identity && *(int64_t *)source == 1 + identity);
        value(source, src, n); value(pattern, from, m); value(target, to, t); value(out, expected, length);
        CHECK(*(int64_t *)pattern == 1 && *(int64_t *)target == 1);
#ifdef PLUM_HEAP_STATS
        CHECK(field(3) == live + request && origin_field(3) == origin + request);
#else
        (void)origin;
#endif
        plum_rel_str(out); CHECK(field(3) == live);
        plum_rel_str(source); plum_rel_str(pattern); plum_rel_str(target);
    }
    void *source = text("abcabc", 6, 4096), *pattern = text("abc", 3, 3);
    void *out = plum_str_replace(source, pattern, source);
    value(out, "abcabcabcabc", 12); value(source, "abcabc", 6); CHECK(*(int64_t *)source == 1);
    plum_rel_str(out); plum_rel_str(pattern);
    failures = 1; attempts = 0;
    out = plum_str_replace(source, source, source);
    CHECK(out == source && attempts == 0 && *(int64_t *)source == 2);
    failures = 0; plum_rel_str(out); plum_rel_str(source);
    source = text("abc", 3, 4096); void *target = text("XY", 2, 2);
    out = plum_str_replace(source, source, target); value(out, "XY", 2); value(source, "abc", 3);
    plum_rel_str(out); plum_rel_str(target); plum_rel_str(source);
    /* Identity owns the original capacity after the caller drops its reference. */
    source = text("abc", 3, 4096); pattern = text("z", 1, 1); target = text("q", 1, 1);
    out = plum_str_replace(source, pattern, target);
    CHECK(out == source && *(int64_t *)out == 2);
    plum_rel_str(source); plum_rel_str(pattern); plum_rel_str(target);
    value(out, "abc", 3); CHECK(*(int64_t *)out == 1);
#ifdef PLUM_HEAP_STATS
    CHECK(field(3) == 4113 && origin_field(3) == 4113);
#endif
    plum_rel_str(out);
    const int64_t max = INT64_MAX - 17;
    CHECK(plum_str_replace_length(max, 0, max, 0) == max);
    CHECK(plum_str_replace_length(max, max, 0, 1) == 0);
    CHECK(plum_str_replace_length(max, max, max, 1) == max);
    CHECK(plum_str_replace_length(max, 1, 1, max) == max);
    CHECK(plum_str_replace_length(max, 1, 0, max) == 0);
    CHECK(plum_str_replace_length(max - 1, 1, 2, 1) == max);
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
        else if (!strncmp(argv[1], "join-", 5) || !strcmp(argv[1], "concat-all-oom")) {
            int64_t zero[] = {-1, 0}, one[] = {-1, 1}, huge[] = {-1, INT64_MAX - 17};
            struct { int64_t rc, length; void *items[3]; } parts = {-1, 1, {one, zero, zero}};
            void *sep = zero;
            if (!strcmp(argv[1], "join-negative-count")) parts.length = -1;
            else if (!strcmp(argv[1], "join-large-count")) parts.length = 1152921504606846974LL;
            else if (!strcmp(argv[1], "join-negative-part")) one[1] = -1;
            else if (!strcmp(argv[1], "join-header-overflow")) one[1] = INT64_MAX - 16;
            else if (!strcmp(argv[1], "join-sum-overflow")) { parts.length = 2; parts.items[1] = one; one[1] = (INT64_MAX - 17) / 2 + 20; }
            else if (!strcmp(argv[1], "join-negative-separator")) { parts.length = 2; huge[1] = -1; sep = huge; }
            else if (!strcmp(argv[1], "join-product-overflow")) { parts.length = 3; huge[1] = INT64_MAX; sep = huge; }
            else if (!strcmp(argv[1], "join-separator-header-overflow")) { parts.length = 2; huge[1] = INT64_MAX - 16; sep = huge; }
            else if (!strcmp(argv[1], "join-combined-overflow")) { parts.length = 2; sep = huge; }
            else if (!strcmp(argv[1], "join-oom")) failures = 1;
            else if (!strcmp(argv[1], "concat-all-oom")) { failures = 1; plum_str_concat_all(&parts); return 4; }
            else return 3;
            /* OOM cases have no byte payload: failure must precede copying. */
            plum_str_join(&parts, sep);
        }
        else if (!strcmp(argv[1], "percent-negative-length")) {
            int64_t input[] = {1, -1}; plum_bytes_percent_encode(input);
        }
        else if (!strcmp(argv[1], "percent-large-length")) {
            int64_t input[] = {1, INT64_MAX}; plum_bytes_percent_encode(input);
        }
        else if (!strcmp(argv[1], "percent-sum-overflow") || !strcmp(argv[1], "percent-header-overflow")) {
            const char error[] = "Plum String: invalid percent-encoding size\n";
            int64_t length = !strcmp(argv[1], "percent-sum-overflow") ? INT64_MAX - 2 : INT64_MAX - 18;
            plum_str_added_length(length, 3, error, (int64_t)sizeof(error) - 1);
        }
        else if (!strcmp(argv[1], "percent-oom")) {
            struct { int64_t rc, length; char data[2]; } input = {1, 1, "%"};
            failures = 1; plum_bytes_percent_encode(&input);
        }
        else if (!strcmp(argv[1], "base64-negative-length")) {
            int64_t input[] = {1, -1}; plum_bytes_base64_encode(input, false);
        }
        else if (!strcmp(argv[1], "base64-large-length")) {
            int64_t input[] = {1, INT64_MAX}; plum_bytes_base64_encode(input, true);
        }
        else if (!strcmp(argv[1], "base64-product-overflow")) {
            int64_t input[] = {1, INT64_MAX - 17}; plum_bytes_base64_encode(input, false);
        }
        else if (!strcmp(argv[1], "base64-header-overflow")) {
            int64_t input[] = {1, ((INT64_MAX - 17) / 4) * 3 + 1}; plum_bytes_base64_encode(input, false);
        }
        else if (!strcmp(argv[1], "base64-url-header-overflow")) {
            int64_t input[] = {1, ((INT64_MAX - 17) / 4) * 3 + 2}; plum_bytes_base64_encode(input, true);
        }
        else if (!strcmp(argv[1], "base64-oom")) {
            /* Header only: the failed result allocation precedes every data read. */
            int64_t input[] = {1, 1}; failures = 1; plum_bytes_base64_encode(input, false);
        }
        else if (!strncmp(argv[1], "b64decode-", 10)) {
            int64_t input[] = {1, 2}, count = 2;
            if (!strcmp(argv[1], "b64decode-negative-input")) input[1] = -1;
            else if (!strcmp(argv[1], "b64decode-large-input")) input[1] = INT64_MAX;
            else if (!strcmp(argv[1], "b64decode-negative-count")) count = -1;
            else if (!strcmp(argv[1], "b64decode-count-overflow")) count = 3;
            else if (!strcmp(argv[1], "b64decode-short-group")) { input[1] = 1; count = 1; }
            else if (!strcmp(argv[1], "b64decode-padding-shape")) input[1] = 3;
            else if (!strcmp(argv[1], "b64decode-extra-padding")) input[1] = 8;
            else if (!strcmp(argv[1], "b64decode-oom")) failures = 1;
            else return 3;
            /* A header-only source proves failure precedes any payload read. */
            plum_base64_decode_validated(input, count);
        }
        else if (!strncmp(argv[1], "pctdecode-", 10)) {
            int64_t input[] = {1, 3}, escapes = 1;
            if (!strcmp(argv[1], "pctdecode-negative-input")) input[1] = -1;
            else if (!strcmp(argv[1], "pctdecode-large-input")) input[1] = INT64_MAX;
            else if (!strcmp(argv[1], "pctdecode-negative-count")) escapes = -1;
            else if (!strcmp(argv[1], "pctdecode-count-overflow")) escapes = INT64_MAX;
            else if (!strcmp(argv[1], "pctdecode-short-group")) input[1] = 2;
            else if (!strcmp(argv[1], "pctdecode-oom")) failures = 1;
            else return 3;
            /* Header only: count/size/OOM failures must precede payload reads. */
            plum_percent_decode_validated(input, escapes);
        }
        else if (!strncmp(argv[1], "hexdecode-", 10)) {
            int64_t input[] = {1, 2};
            if (!strcmp(argv[1], "hexdecode-negative-input")) input[1] = -1;
            else if (!strcmp(argv[1], "hexdecode-large-input")) input[1] = INT64_MAX;
            else if (!strcmp(argv[1], "hexdecode-header-overflow")) input[1] = INT64_MAX - 16;
            else if (!strcmp(argv[1], "hexdecode-odd-input")) input[1] = 1;
            else if (!strcmp(argv[1], "hexdecode-oom")) failures = 1;
            else return 3;
            /* Header only: invalid sizes/OOM must fail before any payload read. */
            plum_hex_decode_validated(input);
        }
        else if (!strncmp(argv[1], "replace-", 8)) {
            int64_t source[] = {1, 2}, from[] = {1, 1}, to[] = {1, 2};
            const int64_t max = INT64_MAX - 17;
            if (!strcmp(argv[1], "replace-negative-input")) source[1] = -1;
            else if (!strcmp(argv[1], "replace-large-input")) source[1] = INT64_MAX;
            else if (!strcmp(argv[1], "replace-negative-pattern")) from[1] = -1;
            else if (!strcmp(argv[1], "replace-negative-target")) to[1] = -1;
            else if (!strcmp(argv[1], "replace-large-pattern")) from[1] = INT64_MAX;
            else if (!strcmp(argv[1], "replace-large-target")) to[1] = INT64_MAX;
            else if (!strcmp(argv[1], "replace-negative-count")) { plum_str_replace_length(2, 1, 2, -1); return 4; }
            else if (!strcmp(argv[1], "replace-large-count")) { plum_str_replace_length(2, 1, 2, 3); return 4; }
            else if (!strcmp(argv[1], "replace-empty-pattern-count")) { plum_str_replace_length(2, 0, 2, 1); return 4; }
            else if (!strcmp(argv[1], "replace-product-overflow")) { plum_str_replace_length(max, 1, max, 2); return 4; }
            else if (!strcmp(argv[1], "replace-header-overflow")) { plum_str_replace_length(max, 1, 2, 1); return 4; }
            else if (!strcmp(argv[1], "replace-signed-overflow")) { plum_str_replace_length(max, 1, max, 1); return 4; }
            else if (!strcmp(argv[1], "replace-oom")) {
                struct {int64_t rc, n; char data[3];} s = {1, 1, "a"}, f = {1, 1, "a"}, t = {1, 2, "bb"};
                failures = 1; plum_str_replace(&s, &f, &t); return 4;
            }
            else return 3;
            /* Invalid headers fail before any payload or reference update. */
            plum_str_replace(source, from, to);
        }
        else if (!strcmp(argv[1], "oom")) { failures = 2; plum_alloc_preferred(33, 80, 1); }
        else if (!strcmp(argv[1], "exact-oom")) { failures = 1; plum_alloc_preferred(33, 33, 1); }
        else return 3;
        return 4;
    }
    arithmetic(); fallback(); ownership(); growth_fallback(); snapshots();
    for (int steps = 512; steps <= 4096; steps *= 2) traffic(steps);
    repetition(); hexadecimal(); joining(); percent_encoding(); base64_encoding(); base64_decoding(); percent_decoding(); hex_decoding(); replacement();
    return 0;
}
