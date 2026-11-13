#include <stddef.h>
#include <stdint.h>

/* Compile with -ffreestanding -fno-builtin, as Plum's profile does, so loop
 * idiom recognition cannot turn these implementations into self-calls. */
void *memcpy(void *destination, const void *source, size_t length) {
    uint8_t *out = destination;
    const uint8_t *in = source;
    for (size_t i = 0; i < length; ++i) out[i] = in[i];
    return destination;
}

void *memmove(void *destination, const void *source, size_t length) {
    uint8_t *out = destination;
    const uint8_t *in = source;
    if ((uintptr_t)out < (uintptr_t)in) {
        for (size_t i = 0; i < length; ++i) out[i] = in[i];
    } else {
        for (size_t i = length; i > 0; --i) out[i - 1] = in[i - 1];
    }
    return destination;
}

void *memset(void *destination, int value, size_t length) {
    uint8_t *out = destination;
    for (size_t i = 0; i < length; ++i) out[i] = (uint8_t)value;
    return destination;
}

int memcmp(const void *left, const void *right, size_t length) {
    const uint8_t *a = left;
    const uint8_t *b = right;
    for (size_t i = 0; i < length; ++i) {
        if (a[i] != b[i]) return a[i] < b[i] ? -1 : 1;
    }
    return 0;
}
