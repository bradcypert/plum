// Filesystem and self-location primitives, so the compiler stops
// shelling out to Unix commands to do them.
//
// `bootstrap/self_host/main.plum` used to reach these through
// `run_process`: `mktemp -d` (5 sites), `rm -rf`/`rm -f` (6), `cp -r`
// (1), `mkdir` (1), and `/proc/self/exe` (3). That works on Linux,
// works on macOS by luck, and cannot work on Windows, where none of
// those programs exist. `/proc/self/exe` is worse than the others: it
// is Linux-only, so the language server -- which re-invokes itself
// through it -- was already broken on a Mac.
//
// Replacing them is also just better on the platforms where they DID
// work. A `plum build` forked three processes to make a directory,
// delete a file, and delete a directory; now it forks one, for `clang`,
// which is the only one that was ever doing real work.
//
// Same shim conventions as its neighbours (see `net_shim.c`'s header
// for the general pattern): every function takes and returns only
// `long long` (Plum `Int`) or `const char *` (Plum `CStr`), because
// Plum's extern surface has no raw pointers, no out-parameters and no
// multi-value return.
//
// **Returned strings live in a static buffer**, not in `malloc`'d
// memory. The Plum side copies immediately into a string cell
// (`plum_str_new`), so the buffer's contents are needed only until the
// call returns -- and a `malloc` here would be a leak, since the
// extern boundary gives the Plum side no way to free it. Not
// re-entrant, and not required to be: nothing calls these from more
// than one thread.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <dirent.h>

#if defined(_WIN32)
#include <windows.h>
#include <direct.h>
#include <io.h>
#define PLUM_MKDIR(p) _mkdir(p)
#define PLUM_RMDIR(p) _rmdir(p)
#else
#include <unistd.h>
#define PLUM_MKDIR(p) mkdir((p), 0700)
#define PLUM_RMDIR(p) rmdir(p)
#endif

#if defined(__APPLE__)
#include <mach-o/dyld.h>
#endif

#define PLUM_PATH_MAX 4096

static char plum_os_buf[PLUM_PATH_MAX];

// Creates a fresh, empty, private directory and returns its path.
// Returns "" -- an empty, non-null CStr -- on failure, because a null
// CStr return is a hard runtime abort under Plum's FFI semantics (see
// `net_shim.c`'s `tcp_recv` note, the same trade for the same reason).
const char *os_temp_dir(void) {
#if defined(_WIN32)
    char base[PLUM_PATH_MAX];
    DWORD n = GetTempPathA((DWORD)sizeof(base), base);
    if (n == 0 || n >= sizeof(base)) { plum_os_buf[0] = '\0'; return plum_os_buf; }
    // `GetTempFileNameA` creates a FILE; the directory of the same name
    // is what is wanted, so the file is removed and a directory put in
    // its place. The name is still unique -- that is what the call
    // bought -- and the window between the two is not a security
    // boundary this compiler relies on.
    if (GetTempFileNameA(base, "plum", 0, plum_os_buf) == 0) { plum_os_buf[0] = '\0'; return plum_os_buf; }
    DeleteFileA(plum_os_buf);
    if (PLUM_MKDIR(plum_os_buf) != 0) { plum_os_buf[0] = '\0'; }
    return plum_os_buf;
#else
    const char *tmp = getenv("TMPDIR");
    if (tmp == NULL || tmp[0] == '\0') tmp = "/tmp";
    if (snprintf(plum_os_buf, sizeof(plum_os_buf), "%s/plum-XXXXXX", tmp) >= (int)sizeof(plum_os_buf)) {
        plum_os_buf[0] = '\0';
        return plum_os_buf;
    }
    if (mkdtemp(plum_os_buf) == NULL) plum_os_buf[0] = '\0';
    return plum_os_buf;
#endif
}

// The path of the running executable. Three genuinely different calls
// for one question; there is no portable spelling.
const char *os_self_exe(void) {
    plum_os_buf[0] = '\0';
#if defined(_WIN32)
    DWORD n = GetModuleFileNameA(NULL, plum_os_buf, (DWORD)sizeof(plum_os_buf));
    if (n == 0 || n >= sizeof(plum_os_buf)) plum_os_buf[0] = '\0';
#elif defined(__APPLE__)
    uint32_t n = (uint32_t)sizeof(plum_os_buf);
    if (_NSGetExecutablePath(plum_os_buf, &n) != 0) plum_os_buf[0] = '\0';
#else
    ssize_t n = readlink("/proc/self/exe", plum_os_buf, sizeof(plum_os_buf) - 1);
    if (n < 0) { plum_os_buf[0] = '\0'; } else { plum_os_buf[n] = '\0'; }
#endif
    return plum_os_buf;
}

// 0 on success, -1 on failure, matching the other shims' Int-sentinel
// convention rather than C's.
long long os_make_dir(const char *path) {
    return PLUM_MKDIR(path) == 0 ? 0 : -1;
}

long long os_remove_file(const char *path) {
    return remove(path) == 0 ? 0 : -1;
}

// `rename`, which is ATOMIC within a filesystem: the destination is
// either the old file or the new one, never a half-written mixture.
// That is the whole reason this exists -- `plum fmt --write` builds the
// formatted text beside the original and renames it into place, so an
// interrupted run cannot leave somebody's source truncated.
//
// Across filesystems `rename` fails rather than copying, and the
// caller is told so rather than being silently given a slower,
// non-atomic fallback it did not ask for.
long long os_rename_file(const char *from, const char *to) {
    return rename(from, to) == 0 ? 0 : -1;
}

static int plum_is_dir(const char *path) {
    struct stat st;
    if (stat(path, &st) != 0) return 0;
    return S_ISDIR(st.st_mode) ? 1 : 0;
}

static int plum_join(char *out, size_t cap, const char *a, const char *b) {
    int n = snprintf(out, cap, "%s/%s", a, b);
    return (n > 0 && (size_t)n < cap) ? 0 : -1;
}

// Deletes a directory and everything under it. Recurses with a
// heap-allocated path buffer per level rather than a shared one,
// because the caller's path must stay intact across the child call.
//
// Symlinks are removed, never followed: `plum_is_dir` uses `stat`,
// which follows, so a symlink to a directory would recurse into the
// TARGET and delete someone else's files. `lstat` is used here for
// exactly that reason. On Windows there is no `lstat`; the reparse
// point is deleted by `remove` as an ordinary entry.
static int plum_remove_tree(const char *path) {
    DIR *d;
    struct dirent *e;
    char *child;
    int rc = 0;

#if !defined(_WIN32)
    struct stat st;
    if (lstat(path, &st) != 0) return -1;
    if (!S_ISDIR(st.st_mode)) return remove(path) == 0 ? 0 : -1;
#else
    if (!plum_is_dir(path)) return remove(path) == 0 ? 0 : -1;
#endif

    d = opendir(path);
    if (d == NULL) return -1;
    child = (char *)malloc(PLUM_PATH_MAX);
    if (child == NULL) { closedir(d); return -1; }

    while ((e = readdir(d)) != NULL) {
        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) continue;
        if (plum_join(child, PLUM_PATH_MAX, path, e->d_name) != 0) { rc = -1; continue; }
        if (plum_remove_tree(child) != 0) rc = -1;
    }
    free(child);
    closedir(d);
    if (PLUM_RMDIR(path) != 0) rc = -1;
    return rc;
}

long long os_remove_tree(const char *path) {
    return plum_remove_tree(path) == 0 ? 0 : -1;
}

static int plum_copy_file(const char *src, const char *dst) {
    FILE *in, *out;
    char buf[65536];
    size_t n;
    int rc = 0;

    in = fopen(src, "rb");
    if (in == NULL) return -1;
    out = fopen(dst, "wb");
    if (out == NULL) { fclose(in); return -1; }
    while ((n = fread(buf, 1, sizeof(buf), in)) > 0) {
        if (fwrite(buf, 1, n, out) != n) { rc = -1; break; }
    }
    if (ferror(in)) rc = -1;
    if (fclose(out) != 0) rc = -1;
    fclose(in);
    return rc;
}

// Copies the CONTENTS of `src` into `dst`, which must already exist --
// the semantics of `cp -r src/. dst`, which is what this replaced, and
// deliberately not `cp -r src dst` (that would nest a directory).
//
// File MODES are not preserved. The one caller copies a project into a
// scratch directory to type-check it, where nothing is executed; if a
// caller ever needs the executable bit, this is the line to revisit
// rather than a thing to assume.
static int plum_copy_tree(const char *src, const char *dst) {
    DIR *d;
    struct dirent *e;
    char *s;
    char *t;
    int rc = 0;

    d = opendir(src);
    if (d == NULL) return -1;
    s = (char *)malloc(PLUM_PATH_MAX);
    t = (char *)malloc(PLUM_PATH_MAX);
    if (s == NULL || t == NULL) { free(s); free(t); closedir(d); return -1; }

    while ((e = readdir(d)) != NULL) {
        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) continue;
        if (plum_join(s, PLUM_PATH_MAX, src, e->d_name) != 0) { rc = -1; continue; }
        if (plum_join(t, PLUM_PATH_MAX, dst, e->d_name) != 0) { rc = -1; continue; }
        if (plum_is_dir(s)) {
            if (PLUM_MKDIR(t) != 0 && !plum_is_dir(t)) { rc = -1; continue; }
            if (plum_copy_tree(s, t) != 0) rc = -1;
        } else {
            if (plum_copy_file(s, t) != 0) rc = -1;
        }
    }
    free(s);
    free(t);
    closedir(d);
    return rc;
}

long long os_copy_tree(const char *src, const char *dst) {
    return plum_copy_tree(src, dst) == 0 ? 0 : -1;
}
