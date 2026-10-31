// Native filesystem boundary. Plum paths are UTF-8 on Windows; POSIX paths
// remain byte strings. Returned conversion buffers belong to the caller.
#ifndef PLUM_PATH_SHIM_H
#define PLUM_PATH_SHIM_H
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <limits.h>
#include <errno.h>
#include <sys/stat.h>
#include <dirent.h>
#if defined(_WIN32)
#include <windows.h>
#include <direct.h>
#include <wchar.h>

static inline wchar_t *plum_path_wide_limit(const char *path, int max_units) {
    int n = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, NULL, 0);
    if (!n) { errno = EILSEQ; return NULL; }
    if (n > max_units) { errno = E2BIG; SetLastError(ERROR_BUFFER_OVERFLOW); return NULL; }
    if ((size_t)n > SIZE_MAX / sizeof(wchar_t)) { errno = ENOMEM; return NULL; }
    wchar_t *wide = malloc((size_t)n * sizeof(wchar_t));
    if (!wide) { errno = ENOMEM; SetLastError(ERROR_NOT_ENOUGH_MEMORY); return NULL; }
    if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, wide, n) != n) {
        free(wide); errno = EILSEQ; return NULL;
    }
    return wide;
}

static inline wchar_t *plum_path_wide(const char *path) {
    return plum_path_wide_limit(path, INT_MAX);
}

// No replacement characters and no successful truncation, including when a
// Windows filename contains an unpaired surrogate (not representable in UTF-8).
static inline int plum_path_utf8(const wchar_t *wide, char *out, size_t capacity) {
    if (!capacity || capacity > INT_MAX) { errno = ERANGE; return 0; }
    out[0] = 0;
    int n = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, wide, -1,
                                out, (int)capacity, NULL, NULL);
    if (!n) { out[0] = 0; errno = GetLastError() == ERROR_INSUFFICIENT_BUFFER ? ERANGE : EILSEQ; }
    return n;
}

static inline FILE *plum_path_fopen(const char *path, const char *mode) {
    wchar_t *wide = plum_path_wide(path);
    if (!wide) return NULL;
    // All shim callers use short ASCII CRT modes, never user-supplied text.
    wchar_t wm[16]; size_t i = 0;
    for (; mode[i] && i < 15; i++) wm[i] = (unsigned char)mode[i];
    wm[i] = 0;
    FILE *f = mode[i] ? NULL : _wfopen(wide, wm);
    free(wide);
    return f;
}

typedef struct _stat64 PlumPathStat;
static inline int plum_path_stat(const char *path, PlumPathStat *st) {
    wchar_t *wide = plum_path_wide(path);
    if (!wide) return -1;
    int rc = _wstat64(wide, st);
    free(wide); return rc;
}
static inline int plum_path_remove(const char *path) {
    wchar_t *wide = plum_path_wide(path);
    if (!wide) return -1;
    int rc = _wremove(wide);
    free(wide); return rc;
}
static inline int plum_path_mkdir(const char *path) {
    wchar_t *wide = plum_path_wide(path);
    if (!wide) return -1;
    int rc = _wmkdir(wide);
    free(wide); return rc;
}
static inline DWORD plum_path_attributes(const char *path) {
    wchar_t *wide = plum_path_wide(path);
    if (!wide) return INVALID_FILE_ATTRIBUTES;
    DWORD attrs = GetFileAttributesW(wide);
    free(wide); return attrs;
}

// MinGW's wide stream owns its entry storage. One UTF-8 buffer per stream,
// bounded by the maximum Windows filename, with no allocation per entry.
typedef struct { _WDIR *stream; char name[4 * MAX_PATH]; } PlumPathDir;
static inline PlumPathDir *plum_path_opendir(const char *path) {
    wchar_t *wide = plum_path_wide(path);
    if (!wide) return NULL;
    _WDIR *stream = _wopendir(wide);
    free(wide);
    if (!stream) return NULL;
    PlumPathDir *dir = malloc(sizeof(*dir));
    if (!dir) { _wclosedir(stream); errno = ENOMEM; return NULL; }
    dir->stream = stream; dir->name[0] = 0;
    return dir;
}
static inline const char *plum_path_readdir(PlumPathDir *dir) {
    struct _wdirent *entry = _wreaddir(dir->stream);
    if (!entry) return NULL;
    return plum_path_utf8(entry->d_name, dir->name, sizeof(dir->name)) ? dir->name : NULL;
}
static inline void plum_path_closedir(PlumPathDir *dir) {
    _wclosedir(dir->stream); free(dir);
}
#else
typedef struct stat PlumPathStat;
typedef DIR PlumPathDir;
#define plum_path_fopen fopen
#define plum_path_stat stat
#define plum_path_remove remove
#define plum_path_mkdir(path) mkdir((path), 0700)
#define plum_path_opendir opendir
#define plum_path_closedir closedir
static inline const char *plum_path_readdir(PlumPathDir *dir) {
    struct dirent *entry = readdir(dir);
    return entry ? entry->d_name : NULL;
}
#endif
#endif
