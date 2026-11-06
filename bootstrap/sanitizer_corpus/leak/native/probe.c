#include <stdlib.h>
/* Volatile access keeps the intentional allocation observable at -O2. Do not
 * retain its pointer in globals or return it: it must be unreachable at exit. */
void lose_allocation(void) {
    volatile char *p = malloc(197);
    if (!p) abort();
    *p = 7;
}
