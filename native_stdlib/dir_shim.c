// Directory streams own their borrowed entry storage. No entry allocation
// escapes the shim: callers copy dir_entry_data before advancing or closing
// that handle. Separate handles have independent storage (no global/TLS buffer).
#include <dirent.h>
#include <sys/stat.h>
#include <string.h>
#include <stdlib.h>
#include <stdint.h>
#include <errno.h>
#include "path_shim.h"

typedef struct {
    PlumPathDir *stream;
    const char *name;
} PlumDir;

long long dir_open(const char *path) {
    PlumPathDir *stream = plum_path_opendir(path);
    if (stream == NULL) return -1;
    PlumDir *dir = malloc(sizeof(*dir));
    if (dir == NULL) {
        plum_path_closedir(stream);
        return -1;
    }
    dir->stream = stream;
    dir->name = "";
    return (long long)(intptr_t)dir;
}

// Positive byte length = entry; zero = exhaustion; -1 = read failure.
// Clear errno immediately before EACH read: skipped entries and callers
// can leave unrelated errors behind. Never turn a failed read into EOF.
long long dir_next_size(long long handle) {
    PlumDir *dir = (PlumDir *)(intptr_t)handle;
    if (dir == NULL) return -1;
    dir->name = "";
    for (;;) {
        errno = 0;
        const char *entry = plum_path_readdir(dir->stream);
        if (entry == NULL) return errno == 0 ? 0 : -1;
        if (strcmp(entry, ".") == 0 || strcmp(entry, "..") == 0)
            continue;
        dir->name = entry;
        return (long long)strlen(dir->name);
    }
}

const char *dir_entry_data(long long handle) {
    PlumDir *dir = (PlumDir *)(intptr_t)handle;
    return dir == NULL ? "" : dir->name;
}

void dir_close(long long handle) {
    PlumDir *dir = (PlumDir *)(intptr_t)handle;
    if (dir != NULL) {
        plum_path_closedir(dir->stream);
        free(dir);
    }
}

// `stat(2)`-based — tells a caller (module resolution, walking a
// project tree) whether to recurse into an entry `list_dir` returned.
// Returns 1 (is a directory), 0 (exists, isn't a directory), or -1
// (`stat` itself failed — most commonly: the path doesn't exist at
// all). The three-way return (not a plain `Bool`) is what lets the
// Plum-level `is_directory` wrapper distinguish "doesn't exist" from
// "exists but isn't a directory" as a real `Err`, not silently
// collapse both into `false`.
long long path_is_dir(const char *path) {
    PlumPathStat st;
    if (plum_path_stat(path, &st) != 0) {
        return -1;
    }
    return S_ISDIR(st.st_mode) ? 1 : 0;
}
