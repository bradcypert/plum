#include <stdint.h>

static int64_t closed;

void nested_close(int64_t value) {
    (void)value;
    closed++;
}

int64_t nested_closed(void) {
    return closed;
}
