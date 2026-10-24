// Deterministic fault injection into the real native reader. Output-only
// tests cannot force allocation, positioning or late read/close failures,
// or prove cleanup happened before the next operation.
#define _FILE_OFFSET_BITS 64
#define _POSIX_C_SOURCE 200809L
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum fault {
    NONE, OPEN, SEEK_END_FAIL, TELL, OVERSIZE, SEEK_SET_FAIL, ALLOC,
    SHORT_READ, READ_ERROR, FULL_READ_ERROR, CLOSE, LARGE_ALLOC
};
static enum fault fault;
static FILE *stream;
static int opened, seeks, tells, allocations, reads, errors, closes, frees;
static size_t requested;
static void *buffer;
static const unsigned char payload[] = {'a', 0, 255, '\r', '\n', 'z'};
static size_t payload_size = sizeof(payload);

static FILE *test_open(const char *path, const char *mode) {
    assert(strcmp(path, "fixture") == 0 && strcmp(mode, "rb") == 0);
    if (fault == OPEN) return NULL;
    opened++;
    return stream;
}

static int test_seek(FILE *f, int64_t offset, int origin) {
    assert(f == stream && offset == 0 && closes == 0);
    seeks++;
    assert(origin == (seeks == 1 ? SEEK_END : SEEK_SET));
    // Nonzero is failure, not only -1.
    return ((fault == SEEK_END_FAIL && seeks == 1) ||
            (fault == SEEK_SET_FAIL && seeks == 2)) ? 1 : 0;
}

static int64_t test_tell(FILE *f) {
    assert(f == stream && seeks == 1 && closes == 0);
    tells++;
    if (fault == TELL) return -1;
    if (fault == OVERSIZE) return INT64_MAX - 16;
    if (fault == LARGE_ALLOC) return INT64_C(4294967299);
    return (int64_t)payload_size;
}

static void *test_malloc(size_t size) {
    assert(buffer == NULL && seeks == 2 && tells == 1 && closes == 0);
    allocations++;
    requested = size;
    if (fault == ALLOC || fault == LARGE_ALLOC) return NULL;
    assert(size == payload_size + 1);
    buffer = malloc(size);
    assert(buffer != NULL);
    return buffer;
}

static size_t test_read(void *dest, size_t size, size_t count, FILE *f) {
    assert(dest == buffer && size == 1 && count == payload_size);
    assert(f == stream && allocations == 1 && closes == 0);
    reads++;
    size_t got = (fault == SHORT_READ || fault == READ_ERROR) ? count / 2 : count;
    memcpy(dest, payload, got);
    return got;
}

static int test_error(FILE *f) {
    assert(f == stream && reads == 1 && closes == 0);
    errors++;
    return fault == READ_ERROR || fault == FULL_READ_ERROR;
}

static int test_close(FILE *f) {
    assert(f == stream && opened == 1 && closes == 0);
    closes++;
    return fault == CLOSE ? -1 : 0;
}

static void test_free(void *p) {
    if (p == NULL) return;
    assert(p == buffer && closes == 1);
    frees++;
    free(p);
    buffer = NULL;
}

#define fopen test_open
#define fseeko test_seek
#define ftello test_tell
#define _fseeki64 test_seek
#define _ftelli64 test_tell
#define malloc test_malloc
#define fread test_read
#define ferror test_error
#define fclose test_close
#define free test_free
#include "../native_stdlib/whole_file_shim.c"
#undef fopen
#undef fseeko
#undef ftello
#undef _fseeki64
#undef _ftelli64
#undef malloc
#undef fread
#undef ferror
#undef fclose
#undef free

static void check(enum fault which) {
    fault = which;
    opened = seeks = tells = allocations = reads = errors = closes = frees = 0;
    requested = 0;
    long long length = -99;
    char *data = plum_read_whole_file("fixture", &length);
    assert(closes == opened);
    if (which == NONE) {
        assert(data != NULL && length == (long long)payload_size);
        assert(memcmp(data, payload, payload_size) == 0 && data[length] == 0);
        assert(reads == 1 && errors == 1 && frees == 0);
        test_free(data);
    } else {
        assert(data == NULL && length == 0 && buffer == NULL);
        assert(frees == (which >= SHORT_READ && which <= CLOSE ? 1 : 0));
        if (which <= SEEK_SET_FAIL) assert(allocations == 0 && reads == 0);
        if (which == ALLOC || which == LARGE_ALLOC) assert(allocations == 1 && reads == 0);
        if (which == LARGE_ALLOC) assert(requested == UINT64_C(4294967300));
    }
    assert(buffer == NULL);
}

int main(void) {
    stream = tmpfile();
    assert(stream != NULL);
    // Check native lifetimes on every iteration, before process exit.
    for (int i = 0; i < 1000; i++) {
        for (enum fault f = NONE; f <= LARGE_ALLOC; f++) check(f);
    }
    payload_size = 0;
    check(NONE);
    assert(requested == 1);
    fclose(stream);
    puts("whole-file reader: checked failures close files and free buffers; empty and binary reads survive");
    return 0;
}
