#include <stdint.h>
#include <stdlib.h>

static int64_t closed[7];
void reset_close(int64_t n) {
    if (n < 1 || n >= 7 || closed[n] != 0) abort();
    closed[n]++;
}
int64_t reset_closed(int64_t n) {
    if (n < 1 || n >= 7) abort();
    return closed[n];
}
