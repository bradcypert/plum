#ifndef PLUM_NATIVE_BUFFER_H
#define PLUM_NATIVE_BUFFER_H
/* NativeBuffer owns mutable storage independently of immutable Plum cells.
 * Tokens are private to the standard-library wrapper. An acquired lease
 * retains the resource and keeps its address/capacity stable until end.
 * Raw pointers may not escape it. Status: 0 success, -1 bounds/termination,
 * -2 busy, -3 allocation failure. Acquisition returns a token or -1/-2.
 * Mode 0 reads, 1 writes binary, 2 edits bounded NUL-terminated text. */
long long plum_buffer_create(long long capacity);
void plum_buffer_release(long long buffer);
long long plum_buffer_length(long long buffer);
long long plum_buffer_capacity(long long buffer);
long long plum_buffer_reserve(long long buffer, long long capacity);
long long plum_buffer_write(long long buffer, const unsigned char *data, long long length);
long long plum_buffer_acquire(long long buffer, long long mode);
unsigned char *plum_buffer_data(long long lease);
long long plum_buffer_commit(long long lease, long long length);
long long plum_buffer_text_length(long long lease);
void plum_buffer_end(long long lease);
long long plum_buffer_snapshot(void);
long long plum_buffer_snapshot_field(long long token, long long field);
#endif
