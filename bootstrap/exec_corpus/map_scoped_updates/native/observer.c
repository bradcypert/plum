#include <stdint.h>
#include <stdlib.h>

static unsigned char closed[67];
static int64_t count;

void map_scoped_close(int64_t id) {
    if (id < 0 || id >= 67 || closed[id]) abort();
    closed[id] = 1;
    count++;
}

int64_t map_scoped_closed(void) { return count; }
