/* Exercise the emitted kernel, including optional-reserve failure. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../native_stdlib/memory_shim.h"

typedef struct { int64_t rc, len; uint64_t data[]; } Array;
typedef struct { int64_t rc, value; } Child;
extern void *plum_array_concat(void *, void *, _Bool, void (*)(void *));
extern int64_t plum_array_concat_size(int64_t, int64_t);
extern void plum_rc_inc(void *);
extern size_t malloc_usable_size(void *);
extern int64_t plum_heap_snapshot(void), plum_heap_snapshot_field(int64_t, int64_t);
extern int64_t plum_n_alloc, plum_b_alloc;
static int attempts, fail_first, fail_all, children_freed, heap_drops;
static int64_t requests[2];
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "array-concat: %s at %d\n", #x, __LINE__); exit(2); } } while (0)
void *concat_test_malloc(int64_t n) {
    if (attempts < 2) requests[attempts] = n;
    attempts++;
    return fail_all || (fail_first && attempts == 1) ? NULL : malloc((size_t)n);
}
static int64_t live(void) {
    int64_t t = plum_heap_snapshot();
    return t ? plum_heap_snapshot_field(t, 3) : 0;
}
static Array *array(int64_t n, int64_t cap, int64_t start) {
    CHECK(cap >= n);
    int64_t bytes = 16 + 8 * cap;
    Array *a = malloc((size_t)bytes); CHECK(a);
    a->rc = 1; a->len = n;
    for (int64_t i = 0; i < n; i++) a->data[i] = (uint64_t)(start + i);
#ifdef PLUM_HEAP_STATS
    plum_heap_adopt_origin(a, bytes, PLUM_ORIGIN_ARRAY);
#endif
    return a;
}
static void scalar_drop(void *p) {
    Array *a = p;
    if (a->rc < 0 || --a->rc) return;
    plum_free(a);
}
static void child_drop(Child *c) {
    if (--c->rc) return;
    children_freed++; plum_free(c);
}
static void heap_drop(void *p) {
    heap_drops++;
    Array *a = p;
    if (a->rc < 0 || --a->rc) return;
    for (int64_t i = 0; i < a->len; i++) child_drop((Child *)(uintptr_t)a->data[i]);
    plum_free(a);
}
static void scalar_case(int64_t na, int64_t nb, int64_t cap, int shared) {
    int64_t baseline = live();
    Array *a = array(na, cap, 0), *b = array(nb, nb, na);
    if (shared) plum_rc_inc(a);
    int reuse = na > 0 && nb > 0 && !shared && malloc_usable_size(a) >= (uint64_t)(16 + 8 * (na + nb));
    uintptr_t id = (uintptr_t)a;
    attempts = 0;
    Array *out = plum_array_concat(a, b, 0, scalar_drop);
    CHECK(out->len == na + nb);
    for (int64_t i = 0; i < out->len; i++) CHECK(out->data[i] == (uint64_t)i);
    CHECK(attempts == (na == 0 || nb == 0 || reuse ? 0 : 1));
    if (na > 0 && nb > 0) CHECK(((uintptr_t)out == id) == reuse);
    if (shared) {
        CHECK(a->len == na);
        for (int64_t i = 0; i < na; i++) CHECK(a->data[i] == (uint64_t)i);
        if (na > 0 && nb > 0) CHECK(requests[0] == 16 + 8 * (na + nb));
    }
    scalar_drop(out); if (shared) scalar_drop(a);
    CHECK(live() == baseline);
}
static void heap_case(int shared, int spare) {
    int64_t baseline = live(); int freed = children_freed;
    Child *c = malloc(sizeof *c); CHECK(c); c->rc = 1; c->value = 7;
#ifdef PLUM_HEAP_STATS
    plum_heap_adopt_origin(c, sizeof *c, PLUM_ORIGIN_RECORD);
#endif
    int64_t nb = spare ? 1 : 32, total = 2 + nb;
    Array *a = array(2, spare ? 64 : 2, 0), *b = array(nb, nb, 0);
    a->data[0] = a->data[1] = (uint64_t)(uintptr_t)c;
    for (int64_t i = 0; i < nb; i++) b->data[i] = (uint64_t)(uintptr_t)c;
    c->rc += total;
    if (shared) plum_rc_inc(a);
    int drops = heap_drops;
    Array *out = plum_array_concat(a, b, 1, heap_drop);
    CHECK(out->len == total && c->rc == total + (shared ? 3 : 1));
    /* A unique prefix transfers; freeing its old cell never drops children. */
    CHECK(heap_drops == drops + (shared ? 2 : 1));
    for (int64_t i = 0; i < total; i++) CHECK(out->data[i] == (uint64_t)(uintptr_t)c);
    heap_drop(out); if (shared) heap_drop(a);
    CHECK(c->rc == 1); child_drop(c);
    CHECK(children_freed == freed + 1 && live() == baseline);
}
static void alias_case(void) {
    int64_t baseline = live(); Array *a = array(2, 32, 0);
    plum_rc_inc(a); attempts = 0;
    Array *out = plum_array_concat(a, a, 0, scalar_drop);
    CHECK(attempts == 1 && requests[0] == 48 && out->len == 4);
    for (int i = 0; i < 4; i++) CHECK(out->data[i] == (uint64_t)(i % 2));
    scalar_drop(out); CHECK(live() == baseline);
}
static void retry_case(void) {
    int64_t baseline = live(); Array *a = array(16, 16, 0);
    int64_t cap = ((int64_t)malloc_usable_size(a) - 16) / 8;
    int64_t nb = cap + 1 - a->len, total = a->len + nb;
    Array *b = array(nb, nb, a->len);
    int64_t old_n = plum_n_alloc, old_bytes = plum_b_alloc;
    attempts = 0; fail_first = 1;
    Array *out = plum_array_concat(a, b, 0, scalar_drop);
    fail_first = 0;
    CHECK(attempts == 2 && requests[0] > requests[1] && requests[1] == 16 + 8 * total);
    CHECK(plum_n_alloc == old_n + 1 && plum_b_alloc == old_bytes + requests[1]);
    CHECK(out->len == total);
    for (int64_t i = 0; i < total; i++) CHECK(out->data[i] == (uint64_t)i);
#ifdef PLUM_HEAP_STATS
    CHECK(live() == baseline + requests[1]);
#endif
    scalar_drop(out); CHECK(live() == baseline);
}
static void fatal(const char *kind) {
    struct { int64_t rc, len; } a = {2, 1}, b = {2, 1};
    if (!strcmp(kind, "negative-left")) a.len = -1;
    else if (!strcmp(kind, "negative-right")) b.len = -1;
    else if (!strcmp(kind, "large-left")) a.len = INT64_MAX;
    else if (!strcmp(kind, "large-right")) b.len = INT64_MAX;
    else if (!strcmp(kind, "combined")) a.len = 1152921504606846973;
    else if (!strcmp(kind, "oom")) fail_all = 1;
    else exit(3);
    /* Invalid sizes and allocation failure precede any payload access. */
    plum_array_concat(&a, &b, 0, scalar_drop); exit(4);
}
int main(int argc, char **argv) {
    if (argc == 2) fatal(argv[1]);
    CHECK(plum_array_concat_size(1152921504606846973, 0) == 1152921504606846973);
    CHECK(plum_array_concat_size(1152921504606846972, 1) == 1152921504606846973);
    for (int shared = 0; shared < 2; shared++) {
        for (int64_t n = 0; n < 65; n++) {
            scalar_case(n, 7, n, shared); scalar_case(n, 7, n + 100, shared);
            scalar_case(n, 0, n + 100, shared); scalar_case(0, n, 4096, shared);
        }
        heap_case(shared, 0); heap_case(shared, 1);
    }
    alias_case(); retry_case();
    puts("array-concat: reuse, exact copies, child ownership, reserve retry and accounting passed");
    return 0;
}
