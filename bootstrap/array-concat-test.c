/* Exercise the emitted kernel, including optional-reserve failure. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../native_stdlib/memory_shim.h"

typedef struct { int64_t rc, len; uint64_t data[]; } Array;
typedef struct { int64_t rc, value; } Child;
extern void *plum_array_concat(void *, void *, _Bool, void (*)(void *));
extern void *plum_array_concat_exact(void *, void *, _Bool, void (*)(void *));
extern void *plum_array_interleave_exact(void *, void *, uint64_t, _Bool, void (*)(void *));
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
static void heap_case(int shared_left, int shared_right, int spare) {
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
    if (shared_left) plum_rc_inc(a);
    if (shared_right) plum_rc_inc(b);
    int drops = heap_drops;
    Array *out = plum_array_concat(a, b, 1, heap_drop);
    CHECK(out->len == total && c->rc == total + 1 + (shared_left ? 2 : 0) + (shared_right ? nb : 0));
    /* Transfer unique children directly. Shared arrays still use the typed release. */
    CHECK(heap_drops == drops + shared_left + shared_right);
    for (int64_t i = 0; i < total; i++) CHECK(out->data[i] == (uint64_t)(uintptr_t)c);
    heap_drop(out);
    if (shared_left) heap_drop(a);
    if (shared_right) heap_drop(b);
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
static void heap_alias_case(void) {
    int64_t baseline = live();
    int freed = children_freed;
    Child *child = malloc(sizeof *child);
    CHECK(child);
    *child = (Child){3, 7};
#ifdef PLUM_HEAP_STATS
    plum_heap_adopt_origin(child, sizeof *child, PLUM_ORIGIN_RECORD);
#endif
    Array *a = array(2, 8, 0);
    a->data[0] = a->data[1] = (uintptr_t)child;
    plum_rc_inc(a);
    plum_rc_inc(a);
    Array *out = plum_array_concat(a, a, 1, heap_drop);
    CHECK(out != a && a->len == 2 && out->len == 4 && child->rc == 7);
    for (int i = 0; i < 4; i++) CHECK(out->data[i] == (uintptr_t)child);
    heap_drop(out);
    heap_drop(a);
    CHECK(child->rc == 1);
    child_drop(child);
    CHECK(children_freed == freed + 1 && live() == baseline);
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
static void exact_allocating_case(int sparse) {
    int64_t baseline = live();
    Array *a = array(16, sparse ? 128 : 16, 0);
    int64_t cap = ((int64_t)malloc_usable_size(a) - 16) / 8;
    int64_t nb = sparse ? 2 : cap + 1 - a->len, total = a->len + nb;
    Array *b = array(nb, nb, a->len);
    attempts = 0;
    Array *out = plum_array_concat_exact(a, b, 0, scalar_drop);
    CHECK(attempts == 1 && requests[0] == 16 + 8 * total);
    CHECK(out->len == total);
    for (int64_t i = 0; i < total; i++) CHECK(out->data[i] == (uint64_t)i);
    scalar_drop(out); CHECK(live() == baseline);
}
/* Three sorted left slots and three sorted right slots merge back to 0..5. */
static void interleave_case(int64_t capacity, int shared) {
    int64_t baseline = live();
    Array *left = array(3, capacity, 0), *right = array(3, 3, 0);
    const uint64_t ldata[] = {0, 2, 5}, rdata[] = {1, 3, 4};
    memcpy(left->data, ldata, sizeof ldata);
    memcpy(right->data, rdata, sizeof rdata);
    if (shared) plum_rc_inc(left);
    uintptr_t id = (uintptr_t)left;
    attempts = 0;
    Array *out = plum_array_interleave_exact(left, right, 26, 0, scalar_drop);
    CHECK(out->len == 6);
    for (int64_t i = 0; i < 6; i++) CHECK(out->data[i] == (uint64_t)i);
    int reuse = !shared && capacity == 8;
    CHECK(((uintptr_t)out == id) == reuse);
    CHECK(attempts == (reuse ? 0 : 1));
    if (attempts) CHECK(requests[0] == 64);
    if (shared) {
        CHECK(left->len == 3 && memcmp(left->data, ldata, sizeof ldata) == 0);
        scalar_drop(left);
    }
    scalar_drop(out);
    CHECK(live() == baseline);
}

static void interleave_heap_case(int shared) {
    int64_t baseline = live();
    int freed = children_freed;
    Child *lchild = malloc(sizeof *lchild), *rchild = malloc(sizeof *rchild);
    CHECK(lchild && rchild);
    *lchild = (Child){4, 2}; *rchild = (Child){4, 3};
#ifdef PLUM_HEAP_STATS
    plum_heap_adopt_origin(lchild, sizeof *lchild, PLUM_ORIGIN_RECORD);
    plum_heap_adopt_origin(rchild, sizeof *rchild, PLUM_ORIGIN_RECORD);
#endif
    Array *left = array(3, 8, 0), *right = array(3, 3, 0);
    for (int i = 0; i < 3; i++) {
        left->data[i] = (uintptr_t)lchild;
        right->data[i] = (uintptr_t)rchild;
    }
    if (shared) plum_rc_inc(left);
    plum_rc_inc(right);
    Array *out = plum_array_interleave_exact(left, right, 26, 1, heap_drop);
    CHECK(lchild->rc == (shared ? 7 : 4) && rchild->rc == 7);
    for (int i = 0; i < 6; i++) {
        Child *expected = (26 & (1 << i)) ? rchild : lchild;
        CHECK((uintptr_t)expected == out->data[i]);
    }
    heap_drop(out);
    if (shared) heap_drop(left);
    heap_drop(right);
    CHECK(lchild->rc == 1 && rchild->rc == 1);
    child_drop(lchild);
    child_drop(rchild);
    CHECK(children_freed == freed + 2 && live() == baseline);
}
static void owned_right_case(int shared_left, int interleaved, int sparse) {
    int64_t baseline = live();
    int freed = children_freed;
    Child *lchild = malloc(sizeof *lchild), *rchild = malloc(sizeof *rchild);
    CHECK(lchild && rchild);
    *lchild = (Child){4, 2};
    *rchild = (Child){4, 3};
#ifdef PLUM_HEAP_STATS
    plum_heap_adopt_origin(lchild, sizeof *lchild, PLUM_ORIGIN_RECORD);
    plum_heap_adopt_origin(rchild, sizeof *rchild, PLUM_ORIGIN_RECORD);
#endif
    Array *left = array(3, 3, 0), *right = array(3, sparse ? 128 : 8, 0);
    for (int i = 0; i < 3; i++) {
        left->data[i] = (uintptr_t)lchild;
        right->data[i] = (uintptr_t)rchild;
    }
    if (shared_left) plum_rc_inc(left);
    uintptr_t right_id = (uintptr_t)right;
    attempts = 0;
    Array *out = interleaved
        ? plum_array_interleave_exact(left, right, 26, 1, heap_drop)
        : plum_array_concat_exact(left, right, 1, heap_drop);
    CHECK(out->len == 6 && lchild->rc == (shared_left ? 7 : 4) && rchild->rc == 4);
    CHECK(((uintptr_t)out == right_id) == !sparse);
    CHECK(attempts == sparse);
    if (sparse) CHECK(requests[0] == 64);
    for (int i = 0; i < 6; i++) {
        Child *expected = ((interleaved ? 26 : 56) & (1 << i)) ? rchild : lchild;
        CHECK(out->data[i] == (uintptr_t)expected);
    }
    heap_drop(out);
    if (shared_left) heap_drop(left);
    CHECK(lchild->rc == 1 && rchild->rc == 1);
    child_drop(lchild);
    child_drop(rchild);
    CHECK(children_freed == freed + 2 && live() == baseline);
}
static void fatal(const char *kind) {
    struct { int64_t rc, len; } a = {2, 1}, b = {2, 1};
    if (!strcmp(kind, "negative-left")) a.len = -1;
    else if (!strcmp(kind, "negative-right")) b.len = -1;
    else if (!strcmp(kind, "large-left")) a.len = INT64_MAX;
    else if (!strcmp(kind, "large-right")) b.len = INT64_MAX;
    else if (!strcmp(kind, "combined")) a.len = 1152921504606846973;
    else if (!strcmp(kind, "oom")) fail_all = 1;
    else if (!strcmp(kind, "mask-range")) { plum_array_interleave_exact(&a, &b, 8, 0, scalar_drop); exit(4); }
    else if (!strcmp(kind, "mask-count")) { plum_array_interleave_exact(&a, &b, 3, 0, scalar_drop); exit(4); }
    else if (!strcmp(kind, "mask-total")) { a.len = 32; plum_array_interleave_exact(&a, &b, 1, 0, scalar_drop); exit(4); }
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
        for (int shared_right = 0; shared_right < 2; shared_right++) {
            heap_case(shared, shared_right, 0);
            heap_case(shared, shared_right, 1);
        }
    }
    alias_case(); heap_alias_case(); retry_case(); exact_allocating_case(0); exact_allocating_case(1);
    interleave_case(3, 0); interleave_case(8, 0); interleave_case(8, 1); interleave_case(128, 0);
    interleave_heap_case(0); interleave_heap_case(1);
    for (int shared = 0; shared < 2; shared++) {
        for (int interleaved = 0; interleaved < 2; interleaved++) {
            owned_right_case(shared, interleaved, 0);
            owned_right_case(shared, interleaved, 1);
        }
    }
    puts("array-concat: reuse, exact copies, child ownership, reserve retry and accounting passed");
    return 0;
}
