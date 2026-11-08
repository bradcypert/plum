#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
static int64_t closed;
static const int64_t expected[] = {
    6, 5, 4, 3, 2, 1, 13, 12, 11, 22, 21, 12, 11, 2, 1, 32, 31, 33
};
void close_resource(int64_t id) {
    if (closed >= (int64_t)(sizeof(expected) / sizeof(expected[0])) || expected[closed] != id) {
        fprintf(stderr, "unexpected statement cleanup at %lld: got %lld\n", (long long)closed, (long long)id);
        abort();
    }
    ++closed;
    printf("drop %lld\n", (long long)id);
}
int64_t cleanup_count(void) { return closed; }
