#include <stdint.h>
#include <stdlib.h>
static int64_t closed[3];
void empty_close(int64_t n) {
    if (n < 1 || n >= 3 || closed[n]) abort();
    closed[n]++;
}
int64_t empty_closed(int64_t n) {
    if (n < 1 || n >= 3) abort();
    return closed[n];
}
