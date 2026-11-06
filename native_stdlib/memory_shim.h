#ifndef PLUM_MEMORY_SHIM_H
#define PLUM_MEMORY_SHIM_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
/* Generated-runtime cell boundary. Request the entire cell (at least the
 * eight-byte RC header); plum_alloc initializes RC=1. Native producers must
 * follow the cell's layout/ownership contract. Use plum_free for final cell
 * storage release, after releasing owned children. It also accepts untracked
 * native malloc pointers. Do not use libc free on a tracked Plum cell. */
void *plum_alloc(int64_t size);
void plum_free(void *pointer);
#ifdef __cplusplus
}
#endif
#endif
