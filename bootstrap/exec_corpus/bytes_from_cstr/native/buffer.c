#include <stdint.h>

static unsigned char buffer[] = {65, 0, 255, 66};
const char *copy_test_data(void) { return (const char *)buffer; }
const char *copy_test_null(void) { return 0; }
void copy_test_reuse(void) { buffer[0] = 90; }
