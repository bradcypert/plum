// Test-only mutation: keep malformed bytes out of Plum String values.
#include <assert.h>
#include <stdlib.h>

void environment_test_set(const char *value) {
#ifdef _WIN32
    assert(_putenv_s("PLUM_TEST_ENV_135", value) == 0);
#else
    assert(setenv("PLUM_TEST_ENV_135", value, 1) == 0);
#endif
}

void environment_test_unset(void) {
#ifdef _WIN32
    assert(_putenv_s("PLUM_TEST_ENV_135", "") == 0);
#else
    assert(unsetenv("PLUM_TEST_ENV_135") == 0);
#endif
}
