#include <stdint.h>
#include <stdlib.h>

static uint64_t closed;

void zip_close(int64_t n) {
    uint64_t bit = UINT64_C(1) << n;
    if (closed & bit) abort();
    closed |= bit;
}

int64_t zip_closed(void) { return (int64_t)closed; }
