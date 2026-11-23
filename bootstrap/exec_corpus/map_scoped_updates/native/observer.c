#include <stdint.h>
#include <stdlib.h>

static uint64_t closed;
static int64_t count;

void map_scoped_close(int64_t id) {
    if (id < 0 || id >= 49) abort();
    uint64_t bit = UINT64_C(1) << id;
    if (closed & bit) abort();
    closed |= bit;
    count++;
}

int64_t map_scoped_closed(void) { return count; }
