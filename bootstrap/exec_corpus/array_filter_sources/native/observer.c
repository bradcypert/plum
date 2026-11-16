#include <stdint.h>
#include <stdlib.h>
static int64_t closed[11], order[11], total;
void filter_close(int64_t n) {
    if (n < 0 || n >= 11 || closed[n] || total >= 11) abort();
    closed[n]++; order[total++] = n;
}
int64_t filter_total(void) { return total; }
int64_t filter_at(int64_t i) { if (i < 0 || i >= total) abort(); return order[i]; }
