#include <stdint.h>
#include <stdio.h>
#include <string.h>

int64_t bit_text_check(const char *text, int64_t value, int64_t hex) {
    char expected[65];
    const uint64_t bits = (uint64_t)value;
    const size_t length = hex ? 16 : 64;
    if (hex) {
        if (snprintf(expected, sizeof expected, "%016llx",
                     (unsigned long long)bits) != 16) return 0;
    } else {
        uint64_t mask = UINT64_C(1) << 63;
        for (size_t i = 0; i < 64; ++i, mask >>= 1)
            expected[i] = bits & mask ? '1' : '0';
        expected[64] = 0;
    }
    return text[length] == 0 && memcmp(text, expected, length + 1) == 0;
}
