#include <stdint.h>
#include <stdlib.h>

static int64_t count, ids[32];
void concat_close(int64_t n) {
    if (count >= 32) abort();
    ids[count++] = n;
}
int64_t concat_count(void) { return count; }
int64_t concat_at(int64_t i) {
    if (i < 0 || i >= count) abort();
    return ids[i];
}

/* Include NaN payloads, signed zero, subnormals and infinity. */
#include <string.h>
double concat_pattern(int64_t i) {
    static const uint64_t bits[] = { UINT64_C(0x7ff8000000000042),
        UINT64_C(0x8000000000000000), UINT64_C(1), UINT64_C(0x7ff0000000000000) };
    if (i < 0 || i >= 4) abort();
    double n;
    memcpy(&n, &bits[i], sizeof n);
    return n;
}
int64_t concat_bits(double n) {
    int64_t bits;
    memcpy(&bits, &n, sizeof bits);
    return bits;
}
