#include <stdint.h>
#include <stdlib.h>
static int64_t closed[7], total;
void map_close(int64_t n) {
    if (n < 0 || n >= 7 || closed[n]) abort();
    closed[n]++; total++;
}
int64_t map_total(void) { return total; }
