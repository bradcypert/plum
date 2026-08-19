#include <string.h>
long long shim_add(long long a, long long b) { return a + b; }
double shim_scale(double x, double f) { return x * f; }
int shim_is_even(long long n) { return n % 2 == 0; }
long long shim_strlen(const char *s) { return (long long)strlen(s); }
const char *shim_greeting(void) { return "hello from C"; }
void shim_noop(void) { }
