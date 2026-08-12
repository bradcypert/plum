// A thin ABI-adapter shim for POSIX directory listing — `opendir`/
// `readdir`/`closedir` return a `struct dirent *`/`DIR *`, neither of
// which fit Plum's `extern "C"` type surface (`Int`/`Float`/`Bool`/
// `CStr`/qualifying-struct only, no raw pointers/opaque types). Same
// pattern as `net_shim.c` (see its own doc comment for the general
// story) — flat `Int`/`CStr` functions, real POSIX structs built/
// consumed internally.
//
// **Handle-based, not a single call returning `Array[String]`** — the
// extern surface has no way to return an array at all. Mirrors `net_
// shim.c`'s own accept-loop shape: `dir_open` returns an opaque `Int`
// handle, `dir_read_next` returns ONE entry name per call (`""`, a
// real non-null empty `CStr`, when exhausted — same "never return
// null, an empty result is a normal outcome" convention `tcp_recv`
// already established, not a new one), and the Plum-level wrapper
// (`list_dir` in `STDLIB_OS_SRC`) loops via the SAME tail-recursive
// accumulator idiom every other "read until done" stdlib function
// already uses. `.` and `..` are skipped here, C-side, once — no
// caller of `list_dir` should ever have to filter them out themselves.
//
// **Unix-only (Linux/macOS)** — same documented v1 scope as every
// other native_stdlib shim.
#include <dirent.h>
#include <sys/stat.h>
#include <string.h>
#include <stdlib.h>
#include <stdint.h>

long long dir_open(const char *path) {
    DIR *d = opendir(path);
    if (d == NULL) {
        return -1;
    }
    return (long long)(intptr_t)d;
}

const char *dir_read_next(long long handle) {
    DIR *d = (DIR *)(intptr_t)handle;
    if (d == NULL) {
        return "";
    }
    struct dirent *entry;
    while ((entry = readdir(d)) != NULL) {
        if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0) {
            continue;
        }
        size_t len = strlen(entry->d_name);
        char *copy = malloc(len + 1);
        if (copy == NULL) {
            return "";
        }
        memcpy(copy, entry->d_name, len + 1);
        return copy;
    }
    return "";
}

void dir_close(long long handle) {
    DIR *d = (DIR *)(intptr_t)handle;
    if (d != NULL) {
        closedir(d);
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
