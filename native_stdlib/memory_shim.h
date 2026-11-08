#ifndef PLUM_MEMORY_SHIM_H
#define PLUM_MEMORY_SHIM_H
#include <stdint.h>
/* Physical allocation provenance, never the current occupant's type.
 * Keep the compiler/Memory inventories synchronized (checked by the harness). */
enum PlumAllocationOrigin {
    PLUM_ORIGIN_UNSPECIFIED, PLUM_ORIGIN_BYTES, PLUM_ORIGIN_ARRAY,
    PLUM_ORIGIN_RECORD, PLUM_ORIGIN_VARIANT, PLUM_ORIGIN_CLOSURE,
    PLUM_ORIGIN_REF, PLUM_ORIGIN_HANDLE, PLUM_ORIGIN_COUNT
};
#ifdef __cplusplus
extern "C" {
#endif
/* Internal, allocation-free fatal path for checked runtime constructors. */
void plum_memory_fatal(const char *message, int64_t length);
/* Generated-runtime cell boundary. Request the entire cell (at least the
 * eight-byte RC header); plum_alloc initializes RC=1. Native producers must
 * follow the cell's layout/ownership contract. Use plum_free for final cell
 * storage release, after releasing owned children. It also accepts untracked
 * native malloc pointers. Do not use libc free on a tracked Plum cell. */
void *plum_alloc(int64_t size);
void plum_free(void *pointer);
#ifdef PLUM_HEAP_STATS
/* Internal diagnostic boundary: register one successful raw allocation before
 * publishing its cell. The request/origin remain fixed through final release. */
void plum_heap_adopt_origin(void *pointer, int64_t size, int64_t origin);
#endif
#ifdef __cplusplus
}
#endif
#endif
