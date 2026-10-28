// Exercise real stream lifetimes and deterministic native failures.
#include <assert.h>
#include <dirent.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int fail_alloc, fail_read, closes, allocations;
static void *test_malloc(size_t n) {
    allocations++;
    return fail_alloc ? NULL : malloc(n);
}
static int test_closedir(DIR *d) { closes++; return closedir(d); }
static struct dirent *test_readdir(DIR *d) {
    if (fail_read) { errno = EIO; return NULL; }
    return readdir(d);
}
#define malloc test_malloc
#define closedir test_closedir
#define readdir test_readdir
#include "../native_stdlib/dir_shim.c"
#undef malloc
#undef closedir
#undef readdir

int main(void) {
    assert(dir_open("/no/such/plum-directory-128") < 0);
    fail_alloc = 1;
    int before = closes;
    assert(dir_open("bootstrap") < 0);
    assert(closes == before + 1); // opendir succeeded, wrapper allocation failed
    fail_alloc = 0;

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
    }
    long long h = dir_open("bootstrap");
    assert(h != -1 && dir_next_size(h) > 0);
    fail_read = 1;
    assert(dir_next_size(h) == -1); // read failure after a successful entry
    assert(dir_entry_data(h)[0] == '\0');
    dir_close(h);
    dir_close(0);
    assert(dir_next_size(0) == -1);
    puts("directory native: ok");
}
