#include <stdint.h>
#include <stdlib.h>
static int64_t live, closed;
int64_t record_open(void) { return ++live; }
void record_close(int64_t n) { (void)n; if (live <= 0) abort(); live--; closed++; }
int64_t record_live(void) { return live; }
int64_t record_closed(void) { return closed; }
int64_t record_zero(void) { return 0; }
