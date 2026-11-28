#include <stdint.h>
#include <stdlib.h>
extern int64_t plum_heap_snapshot(void), plum_heap_snapshot_field(int64_t, int64_t);
enum { resource_count = 15 };
static int64_t closed[resource_count], order[resource_count], total;
void fold_close(int64_t n) {
    if (n < 0 || n >= resource_count || closed[n] || total >= resource_count) abort();
    closed[n]++; order[total++] = n;
}
int64_t fold_total(void) { return total; }
int64_t fold_at(int64_t i) { if (i < 0 || i >= total) abort(); return order[i]; }
int64_t fold_live(void) { int64_t t = plum_heap_snapshot(); return t ? plum_heap_snapshot_field(t, 3) : -1; }
