#include <stddef.h>
#include <string.h>
const char *null_pointer(void) { return NULL; }
long long inspect_raw(const char *pointer) { return (long long)strlen(pointer); }
