#include <assert.h>
#include <stdio.h>
static long long next = 1024;
void growth_drop(long long h) {
    assert(h == --next);
    if (next == 0) puts("cleaned 1024 handles in reverse order");
}
