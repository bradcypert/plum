#include <stddef.h>
#include <string.h>

long long inspect(const char *value) { return (long long)strlen(value); }
long long inspect_bytes(const unsigned char *value, long long length) {
    return length == 3 && value[0] == 65 && value[1] == 0 && value[2] == 66;
}
long long inspect_pair(const char *value, long long ignored) {
    (void)ignored;
    return inspect(value);
}
const char *native_suffix(const char *value) { return value + 1; }
const char *native_literal(void) { return "native"; }

long long inspect_raw(const char *value) { return inspect(value); }
