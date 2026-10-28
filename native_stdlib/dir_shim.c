// Directory streams own their borrowed entry storage. No entry allocation
// escapes the shim: callers copy dir_entry_data before advancing or closing
// that handle. Separate handles have independent storage (no global/TLS buffer).
#include <dirent.h>
#include <sys/stat.h>
#include <string.h>
#include <stdlib.h>
#include <stdint.h>
#include <errno.h>

typedef struct {
    DIR *stream;
    const char *name;
} PlumDir;

long long dir_open(const char *path) {
    DIR *stream = opendir(path);
    if (stream == NULL) return -1;
    PlumDir *dir = malloc(sizeof(*dir));
    if (dir == NULL) {
        closedir(stream);
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
        struct dirent *entry = readdir(dir->stream);
        if (entry == NULL) return errno == 0 ? 0 : -1;
        if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0)
            continue;
        dir->name = entry->d_name;
        return (long long)strlen(dir->name);
    }
}

const char *dir_entry_data(long long handle) {
    PlumDir *dir = (PlumDir *)(intptr_t)handle;
    return dir == NULL ? "" : dir->name;
}

// Compatibility for the checked-in bootstrap seed's String-only adapter.
// New programs use dir_next_size and validate bytes in Os.list_dir. Keeping
// this symbol lets from-seed link against current native sources without
// restoring the old per-entry allocation leak.
const char *dir_read_next(long long handle) {
    return dir_next_size(handle) > 0 ? dir_entry_data(handle) : "";
}

void dir_close(long long handle) {
    PlumDir *dir = (PlumDir *)(intptr_t)handle;
    if (dir != NULL) {
        closedir(dir->stream);
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
    struct stat st;
    if (stat(path, &st) != 0) {
        return -1;
    }
    return S_ISDIR(st.st_mode) ? 1 : 0;
}
