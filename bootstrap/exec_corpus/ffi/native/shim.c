#include <string.h>
long long shim_add(long long a, long long b) { return a + b; }
double shim_scale(double x, double f) { return x * f; }
int shim_is_even(long long n) { return n % 2 == 0; }
long long shim_strlen(const char *s) { return (long long)strlen(s); }
const char *shim_greeting(void) { return "hello from C"; }
void shim_noop(void) { }

/* Truthy but NOT 1 -- C's int is 32 bits and Plum's Bool is i1, so a
   crossing that truncates instead of comparing reads this as false. */
int shim_returns_two(void) { return 2; }
long long shim_takes_bool(int b) { return b ? 100 : 200; }
