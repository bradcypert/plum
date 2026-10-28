// Whole-file I/O for the generated runtime. Keep libc's FILE,
// size_t and positioning types on the C side of the boundary: in
// particular, Windows long/ftell are only 32 bits even on 64-bit builds.
// The caller owns the returned buffer and must free it after copying it
// into a Plum cell. NULL means failure; an empty file still owns a buffer.
#if defined(__APPLE__)
#define _DARWIN_C_SOURCE 1
#endif
#if !defined(_WIN32)
#ifndef _FILE_OFFSET_BITS
#define _FILE_OFFSET_BITS 64
#endif
#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
#endif

#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <sys/stat.h>

#if defined(_WIN32)
#include <windows.h>
#include <winternl.h>
#include <fcntl.h>
#include <io.h>
#else
#include <unistd.h>
#include <fcntl.h>
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

// Complete the stream before publication. Both ordinary whole-file writes
// and strict generated-output writes use this transaction boundary.
static int plum_whole_finish(FILE *f, const char *data, size_t length,
                             int preserve_mode, unsigned mode) {
    size_t wrote = fwrite(data, 1, length, f);
    int ok = wrote == length && !ferror(f);
#if !defined(_WIN32)
    if (ok && preserve_mode && fchmod(fileno(f), mode & 0777) != 0) ok = 0;
#else
    (void)preserve_mode;
    (void)mode;
#endif
    if (fclose(f) != 0) ok = 0;
    return ok;
}

#if defined(_WIN32)
// Both MoveFileEx and an NT rename with RootDirectory reopen the destination
// directory for write access, conflicting with the ancestor locks. The NT
// same-directory form (bare filename, NULL RootDirectory) instead renames
// within the source handle's directory, without resolving another parent.
static int plum_whole_rename_in_place(const wchar_t *from, const wchar_t *to) {
    const wchar_t *leaf = wcsrchr(to, L'\\');
    if (!leaf || !*++leaf) return -1;
    size_t bytes = wcslen(leaf) * sizeof(wchar_t);
    size_t size = sizeof(FILE_RENAME_INFORMATION) + bytes;
    if (size > MAXDWORD) return -1;
    FILE_RENAME_INFORMATION *rename_info = malloc(size);
    if (!rename_info) return -1;
    memset(rename_info, 0, size);
    rename_info->ReplaceIfExists = TRUE;
    rename_info->RootDirectory = NULL;
    rename_info->FileNameLength = (DWORD)bytes;
    memcpy(rename_info->FileName, leaf, bytes);
    typedef NTSTATUS (NTAPI *Rename)(HANDLE, PIO_STATUS_BLOCK, PVOID, ULONG, FILE_INFORMATION_CLASS);
    HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
    Rename rename_file = ntdll ? (Rename)(void *)GetProcAddress(ntdll, "NtSetInformationFile") : NULL;
    if (!rename_file) { free(rename_info); return -1; }
    HANDLE file = CreateFileW(from, DELETE | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL, OPEN_EXISTING,
        FILE_FLAG_OPEN_REPARSE_POINT, NULL);
    int ok = 0;
    if (file != INVALID_HANDLE_VALUE) {
        BY_HANDLE_FILE_INFORMATION info;
        IO_STATUS_BLOCK status;
        ok = GetFileInformationByHandle(file, &info) &&
            !(info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)) &&
            rename_file(file, &status, rename_info, (ULONG)size, FileRenameInformation) >= 0;
        CloseHandle(file);
    }
    free(rename_info);
    return ok ? 0 : -1;
}
#endif

// Borrow the payload; do not copy it into another file-sized buffer.
// Only the sibling name (O(path length)) and libc's bounded FILE buffer
// are allocated here. Every ordinary failure releases both before return.
static long long plum_write_whole_file_impl(const char *path, const char *data, long long length,
                                            int strict) {
#if !defined(_WIN32)
    (void)strict;
#endif
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
    struct stat st = {0};
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
#if defined(_WIN32)
    int ok = plum_whole_finish(f, data, (size_t)length, 0, 0);
#else
    int ok = plum_whole_finish(f, data, (size_t)length, exists, st.st_mode);
#endif
    if (ok) {
#if defined(_WIN32)
        int published = strict ? plum_whole_rename_in_place(wide_tmp, dest) : os_rename_file(tmp, path);
#else
        int published = os_rename_file(tmp, path);
#endif
        if (published == 0) result = 0;
    }
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

long long plum_write_whole_file(const char *path, const char *data, long long length) {
    return plum_write_whole_file_impl(path, data, length, 0);
}

// Generated output has a stricter trust boundary than Os.write_file:
// no ancestor may redirect writes. Walk iteratively, holding native directory
// identities rather than checking names and later resolving them again.
// Only path-sized scratch is allocated; the rendered page remains borrowed.
#if !defined(_WIN32)
static int plum_generated_parent(char *path, char **leaf) {
    int dir = open(path[0] == '/' ? "/" : ".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (dir < 0) return -1;
    char *part = path;
    for (char *p = path; *p; p++) {
        if (*p != '/') continue;
        *p = 0;
        if (*part && strcmp(part, ".") != 0) {
            if (mkdirat(dir, part, 0700) != 0 && errno != EEXIST) goto fail;
            int next = openat(dir, part, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
            if (next < 0) goto fail;
            close(dir);
            dir = next;
        }
        part = p + 1;
    }
    if (!*part || strcmp(part, ".") == 0 || strcmp(part, "..") == 0) goto fail;
    *leaf = part;
    return dir;
fail:
    close(dir);
    return -1;
}
#else
// Windows path operations are anchored by keeping every ancestor open without
// FILE_SHARE_DELETE or FILE_SHARE_WRITE. A checked directory cannot be
// renamed or rewritten into a junction during publication. Reject reparse points.
// Bound live handles (and accept an explicit error for excessive depth).
#define PLUM_GENERATED_DEPTH 128
static wchar_t *plum_generated_lock(const char *path, HANDLE *dirs, size_t *count) {
    wchar_t *input = plum_whole_wide(path);
    if (!input) return NULL;
    DWORD n = GetFullPathNameW(input, 0, NULL, NULL);
    wchar_t *full = n ? malloc((size_t)n * sizeof(wchar_t)) : NULL;
    DWORD got = full ? GetFullPathNameW(input, n, full, NULL) : 0;
    if (!got || got >= n) {
        free(input); free(full); return NULL;
    }
    free(input);
    // A drive root or a UNC share is the traversal anchor. Device namespace
    // paths are deliberately outside this generated-output API.
    size_t start = 0;
    if (full[0] && full[1] == L':' && full[2] == L'\\') start = 3;
    else if (full[0] == L'\\' && full[1] == L'\\' && full[2] != L'?' && full[2] != L'.') {
        wchar_t *server = wcschr(full + 2, L'\\');
        wchar_t *share = server ? wcschr(server + 1, L'\\') : NULL;
        if (share) start = (size_t)(share - full) + 1;
    }
    if (!start) { free(full); return NULL; }
    for (size_t i = start - 1; full[i]; i++) {
        if (full[i] != L'\\') continue;
        if (*count == PLUM_GENERATED_DEPTH) goto fail;
        // Keep the separator on the root (C:\ or \\server\share\).
        size_t end = i == start - 1 ? i + 1 : i;
        wchar_t tail = full[end]; full[end] = 0;
        if (i != start - 1 && !CreateDirectoryW(full, NULL) && GetLastError() != ERROR_ALREADY_EXISTS) {
            full[end] = tail; goto fail;
        }
        // An attributes-only open does not participate in Windows sharing
        // checks. Directory-list access makes denial of write/delete sharing
        // effective, so the checked ancestor really stays pinned.
        HANDLE h = CreateFileW(full, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
                               NULL, OPEN_EXISTING,
                               FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, NULL);
        full[end] = tail;
        if (h == INVALID_HANDLE_VALUE) goto fail;
        BY_HANDLE_FILE_INFORMATION info;
        if (!GetFileInformationByHandle(h, &info) ||
            !(info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) ||
            (info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)) {
            CloseHandle(h); goto fail;
        }
        dirs[(*count)++] = h;
    }
    return full;
fail:
    free(full);
    return NULL;
}
#endif

long long plum_write_generated_file(const char *path, const char *data, long long length) {
    if (length < 0 || (uint64_t)length > SIZE_MAX || !*path) return -1;
#if defined(_WIN32)
    HANDLE dirs[PLUM_GENERATED_DEPTH];
    size_t count = 0;
    wchar_t *full = plum_generated_lock(path, dirs, &count);
    long long result = -1;
    if (full) {
        int n = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, full, -1, NULL, 0, NULL, NULL);
        char *absolute = n > 0 ? malloc((size_t)n) : NULL;
        if (absolute && WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, full, -1, absolute, n, NULL, NULL))
            result = plum_write_whole_file_impl(absolute, data, length, 1);
        free(absolute);
        free(full);
    }
    while (count) CloseHandle(dirs[--count]);
    return result;
#else
    char *copy = strdup(path);
    if (!copy) return -1;
    char *leaf = NULL;
    int dir = plum_generated_parent(copy, &leaf);
    long long result = -1;
    if (dir < 0) { free(copy); return -1; }
    struct stat st = {0};
    int exists = fstatat(dir, leaf, &st, AT_SYMLINK_NOFOLLOW) == 0;
    if ((!exists && errno != ENOENT) || (exists && !S_ISREG(st.st_mode))) goto done;
    char tmp[64];
    int fd = -1;
    static _Atomic unsigned sequence;
    for (int attempt = 0; attempt < 128; attempt++) {
        unsigned serial = ++sequence;
        snprintf(tmp, sizeof(tmp), ".plum-%lu-%u.tmp", (unsigned long)getpid(), serial);
        fd = openat(dir, tmp, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0600);
        if (fd >= 0) break;
        if (errno != EEXIST) goto done;
    }
    if (fd < 0) goto done;
    FILE *f = fdopen(fd, "wb");
    if (f == NULL) close(fd);
    else if (plum_whole_finish(f, data, (size_t)length, exists, st.st_mode)) {
        // Recheck type at publication, without following a concurrent link.
        // renameat never dereferences the leaf, even if it changes afterwards.
        int present = fstatat(dir, leaf, &st, AT_SYMLINK_NOFOLLOW) == 0;
        if (((!present && errno == ENOENT) || (present && S_ISREG(st.st_mode))) &&
            renameat(dir, tmp, dir, leaf) == 0) result = 0;
    }
    if (result != 0) unlinkat(dir, tmp, 0);
done:
    close(dir);
    free(copy);
    return result;
#endif
}
