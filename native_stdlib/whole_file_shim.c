// Whole-file I/O for the generated runtime. Keep libc's FILE,
// size_t and positioning types on the C side of the boundary: in
// particular, Windows long/ftell are only 32 bits even on 64-bit builds.
// The caller owns the returned buffer and must free it after copying it
// into a Plum cell. NULL means failure; an empty file still owns a buffer.
#if !defined(_WIN32)
#ifndef _FILE_OFFSET_BITS
#define _FILE_OFFSET_BITS 64
#endif
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#endif

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <sys/stat.h>

#if defined(_WIN32)
#include <windows.h>
#include <fcntl.h>
#include <io.h>
#else
#include <unistd.h>
#endif

#if defined(_WIN32)
#define PLUM_WHOLE_SEEK _fseeki64
#define PLUM_WHOLE_TELL _ftelli64
#else
#define PLUM_WHOLE_SEEK fseeko
#define PLUM_WHOLE_TELL ftello
#endif

char *plum_read_whole_file(const char *path, long long *length) {
    *length = 0;
    FILE *f = fopen(path, "rb");
    if (f == NULL) return NULL;
    char *buf = NULL;

    if (PLUM_WHOLE_SEEK(f, 0, SEEK_END) != 0) goto fail;
    int64_t size = PLUM_WHOLE_TELL(f);
    // Also reserve the runtime cell's refcount, length and trailing NUL
    // (17 bytes). Neither native nor Plum allocation sizes may overflow.
    if (size < 0 || (uint64_t)size > INT64_MAX - 17 ||
        (uint64_t)size > SIZE_MAX - 17) goto fail;
    if (PLUM_WHOLE_SEEK(f, 0, SEEK_SET) != 0) goto fail;

    buf = malloc((size_t)size + 1);
    if (buf == NULL) goto fail;
    size_t got = fread(buf, 1, (size_t)size, f);
    if (ferror(f) || got != (size_t)size) goto fail;
    if (fclose(f) != 0) {
        free(buf);
        return NULL;
    }
    buf[got] = '\0';
    *length = size;
    return buf;

fail:
    fclose(f);
    free(buf);
    return NULL;
}

// Shared with Os.rename_file. Never falls back to a cross-volume copy.
extern long long os_rename_file(const char *from, const char *to);

#if defined(_WIN32)
static wchar_t *plum_whole_wide(const char *path) {
    int n = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, NULL, 0);
    if (n <= 0) return NULL;
    wchar_t *w = malloc((size_t)n * sizeof(wchar_t));
    if (w != NULL && !MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, w, n)) {
        free(w);
        return NULL;
    }
    return w;
}
#endif

// Borrow the payload; do not copy it into another file-sized buffer.
// Only the sibling name (O(path length)) and libc's bounded FILE buffer
// are allocated here. Every ordinary failure releases both before return.
long long plum_write_whole_file(const char *path, const char *data, long long length) {
    if (length < 0 || (uint64_t)length > SIZE_MAX) return -1;
    size_t n = strlen(path);
    if (n > SIZE_MAX - 64) return -1;
    char *tmp = malloc(n + 64);
    if (tmp == NULL) return -1;
    // Use a short basename even when the destination already uses NAME_MAX.
    const char *slash = strrchr(path, '/');
#if defined(_WIN32)
    const char *backslash = strrchr(path, '\\');
    if (backslash != NULL && (slash == NULL || backslash > slash)) slash = backslash;
#endif
    size_t prefix = slash == NULL ? 0 : (size_t)(slash - path) + 1;
#if defined(_WIN32)
    if (prefix == 0 && n >= 2 && path[1] == ':') prefix = 2;
#endif
    memcpy(tmp, path, prefix);
    int fd = -1;
    long long result = -1;
#if defined(_WIN32)
    wchar_t *dest = plum_whole_wide(path);
    wchar_t *wide_tmp = NULL;
    if (dest == NULL) goto done;
    DWORD attrs = GetFileAttributesW(dest);
    if (attrs == INVALID_FILE_ATTRIBUTES) {
        DWORD err = GetLastError();
        if (err != ERROR_FILE_NOT_FOUND && err != ERROR_PATH_NOT_FOUND) goto done;
    } else if (attrs & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT |
                        FILE_ATTRIBUTE_READONLY)) goto done;
    // CREATE_NEW semantics via _O_EXCL. Predictability is harmless: no
    // pre-existing file (including a reparse point) is ever opened.
    static volatile LONG sequence;
    for (int attempt = 0; attempt < 128; attempt++) {
        snprintf(tmp + prefix, n + 64 - prefix, ".plum-%lu-%lu.tmp",
                 (unsigned long)GetCurrentProcessId(),
                 (unsigned long)InterlockedIncrement(&sequence));
        free(wide_tmp);
        wide_tmp = plum_whole_wide(tmp);
        if (wide_tmp == NULL) goto done;
        fd = _wopen(wide_tmp, _O_WRONLY | _O_BINARY | _O_CREAT | _O_EXCL,
                    _S_IREAD | _S_IWRITE);
        if (fd >= 0) break;
        if (errno != EEXIST) goto done;
    }
    if (fd < 0) goto done;
    FILE *f = _fdopen(fd, "wb");
    if (f == NULL) { _close(fd); goto remove_tmp; }
#else
    struct stat st;
    int exists = lstat(path, &st) == 0;
    if (!exists && errno != ENOENT) goto done;
    // Do not turn a device, FIFO or symlink into a regular file, or
    // follow it and modify some other object. Parent paths are trusted.
    if (exists && !S_ISREG(st.st_mode)) goto done;
    memcpy(tmp + prefix, ".plum-XXXXXX", sizeof(".plum-XXXXXX"));
    fd = mkstemp(tmp);
    if (fd < 0) goto done;
    FILE *f = fdopen(fd, "wb");
    if (f == NULL) { close(fd); goto remove_tmp; }
#endif
    size_t wrote = fwrite(data, 1, (size_t)length, f);
    int ok = wrote == (size_t)length && !ferror(f);
#if !defined(_WIN32)
    // Preserve ordinary permissions for replacements, including executable
    // bits. New files retain mkstemp's private 0600 permissions. Ownership,
    // hard-link identity, ACLs and other inode metadata are not copied.
    if (ok && exists && fchmod(fd, st.st_mode & 0777) != 0) ok = 0;
#endif
    if (fclose(f) != 0) ok = 0;
    if (ok && os_rename_file(tmp, path) == 0) result = 0;
remove_tmp:
    if (result != 0) {
#if defined(_WIN32)
        _wremove(wide_tmp);
#else
        unlink(tmp);
#endif
    }
done:
#if defined(_WIN32)
    free(dest);
    free(wide_tmp);
#endif
    free(tmp);
    return result;
}
