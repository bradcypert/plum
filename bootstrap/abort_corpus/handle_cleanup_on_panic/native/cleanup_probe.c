// An `on_drop` whose running is visible in the program's output, which
// is the only way a fixture can assert that cleanup happened.
#include <stdio.h>
void cleanup_probe(long long h) { printf("cleanup %lld\n", h); fflush(stdout); }
