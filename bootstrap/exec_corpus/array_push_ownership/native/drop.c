#include <stdint.h>
#include <stdlib.h>
#include <string.h>
static int64_t closed[129], total;
void push_close(int64_t n) {
    if (n < 0 || n >= 129 || closed[n]) abort();
    closed[n]++; total++;
}
int64_t push_closed(int64_t n) { if (n < 0 || n >= 129) abort(); return closed[n]; }
int64_t push_total(void) { return total; }
double push_pattern(int64_t i) {
    static const uint64_t bits[] = { UINT64_C(0x7ff8000000000042), UINT64_C(0x8000000000000000), UINT64_C(1), UINT64_C(0x7ff0000000000000) };
    if (i < 0 || i >= 4) abort();
    double n; memcpy(&n, &bits[i], sizeof n); return n;
}
int64_t push_bits(double n) { int64_t bits; memcpy(&bits, &n, sizeof bits); return bits; }
