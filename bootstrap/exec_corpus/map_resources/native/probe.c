#include <stdlib.h>
static long long count;
long long map_resource_open(void) { return ++count; }
void map_resource_close(long long value) {
    if (value <= 0 || count <= 0) abort();
    --count;
}
long long map_resource_live(void) { return count; }
