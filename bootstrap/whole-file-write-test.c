// Run the real writer against real files, injecting failures at each boundary.
// Verify contents and native lifetimes before the next call, not only at exit.
#define _FILE_OFFSET_BITS 64
#if defined(__APPLE__)
#define _DARWIN_C_SOURCE 1
#endif
#define _POSIX_C_SOURCE 200809L
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

enum fault { NONE, ALLOC, STAT, CREATE, STREAM, SHORT_WRITE, WRITE_ERROR,
             MODE, CLOSE, RENAME };
static enum fault fault;
static int buffers, descriptors, temporaries;
static size_t allocated;
static char scratch[4096];

static void *test_malloc(size_t n) {
    if (fault == ALLOC) return NULL;
    void *p = malloc(n);
    assert(p != NULL);
    buffers++;
    allocated += n;
    return p;
}
static void test_free(void *p) {
    if (p) { buffers--; free(p); }
}
static int test_stat(const char *path, struct stat *st) {
    if (fault == STAT) { errno = EACCES; return -1; }
    return lstat(path, st);
}
static int test_create(char *path) {
    if (fault == CREATE) return -1;
    int fd = mkstemp(path);
    assert(fd >= 0 && strlen(path) < sizeof(scratch));
    strcpy(scratch, path);
    descriptors++;
    temporaries++;
    return fd;
}
static FILE *test_stream(int fd, const char *mode) {
    return fault == STREAM ? NULL : fdopen(fd, mode);
}
static size_t test_write(const void *p, size_t size, size_t count, FILE *f) {
    assert(size == 1);
    return fwrite(p, size, fault == SHORT_WRITE ? count / 2 : count, f);
}
static int test_error(FILE *f) {
    return fault == WRITE_ERROR || ferror(f);
}
static int test_mode(int fd, mode_t mode) {
    return fault == MODE ? -1 : fchmod(fd, mode);
}
static int test_close_stream(FILE *f) {
    descriptors--;
    int r = fclose(f);
    return fault == CLOSE ? -1 : r;
}
static int test_close(int fd) {
    descriptors--;
    return close(fd);
}
static int test_unlink(const char *path) {
    assert(strcmp(path, scratch) == 0 && descriptors == 0);
    temporaries--;
    return unlink(path);
}
long long os_rename_file(const char *from, const char *to) {
    assert(descriptors == 0 && temporaries == 1);
    if (fault == RENAME) return -1;
    int r = rename(from, to);
    if (r == 0) temporaries--;
    return r;
}

#define malloc test_malloc
#define free test_free
#define lstat test_stat
#define mkstemp test_create
#define fdopen test_stream
#define fwrite test_write
#define ferror test_error
#define fchmod test_mode
#define fclose test_close_stream
#define close test_close
#define unlink test_unlink
#include "../native_stdlib/whole_file_shim.c"
#undef malloc
#undef free
#undef lstat
#undef mkstemp
#undef fdopen
#undef fwrite
#undef ferror
#undef fchmod
#undef fclose
#undef close
#undef unlink

static void check(const char *path, enum fault which, const char *data, size_t n) {
    FILE *f = fopen(path, "wb");
    assert(f && fwrite("old", 1, 3, f) == 3 && fclose(f) == 0);
    assert(chmod(path, 0751) == 0);
    fault = which;
    allocated = 0;
    long long r = plum_write_whole_file(path, data, (long long)n);
    assert((r == 0) == (which == NONE));
    assert(buffers == 0 && descriptors == 0 && temporaries == 0);
    assert(allocated <= strlen(path) + 64); // no payload-sized scratch
    if (scratch[0]) assert(access(scratch, F_OK) != 0);
    f = fopen(path, "rb");
    assert(f);
    const char *want = which == NONE ? data : "old";
    size_t length = which == NONE ? n : 3;
    for (size_t i = 0; i < length; i++) assert(fgetc(f) == (unsigned char)want[i]);
    assert(fgetc(f) == EOF && fclose(f) == 0);
    struct stat st;
    assert(stat(path, &st) == 0 && (st.st_mode & 0777) == 0751);
}

int main(int argc, char **argv) {
    assert(argc == 2);
    const char data[] = {'n', 0, (char)255, '\r', '\n'};
    for (int i = 0; i < 100; i++) {
        for (enum fault f = NONE; f <= RENAME; f++) check(argv[1], f, data, sizeof(data));
    }
    check(argv[1], NONE, "", 0);
    char *large = malloc(1024 * 1024);
    assert(large);
    memset(large, 'x', 1024 * 1024);
    check(argv[1], NONE, large, 1024 * 1024);
    free(large);
    char long_path[4096];
    const char *slash = strrchr(argv[1], '/');
    size_t prefix = slash == NULL ? 0 : (size_t)(slash - argv[1]) + 1;
    assert(prefix + 256 < sizeof(long_path));
    memcpy(long_path, argv[1], prefix);
    memset(long_path + prefix, 'a', 255);
    long_path[prefix + 255] = 0;
    check(long_path, NONE, data, sizeof(data));
    assert(unlink(long_path) == 0);
    assert(unlink(argv[1]) == 0);
    fault = NONE;
    assert(plum_write_whole_file(argv[1], data, sizeof(data)) == 0);
    struct stat st;
    assert(stat(argv[1], &st) == 0 && (st.st_mode & 0777) == 0600);
    assert(buffers == 0 && descriptors == 0 && temporaries == 0);
    puts("whole-file writer: failures preserve contents; bounded scratch and resources released before return");
}
