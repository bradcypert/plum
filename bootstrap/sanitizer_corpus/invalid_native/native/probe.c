#include <stdlib.h>
void native_overflow(void) {
    volatile char *p = malloc(8);
    if (!p) abort();
    p[8] = 7;
    free((void *)p);
}
