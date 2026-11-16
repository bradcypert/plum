#include <stdint.h>
#include <stdlib.h>
static int64_t total;
void scope_close(int64_t n) { if (n != total) abort(); total++; }
int64_t scope_total(void) { return total; }
int64_t scope_value(void) { return 7; }
