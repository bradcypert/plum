#ifndef __has_feature
#define __has_feature(x) 0
#endif
long long native_instrumented(void) { return __has_feature(address_sanitizer); }
