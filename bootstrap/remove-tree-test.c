// Exercise the real shim, with deterministic races and resource failures.
// All destructive fixtures live below an independently created scratch root.
#include <assert.h>
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <dirent.h>
#if defined(_WIN32)
#include <windows.h>
#include <winternl.h>
#else
#include <unistd.h>
#include <fcntl.h>
#include <sys/resource.h>
#endif

static size_t live_bytes, peak_bytes, allocations;
static int fail_alloc;
typedef union { size_t size; max_align_t align; } Allocation;
static void *tracked_malloc(size_t n) {
    if (fail_alloc && --fail_alloc == 0) return NULL;
    Allocation *a = malloc(sizeof(*a) + n);
    assert(a != NULL);
    a->size = n;
    live_bytes += n;
    allocations++;
    if (live_bytes > peak_bytes) peak_bytes = live_bytes;
    return a + 1;
}
static void tracked_free(void *p) {
    if (!p) return;
    Allocation *a = (Allocation *)p - 1;
    live_bytes -= a->size;
    allocations--;
    free(a);
}

static int handles, peak_handles;
static void acquired(void) { if (++handles > peak_handles) peak_handles = handles; }
#if !defined(_WIN32)
static int fail_open, fail_stream, fail_read, race;
static char outside[4096];
static int tracked_open(const char *p, int flags) {
    int fd = open(p, flags);
    if (fd >= 0) acquired();
    return fd;
}
static int tracked_openat(int parent, const char *name, int flags) {
    if (fail_open) { fail_open = 0; errno = EMFILE; return -1; }
    if ((race == 1 || race == 4) && strcmp(name, "victim") == 0) {
        int replacement_directory = race == 4;
        race = 0;
        assert(renameat(parent, name, parent, "parked") == 0);
        if (replacement_directory) assert(mkdirat(parent, name, 0700) == 0);
        else assert(symlinkat(outside, parent, name) == 0);
    }
    int fd = openat(parent, name, flags);
    if (fd >= 0) acquired();
    return fd;
}
static int tracked_close(int fd) { assert(handles > 0); handles--; return close(fd); }
static DIR *tracked_fdopendir(int fd) {
    if (fail_stream) { fail_stream = 0; errno = ENOMEM; return NULL; }
    DIR *dir = fdopendir(fd);
    if (dir && race == 2) {
        race = 0;
        // Replace a child after its parent is opened but before enumeration.
        assert(renameat(fd, "victim", fd, "parked") == 0);
        assert(symlinkat(outside, fd, "victim") == 0);
    }
    if (dir && race == 3) {
        race = 0;
        // Rename the already opened root and put a link at its old name.
        assert(rename("tree", "parked-root") == 0);
        assert(symlink(outside, "tree") == 0);
    }
    return dir;
}
static int tracked_closedir(DIR *d) { assert(handles > 0); handles--; return closedir(d); }
static struct dirent *tracked_readdir(DIR *d) {
    if (fail_read) { fail_read = 0; errno = EIO; return NULL; }
    return readdir(d);
}
#define open tracked_open
#define openat tracked_openat
#define close tracked_close
#define fdopendir tracked_fdopendir
#define closedir tracked_closedir
#define readdir tracked_readdir
#else
static int fail_read, fail_delete, fail_child, deny_swap;
static void junction(const char *path);
static BOOL WINAPI tracked_info(HANDLE h, FILE_INFO_BY_HANDLE_CLASS kind, LPVOID data, DWORD n) {
    if (deny_swap && kind == FileAttributeTagInfo) {
        deny_swap = 0;
        assert(!MoveFileW(L"tree", L"parked"));
        assert(GetLastError() == ERROR_SHARING_VIOLATION);
        HANDLE writer = CreateFileW(L"tree", GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            NULL, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, NULL);
        assert(writer == INVALID_HANDLE_VALUE && GetLastError() == ERROR_SHARING_VIOLATION);
    }
    if (fail_read && kind == FileIdBothDirectoryInfo) {
        fail_read = 0; SetLastError(ERROR_READ_FAULT); return FALSE;
    }
    return GetFileInformationByHandleEx(h, kind, data, n);
}
static BOOL WINAPI tracked_disposition(HANDLE h, FILE_INFO_BY_HANDLE_CLASS kind, LPVOID data, DWORD n) {
    if (fail_delete) { fail_delete = 0; SetLastError(ERROR_ACCESS_DENIED); return FALSE; }
    return SetFileInformationByHandle(h, kind, data, n);
}
static HANDLE WINAPI tracked_create(LPCWSTR path, DWORD access, DWORD share, LPSECURITY_ATTRIBUTES sa,
                                    DWORD creation, DWORD flags, HANDLE template) {
    HANDLE h = CreateFileW(path, access, share, sa, creation, flags, template);
    if (h != INVALID_HANDLE_VALUE) acquired();
    return h;
}
static BOOL WINAPI tracked_close_handle(HANDLE h) { assert(handles > 0); handles--; return CloseHandle(h); }
static NTSTATUS NTAPI tracked_nt_open(PHANDLE out, ACCESS_MASK access, POBJECT_ATTRIBUTES attrs,
    PIO_STATUS_BLOCK io, PLARGE_INTEGER size, ULONG attributes, ULONG share, ULONG disposition,
    ULONG options, PVOID ea, ULONG ea_size) {
    typedef NTSTATUS (NTAPI *Open)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PIO_STATUS_BLOCK,
        PLARGE_INTEGER, ULONG, ULONG, ULONG, ULONG, PVOID, ULONG);
    Open fn = (Open)(void *)GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtCreateFile");
    if (fail_child) { fail_child = 0; return (NTSTATUS)0xc0000022; }
    NTSTATUS result = fn(out, access, attrs, io, size, attributes, share, disposition, options, ea, ea_size);
    if (result >= 0) acquired();
    return result;
}
static FARPROC WINAPI tracked_proc(HMODULE mod, LPCSTR name) {
    if (strcmp(name, "NtCreateFile") == 0) return (FARPROC)(void *)tracked_nt_open;
    return GetProcAddress(mod, name);
}
#define GetFileInformationByHandleEx tracked_info
#define SetFileInformationByHandle tracked_disposition
#define CreateFileW tracked_create
#define CloseHandle tracked_close_handle
#define GetProcAddress tracked_proc
#endif
#define malloc tracked_malloc
#define free tracked_free
#include "../native_stdlib/os_shim.c"
#undef malloc
#undef free
#if !defined(_WIN32)
#undef open
#undef openat
#undef close
#undef fdopendir
#undef closedir
#undef readdir
#else
#undef GetFileInformationByHandleEx
#undef SetFileInformationByHandle
#undef CreateFileW
#undef CloseHandle
#undef GetProcAddress
#endif

static void clean(void) { assert(live_bytes == 0 && allocations == 0 && handles == 0); }
static void put(const char *path) {
    FILE *f = fopen(path, "wb");
    assert(f != NULL && fputs("must survive", f) >= 0 && fclose(f) == 0);
}
static int exists(const char *path) { struct stat st; return stat(path, &st) == 0; }
static void directory(const char *path) { assert(PLUM_MKDIR(path) == 0); }
static void removed(const char *path) { assert(os_remove_tree(path) == 0); clean(); }

static void basics(void) {
    put("file"); removed("file"); assert(!exists("file"));
    directory("tree"); directory("tree/child"); put("tree/child/file");
    removed("tree///"); assert(!exists("tree"));
    assert(os_remove_tree("missing") == -1); clean();
    assert(os_remove_tree("") == -1); clean();
    assert(os_remove_tree(".") == -1); clean();
    assert(os_remove_tree("..") == -1); clean();
#if defined(_WIN32)
    WCHAR cwd[4096]; assert(GetCurrentDirectoryW(4096, cwd));
    if (cwd[1] == L':') {
        char dot[] = {(char)cwd[0], ':', '.', 0};
        char dotdot[] = {(char)cwd[0], ':', '.', '.', 0};
        assert(os_remove_tree(dot) == -1); clean();
        assert(os_remove_tree(dotdot) == -1); clean();
    }
#endif
    directory("tree"); put("tree/keep");
    assert(os_remove_tree("tree/.") == -1); assert(exists("tree/keep")); clean();
    fail_alloc = 1; assert(os_remove_tree("tree") == -1); clean();
    assert(exists("tree/keep"));
    fail_read = 1; assert(os_remove_tree("tree") == -1); clean();
    assert(exists("tree/keep"));
#if !defined(_WIN32)
    fail_open = 1; assert(os_remove_tree("tree") == -1); clean();
    fail_stream = 1; assert(os_remove_tree("tree") == -1); clean();
#else
    fail_alloc = 2; assert(os_remove_tree("tree") == -1); clean();
    fail_child = 1; assert(os_remove_tree("tree") == -1); clean();
    fail_delete = 1; assert(os_remove_tree("tree") == -1); clean();
#endif
    removed("tree");

    // Width must not accumulate buffers or handles. Repeat BEFORE process exit.
    size_t narrow_peak = 0;
    for (int run = 0; run < 3; run++) {
        directory("tree");
        int width = run == 0 ? 1 : 1000;
        for (int i = 0; i < width; i++) { char path[64]; snprintf(path, sizeof(path), "tree/f%d", i); put(path); }
        peak_bytes = 0; peak_handles = 0;
        removed("tree");
        if (run == 0) narrow_peak = peak_bytes;
        assert(peak_bytes == narrow_peak);
        assert(peak_handles <= 3);
    }
    printf("width 1/1000: peak shim scratch %zu bytes; no retained scratch or handles\n", narrow_peak);
}

#if !defined(_WIN32)
static void links_and_races(void) {
    assert(getcwd(outside, sizeof(outside)) != NULL);
    size_t n = strlen(outside);
    assert(n + 9 < sizeof(outside)); strcpy(outside + n, "/outside");
    directory("outside"); put("outside/keep");
    const char *paths[] = {"link", "link/", "link///"};
    for (unsigned i = 0; i < 3; i++) {
        assert(symlink(outside, "link") == 0);
        removed(paths[i]);
        struct stat st; assert(lstat("link", &st) == -1 && errno == ENOENT);
        assert(exists("outside/keep"));
    }
    assert(symlink("absent", "dangling") == 0); removed("dangling/");
    directory("tree"); assert(symlink(outside, "tree/link") == 0);
    assert(symlink("..", "tree/cycle") == 0); removed("tree"); assert(exists("outside/keep"));
    for (int mode = 1; mode <= 4; mode++) {
        directory("tree"); directory("tree/victim"); put("tree/victim/file");
        race = mode;
        long long rc = os_remove_tree("tree/");
        assert(race == 0 && (rc == 0 || rc == -1)); clean();
        assert(exists("outside/keep"));
        if (mode == 1 || mode == 4) assert(rc == -1 && exists("tree/parked/file"));
        if (mode == 3) { assert(rc == -1); removed("parked-root"); }
        if (exists("tree")) removed("tree");
    }
    // Descriptor exhaustion must unwind, with no pathname fallback.
    directory("tree"); directory("tree/a"); directory("tree/a/b"); put("tree/a/b/file");
    struct rlimit original, limited;
    assert(getrlimit(RLIMIT_NOFILE, &original) == 0);
    limited = original; limited.rlim_cur = 6;
    assert(setrlimit(RLIMIT_NOFILE, &limited) == 0);
    assert(os_remove_tree("tree") == -1); clean();
    assert(setrlimit(RLIMIT_NOFILE, &original) == 0);
    assert(exists("tree/a/b/file")); removed("tree");

    // Paths below the acquired root need not fit PLUM_PATH_MAX. Also pin
    // the explicit depth bound and verify cleanup on reaching that bound.
    directory("tree");
    int fd = open("tree", O_RDONLY | O_DIRECTORY); assert(fd >= 0);
    char component[101]; memset(component, 'd', 100); component[100] = 0;
    for (int i = 0; i < PLUM_REMOVE_DEPTH; i++) {
        assert(mkdirat(fd, component, 0700) == 0);
        int child = openat(fd, component, O_RDONLY | O_DIRECTORY); assert(child >= 0);
        close(fd); fd = child;
    }
    close(fd);
    assert(os_remove_tree("tree") == -1); clean();
    assert(peak_handles <= PLUM_REMOVE_DEPTH + 1);
    // Pop the final directory so the remaining depth fits, using the test's
    // own descriptor-relative walk rather than a shell cleanup helper.
    fd = open("tree", O_RDONLY | O_DIRECTORY); assert(fd >= 0);
    for (int i = 0; i < PLUM_REMOVE_DEPTH - 1; i++) {
        int child = openat(fd, component, O_RDONLY | O_DIRECTORY); assert(child >= 0);
        close(fd); fd = child;
    }
    assert(unlinkat(fd, component, AT_REMOVEDIR) == 0); close(fd);
    removed("tree"); removed("outside");
}
#else
static void junction(const char *path) {
    directory(path);
    WCHAR absolute[4096]; assert(GetFullPathNameW(L"outside", 4096, absolute, NULL));
    WCHAR substitute[4104]; assert(swprintf(substitute, 4104, L"\\??\\%ls", absolute) > 0);
    struct Junction {
        DWORD tag; WORD length, reserved;
        WORD substitute_offset, substitute_length, print_offset, print_length;
        WCHAR path[8192];
    } data = {0};
    data.tag = IO_REPARSE_TAG_MOUNT_POINT;
    size_t chars = wcslen(substitute);
    data.substitute_length = (WORD)(chars * sizeof(WCHAR));
    data.print_offset = (WORD)((chars + 1) * sizeof(WCHAR));
    memcpy(data.path, substitute, (chars + 1) * sizeof(WCHAR));
    data.length = (WORD)(8 + (chars + 2) * sizeof(WCHAR));
    WCHAR wide[4096];
    assert(MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, wide, 4096));
    HANDLE h = CreateFileW(wide, GENERIC_WRITE, 0, NULL, OPEN_EXISTING,
                           FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS, NULL);
    assert(h != INVALID_HANDLE_VALUE);
    DWORD bytes;
    assert(DeviceIoControl(h, FSCTL_SET_REPARSE_POINT, &data, data.length + 8, NULL, 0, &bytes, NULL));
    assert(CloseHandle(h));
}
static void links_and_races(void) {
    directory("outside"); put("outside/keep");
    directory("MiXeD"); removed("mixed");
    assert(CreateDirectoryW(L"tree-\x65e5", NULL));
    FILE *unicode = _wfopen(L"tree-\x65e5/file", L"wb");
    assert(unicode != NULL && fclose(unicode) == 0);
    removed("tree-\xe6\x97\xa5/");
    // Junctions do not require Developer Mode or symlink privileges.
    const char *paths[] = {"junction", "junction/", "junction///", "junction\\"};
    for (unsigned i = 0; i < sizeof(paths) / sizeof(paths[0]); i++) {
        junction("junction"); removed(paths[i]); assert(exists("outside/keep"));
    }
    directory("tree"); junction("tree/junction");
    removed("tree"); assert(exists("outside/keep"));
    directory("tree"); put("tree/file");
    deny_swap = 1; removed("tree"); assert(deny_swap == 0);
    // Fail every allocation in a nested walk and check cleanup before the
    // next attempt, rather than relying on process-exit leak detection.
    directory("tree");
    char nested[128] = "tree";
    for (int i = 0; i < 8; i++) { strcat(nested, "/d"); directory(nested); }
    for (int failure = 1; failure <= 10; failure++) {
        fail_alloc = failure;
        assert(os_remove_tree("tree") == -1); clean();
        assert(fail_alloc == 0 && exists(nested));
    }
    peak_bytes = 0; peak_handles = 0;
    removed("tree");
    assert(peak_bytes == 9 * 4096 && peak_handles == 9);
    printf("depth 9: peak shim scratch %zu bytes, peak handles %d\n", peak_bytes, peak_handles);
    removed("outside");
}

#endif

int main(void) {
    char scratch[4096];
    const char *created = os_temp_dir();
    assert(*created && strlen(created) < sizeof(scratch)); strcpy(scratch, created);
#if defined(_WIN32)
    assert(_chdir(scratch) == 0);
#else
    assert(chdir(scratch) == 0);
#endif
    basics(); links_and_races(); clean();
#if defined(_WIN32)
    assert(_chdir("..") == 0);
#else
    assert(chdir("..") == 0);
#endif
    removed(scratch);
    puts("remove-tree: ok");
    return 0;
}
