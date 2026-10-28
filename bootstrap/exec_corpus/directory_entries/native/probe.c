#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// POSIX filenames are bytes. Exercise the public API with a name that cannot
// be represented as a Plum String, without introducing an invalid String.
long long directory_bad_name(const char *root) {
    size_t n = strlen(root);
    char *path = malloc(n + 4);
    if (!path) return -1;
    memcpy(path, root, n);
    memcpy(path + n, "/\xff", 3);
    FILE *f = fopen(path, "wb");
    free(path);
    if (!f) return -1;
    return fclose(f);
}
