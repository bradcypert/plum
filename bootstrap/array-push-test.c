/* Exercise the emitted push kernel, including optional-reserve failure. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../native_stdlib/memory_shim.h"

typedef struct { int64_t rc, len; uint64_t data[]; } Array;
typedef struct { int64_t rc, value; } Child;
extern void *plum_array_push_grow(void *, _Bool, void (*)(void *));
extern int64_t plum_array_push_length(int64_t);
extern void plum_rc_inc(void *);
extern size_t malloc_usable_size(void *);
extern int64_t plum_heap_snapshot(void), plum_heap_snapshot_field(int64_t, int64_t);
extern int64_t plum_n_alloc, plum_b_alloc;
static int attempts, fail_first, fail_all, heap_drops, children_freed;
static int64_t requests[2];
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "array-push: %s at %d\n", #x, __LINE__); exit(2); } } while (0)
void *push_test_malloc(int64_t n) {
    if (attempts < 2) requests[attempts] = n;
    attempts++;
    return fail_all || (fail_first && attempts == 1) ? NULL : malloc((size_t)n);
}
static int64_t live(void) {
    int64_t t = plum_heap_snapshot();
    return t ? plum_heap_snapshot_field(t, 3) : 0;
}
static Array *array(int64_t n, int64_t cap) {
    int64_t bytes = 16 + 8 * cap;
    Array *a = malloc((size_t)bytes); CHECK(a);
    a->rc = 1; a->len = n;
    for (int64_t i = 0; i < n; i++) a->data[i] = (uint64_t)i;
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
    if (c->rc < 0 || --c->rc) return;
    children_freed++; plum_free(c);
}
static void heap_drop(void *p) {
    heap_drops++;
    Array *a = p;
    if (a->rc < 0 || --a->rc) return;
    for (int64_t i = 0; i < a->len; i++) child_drop((Child *)(uintptr_t)a->data[i]);
    plum_free(a);
}
static void scalar_case(int64_t n, int spare, int shared) {
    int64_t baseline = live(); Array *a = array(n, n + (spare ? 64 : 0));
    if (shared) plum_rc_inc(a);
    int reuse = !shared && malloc_usable_size(a) >= (uint64_t)(16 + 8 * (n + 1));
    uintptr_t id = (uintptr_t)a; attempts = 0;
    Array *out = plum_array_push_grow(a, 0, scalar_drop);
    out->data[n] = (uint64_t)n;
    CHECK(out->len == n + 1 && ((uintptr_t)out == id) == reuse);
    CHECK(attempts == (reuse ? 0 : 1));
    for (int64_t i = 0; i <= n; i++) CHECK(out->data[i] == (uint64_t)i);
    if (shared) {
        CHECK(a->len == n);
        for (int64_t i = 0; i < n; i++) CHECK(a->data[i] == (uint64_t)i);
        CHECK(requests[0] == (n ? 16 + 8 * (n + 1) : 32));
    }
    scalar_drop(out); if (shared) scalar_drop(a);
    CHECK(live() == baseline);
}
static void heap_case(int shared, int spare) {
    int64_t baseline = live(); int freed = children_freed;
    Child *c = malloc(sizeof *c); CHECK(c); c->rc = 17; c->value = 7;
#ifdef PLUM_HEAP_STATS
    plum_heap_adopt_origin(c, sizeof *c, PLUM_ORIGIN_RECORD);
#endif
    Array *a = array(16, spare ? 64 : 16);
    for (int i = 0; i < 16; i++) a->data[i] = (uint64_t)(uintptr_t)c;
    if (!shared && !spare) {
        int64_t cap = ((int64_t)malloc_usable_size(a) - 16) / 8;
        while (a->len < cap) {
            c->rc++; a = plum_array_push_grow(a, 1, heap_drop);
            a->data[a->len - 1] = (uint64_t)(uintptr_t)c;
        }
    }
    int64_t n = a->len;
    if (shared) plum_rc_inc(a);
    c->rc++; /* The appended value is owned before the helper runs. */
    int drops = heap_drops; attempts = 0;
    Array *out = plum_array_push_grow(a, 1, heap_drop);
    out->data[n] = (uint64_t)(uintptr_t)c;
    CHECK(out->len == n + 1 && c->rc == n + 2 + (shared ? n : 0));
    CHECK(heap_drops == drops + (shared ? 1 : 0));
    CHECK(attempts == (shared || !spare ? 1 : 0));
    for (int64_t i = 0; i <= n; i++) CHECK(out->data[i] == (uint64_t)(uintptr_t)c);
    heap_drop(out); if (shared) heap_drop(a);
    CHECK(c->rc == 1); child_drop(c);
    CHECK(children_freed == freed + 1 && live() == baseline);
}
static void retry_case(void) {
    int64_t baseline = live(); Array *a = array(16, 16);
    int64_t cap = ((int64_t)malloc_usable_size(a) - 16) / 8;
    while (a->len < cap) {
        int64_t value = a->len; a = plum_array_push_grow(a, 0, scalar_drop);
        a->data[value] = (uint64_t)value;
    }
    int64_t next = a->len, old_n = plum_n_alloc, old_bytes = plum_b_alloc;
    attempts = 0; fail_first = 1;
    Array *out = plum_array_push_grow(a, 0, scalar_drop);
    out->data[next] = (uint64_t)next; fail_first = 0;
    CHECK(attempts == 2 && requests[0] > requests[1] && requests[1] == 16 + 8 * (next + 1));
    CHECK(plum_n_alloc == old_n + 1 && plum_b_alloc == old_bytes + requests[1]);
    for (int64_t i = 0; i <= next; i++) CHECK(out->data[i] == (uint64_t)i);
#ifdef PLUM_HEAP_STATS
    CHECK(live() == baseline + requests[1]);
#endif
    scalar_drop(out); CHECK(live() == baseline);
}
static void static_seed(void) {
    struct { int64_t rc, len; } a = {-1, 0}; attempts = 0;
    Array *out = plum_array_push_grow(&a, 0, scalar_drop); out->data[0] = 42;
    CHECK(a.rc == -1 && a.len == 0 && out->len == 1 && requests[0] == 32);
    scalar_drop(out);
}
static void fatal(const char *kind) {
    struct { int64_t rc, len; } a = {2, 1};
    if (!strcmp(kind, "negative")) a.len = -1;
    else if (!strcmp(kind, "large")) a.len = INT64_MAX;
    else if (!strcmp(kind, "limit")) a.len = 1152921504606846973;
    else if (!strcmp(kind, "oom")) fail_all = 1;
    else exit(3);
    /* Validation/failure precedes payload access and parent disposal. */
    plum_array_push_grow(&a, 0, scalar_drop); exit(4);
}
int main(int argc, char **argv) {
    if (argc == 2) fatal(argv[1]);
    CHECK(plum_array_push_length(1152921504606846972) == 1152921504606846973);
    for (int shared = 0; shared < 2; shared++) {
        for (int64_t n = 0; n < 65; n++) {
            scalar_case(n, 0, shared); scalar_case(n, 1, shared);
        }
        heap_case(shared, 0); heap_case(shared, 1);
    }
    retry_case(); static_seed();
    puts("array-push: exact copies, owned transfer, reserve retry, checked sizes and accounting passed");
    return 0;
}
