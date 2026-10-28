#if defined(_WIN32)
#include "generated-output-windows-test.c"
#else
// Exercise strict traversal and transaction cleanup against real files.
#if defined(__APPLE__)
#define _DARWIN_C_SOURCE 1
#endif
#define _POSIX_C_SOURCE 200809L
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

enum fault { NONE, ALLOC, ANCHOR, WALK, MKDIR, STAT, CREATE, STREAM,
             SHORT_WRITE, WRITE_ERROR, MODE, CLOSE, RENAME };
static enum fault fault;
static int buffers, descriptors, temporaries;
static size_t allocated;
static const char *swap_path, *swap_parked, *swap_target;
static void *test_malloc(size_t n) {
    if (fault == ALLOC) return NULL;
    void *p = malloc(n);
    if (p) { buffers++; allocated += n; }
    return p;
}
static char *test_strdup(const char *s) {
    char *p = test_malloc(strlen(s) + 1);
    if (p) strcpy(p, s);
    return p;
}
static void test_free(void *p) { if (p) { buffers--; free(p); } }
static int test_open(const char *path, int flags) {
    if (fault == ANCHOR) return -1;
    int fd = open(path, flags);
    if (fd >= 0) descriptors++;
    return fd;
}
static int test_openat(int dir, const char *path, int flags, ...) {
    int create = (flags & O_CREAT) != 0;
    if (fault == (create ? CREATE : WALK)) return -1;
    int fd;
    if (create) {
        va_list args; va_start(args, flags);
        int mode = va_arg(args, int); va_end(args);
        fd = openat(dir, path, flags, mode);
    } else fd = openat(dir, path, flags);
    if (fd >= 0) { descriptors++; if (create) temporaries++; }
    return fd;
}
static int test_mkdirat(int dir, const char *path, mode_t mode) {
    if (fault == MKDIR) { errno = EACCES; return -1; }
    return mkdirat(dir, path, mode);
}
static int test_stat(int dir, const char *path, struct stat *st, int flags) {
    if (fault == STAT) { errno = EACCES; return -1; }
    return fstatat(dir, path, st, flags);
}
static int test_close(int fd) { descriptors--; return close(fd); }
static FILE *test_stream(int fd, const char *mode) {
    if (swap_path) {
        assert(rename(swap_path, swap_parked) == 0);
        assert(symlink(swap_target, swap_path) == 0);
        swap_path = NULL;
    }
    return fault == STREAM ? NULL : fdopen(fd, mode);
}
static size_t test_write(const void *p, size_t size, size_t n, FILE *f) {
    return fwrite(p, size, fault == SHORT_WRITE ? n / 2 : n, f);
}
static int test_error(FILE *f) { return fault == WRITE_ERROR || ferror(f); }
static int test_mode(int fd, mode_t mode) { return fault == MODE ? -1 : fchmod(fd, mode); }
static int test_close_stream(FILE *f) {
    descriptors--;
    int rc = fclose(f);
    return fault == CLOSE ? -1 : rc;
}
static int test_unlinkat(int dir, const char *path, int flags) {
    int rc = unlinkat(dir, path, flags);
    if (!rc) temporaries--;
    return rc;
}
static int test_renameat(int dir, const char *from, int dest, const char *to) {
    if (fault == RENAME) return -1;
    int rc = renameat(dir, from, dest, to);
    if (!rc) temporaries--;
    return rc;
}
long long os_rename_file(const char *from, const char *to) { return rename(from, to); }
#define malloc test_malloc
#define strdup test_strdup
#define free test_free
#define open test_open
#define openat test_openat
#define mkdirat test_mkdirat
#define fstatat test_stat
#define close test_close
#define fdopen test_stream
#define fwrite test_write
#define ferror test_error
#define fchmod test_mode
#define fclose test_close_stream
#define unlinkat test_unlinkat
#define renameat test_renameat
#include "../native_stdlib/whole_file_shim.c"
#undef malloc
#undef strdup
#undef free
#undef open
#undef openat
#undef mkdirat
#undef fstatat
#undef close
#undef fdopen
#undef fwrite
#undef ferror
#undef fchmod
#undef fclose
#undef unlinkat
#undef renameat

int main(int argc, char **argv) {
    assert(argc == 2);
    const char *path = argv[1];
    char *large = malloc(1024 * 1024);
    assert(large); memset(large, 'x', 1024 * 1024);
    for (int i = 0; i < 20; i++) {
        for (enum fault which = NONE; which <= RENAME; which++) {
            FILE *f = fopen(path, "wb");
            assert(f && fputs("old", f) >= 0 && fclose(f) == 0);
            assert(chmod(path, 0751) == 0);
            allocated = 0; fault = which;
            long long rc = plum_write_generated_file(path, large, 1024 * 1024);
            assert((rc == 0) == (which == NONE));
            // Check lifetimes before the next operation, not just at exit.
            assert(buffers == 0 && descriptors == 0 && temporaries == 0);
            assert(allocated <= strlen(path) + 1);
            f = fopen(path, "rb"); assert(f);
            size_t want = which == NONE ? 1024 * 1024 : 3;
            for (size_t j = 0; j < want; j++)
                assert(fgetc(f) == (which == NONE ? 'x' : "old"[j]));
            assert(fgetc(f) == EOF && fclose(f) == 0);
            struct stat st;
            assert(stat(path, &st) == 0 && (st.st_mode & 0777) == 0751);
        }
    }
    fault = NONE;
    assert(unlink(path) == 0);
    assert(plum_write_generated_file(path, "", 0) == 0);
    struct stat st;
    assert(stat(path, &st) == 0 && (st.st_mode & 0777) == 0600);
    assert(buffers == 0 && descriptors == 0 && temporaries == 0);
    // Replace the path after the parent and temporary are open. Publication
    // must stay on the original directory, never on the new symlink target.
    char parent[4096], parked[4096], outside[4096], page[8192];
    assert(snprintf(parent, sizeof(parent), "%s.parent", path) < (int)sizeof(parent));
    assert(snprintf(parked, sizeof(parked), "%s.parked", path) < (int)sizeof(parked));
    assert(snprintf(outside, sizeof(outside), "%s.outside", path) < (int)sizeof(outside));
    assert(mkdir(parent, 0700) == 0 && mkdir(outside, 0700) == 0);
    snprintf(page, sizeof(page), "%s/page", parent);
    swap_path = parent; swap_parked = parked; swap_target = outside;
    assert(plum_write_generated_file(page, "pinned", 6) == 0);
    assert(swap_path == NULL);
    snprintf(page, sizeof(page), "%s/page", outside);
    assert(access(page, F_OK) != 0);
    snprintf(page, sizeof(page), "%s/page", parked);
    FILE *f = fopen(page, "rb"); assert(f && fgetc(f) == 'p' && fclose(f) == 0);
    assert(unlink(page) == 0 && unlink(parent) == 0 && rmdir(parked) == 0 && rmdir(outside) == 0);
    assert(buffers == 0 && descriptors == 0 && temporaries == 0);
    free(large);
    puts("generated output: failure cleanup and path-sized scratch verified before return");
}

#endif
