#include <stdint.h>
extern void *plum_alloc(int64_t);
void lose_tracked_cell(void) {
    volatile char *p = plum_alloc(32);
    p[8] = 7;
}
