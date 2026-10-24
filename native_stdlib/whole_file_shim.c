// Whole-file byte reads for the generated runtime. Keep libc's FILE,
// size_t and positioning types on the C side of the boundary: in
// particular, Windows long/ftell are only 32 bits even on 64-bit builds.
// The caller owns the returned buffer and must free it after copying it
// into a Plum cell. NULL means failure; an empty file still owns a buffer.
#if !defined(_WIN32)
#ifndef _FILE_OFFSET_BITS
#define _FILE_OFFSET_BITS 64
#endif
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#endif

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#if defined(_WIN32)
#define PLUM_WHOLE_SEEK _fseeki64
#define PLUM_WHOLE_TELL _ftelli64
#else
#define PLUM_WHOLE_SEEK fseeko
#define PLUM_WHOLE_TELL ftello
#endif

char *plum_read_whole_file(const char *path, long long *length) {
    *length = 0;
    FILE *f = fopen(path, "rb");
    if (f == NULL) return NULL;
    char *buf = NULL;

    if (PLUM_WHOLE_SEEK(f, 0, SEEK_END) != 0) goto fail;
    int64_t size = PLUM_WHOLE_TELL(f);
    // Also reserve the runtime cell's refcount, length and trailing NUL
    // (17 bytes). Neither native nor Plum allocation sizes may overflow.
    if (size < 0 || (uint64_t)size > INT64_MAX - 17 ||
        (uint64_t)size > SIZE_MAX - 17) goto fail;
    if (PLUM_WHOLE_SEEK(f, 0, SEEK_SET) != 0) goto fail;

    buf = malloc((size_t)size + 1);
    if (buf == NULL) goto fail;
    size_t got = fread(buf, 1, (size_t)size, f);
    if (ferror(f) || got != (size_t)size) goto fail;
    if (fclose(f) != 0) {
        free(buf);
        return NULL;
    }
    buf[got] = '\0';
    *length = size;
    return buf;

fail:
    fclose(f);
    free(buf);
    return NULL;
}
