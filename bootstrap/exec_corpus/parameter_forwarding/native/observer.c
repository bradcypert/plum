#include <stdint.h>
#include <stdlib.h>
static int64_t total;
void forward_close(int64_t n) { if (n != total) abort(); total++; }
int64_t forward_total(void) { return total; }
