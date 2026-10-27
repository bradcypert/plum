#include <assert.h>
static long long drops;
void channel_probe_drop(long long h) { assert(h == 42); drops++; }
long long channel_probe_count(void) { return drops; }
