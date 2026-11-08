#include <stdint.h>
#include <stdio.h>
static int64_t closed;
void close_resource(int64_t id) {
    ++closed;
    printf("drop %lld\n", (long long)id);
}
int64_t cleanup_count(void) { return closed; }
