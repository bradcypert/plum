// Exercise real stream lifetimes and deterministic native failures.
#include <assert.h>
#include <dirent.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int fail_at, fail_read, allocations, live_allocations, streams;
static void *test_malloc(size_t n) {
    if (++allocations == fail_at) return NULL;
    void *p = malloc(n);
    if (p) live_allocations++;
    return p;
}
static void test_free(void *p) {
    if (p) { assert(live_allocations > 0); live_allocations--; }
    free(p);
}

// Intercept the native stream API actually used by each adapter. Windows
// converts the path before opening, then allocates two stream wrappers.
#if defined(_WIN32)
#include <windows.h>
#include <direct.h>
#include <wchar.h>
static _WDIR *test_opendir(const wchar_t *path) {
    _WDIR *d = _wopendir(path);
    if (d) streams++;
    return d;
}
static int test_closedir(_WDIR *d) { streams--; return _wclosedir(d); }
static struct _wdirent *test_readdir(_WDIR *d) {
    if (fail_read) { errno = EIO; return NULL; }
    return _wreaddir(d);
}
#define _wopendir test_opendir
#define _wclosedir test_closedir
#define _wreaddir test_readdir
#else
static DIR *test_opendir(const char *path) {
    DIR *d = opendir(path);
    if (d) streams++;
    return d;
}
static int test_closedir(DIR *d) { streams--; return closedir(d); }
static struct dirent *test_readdir(DIR *d) {
    if (fail_read) { errno = EIO; return NULL; }
    return readdir(d);
}
#define opendir test_opendir
#define closedir test_closedir
#define readdir test_readdir
#endif
#define malloc test_malloc
#define free test_free
#include "../native_stdlib/dir_shim.c"
#undef malloc
#undef free
#if defined(_WIN32)
#undef _wopendir
#undef _wclosedir
#undef _wreaddir
#else
#undef opendir
#undef closedir
#undef readdir
#endif

int main(void) {
    assert(dir_open("/no/such/plum-directory-128") < 0);
    assert(streams == 0 && live_allocations == 0);
    allocations = 0;
    long long opened = dir_open("bootstrap");
    assert(opened >= 0 && streams == 1);
    int opening_allocations = allocations;
    assert(opening_allocations > 0);
    dir_close(opened);
    assert(streams == 0 && live_allocations == 0);
    for (int failure = 1; failure <= opening_allocations; failure++) {
        allocations = 0; fail_at = failure;
        assert(dir_open("bootstrap") < 0);
        assert(allocations == failure);
        assert(streams == 0 && live_allocations == 0);
    }
    fail_at = 0;

    for (int round = 0; round < 256; round++) {
        long long a = dir_open("bootstrap"), b = dir_open("native_stdlib");
        assert(a != -1 && b != -1);
        int allocated = allocations;
        long long n = dir_next_size(a);
        assert(n > 0);
        const char *borrowed = dir_entry_data(a);
        char *copy = strdup(borrowed);
        assert(copy != NULL);
        while (dir_next_size(b) > 0) {}
        dir_close(b);
        assert(strcmp(borrowed, copy) == 0); // another handle cannot overwrite it
        free(copy);
        for (;;) {
            errno = EIO; // unrelated caller errno must not turn EOF into error
            n = dir_next_size(a);
            assert(n >= 0);
            if (n == 0) break;
            assert((size_t)n == strlen(dir_entry_data(a)));
            assert(strcmp(dir_entry_data(a), ".") && strcmp(dir_entry_data(a), ".."));
        }
        assert(allocations == allocated); // NO allocation per entry
        dir_close(a);
        assert(streams == 0 && live_allocations == 0);
    }
    long long h = dir_open("bootstrap");
    assert(h != -1 && dir_next_size(h) > 0);
    fail_read = 1;
    assert(dir_next_size(h) == -1); // read failure after a successful entry
    assert(dir_entry_data(h)[0] == '\0');
    dir_close(h);
    dir_close(0);
    assert(dir_next_size(0) == -1);
    assert(streams == 0 && live_allocations == 0);
    puts("directory native: ok");
}
