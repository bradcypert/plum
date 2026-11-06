#include <stdlib.h>
/* Deliberately violate the borrowed-pointer contract. The next length load
 * happens in generated Plum IR, not this native helper. */
void invalidate(const char *bytes) { free((char *)bytes - 16); }
