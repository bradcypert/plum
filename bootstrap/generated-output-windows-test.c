// Included by generated-output-test.c on native Windows. Junctions need no
// Developer Mode privilege, unlike symbolic links.
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>
#include <winioctl.h>
#include <direct.h>

static int buffers, handles, allocation, fail_at, deny_swap;
static void *tracked_malloc(size_t n) {
    if (++allocation == fail_at) return NULL;
    void *p = malloc(n);
    if (p) buffers++;
    return p;
}
static void tracked_free(void *p) { if (p) { buffers--; free(p); } }
static HANDLE WINAPI tracked_create(LPCWSTR p, DWORD a, DWORD s, LPSECURITY_ATTRIBUTES sa,
                                    DWORD c, DWORD f, HANDLE t) {
    HANDLE h = CreateFileW(p, a, s, sa, c, f, t);
    if (h != INVALID_HANDLE_VALUE) handles++;
    return h;
}
static BOOL WINAPI tracked_close(HANDLE h) { handles--; return CloseHandle(h); }
static BOOL WINAPI tracked_info(HANDLE h, LPBY_HANDLE_FILE_INFORMATION info) {
    if (deny_swap) {
        WCHAR path[4096];
        DWORD n = GetFinalPathNameByHandleW(h, path, 4096, FILE_NAME_NORMALIZED);
        assert(n && n < 4096);
        if (n >= 5 && wcscmp(path + n - 5, L"\\held") == 0) {
            deny_swap = 0;
            assert(!MoveFileW(L"held", L"parked"));
            assert(GetLastError() == ERROR_SHARING_VIOLATION);
            HANDLE writer = CreateFileW(L"held", GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL,
                OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, NULL);
            assert(writer == INVALID_HANDLE_VALUE);
        }
    }
    return GetFileInformationByHandle(h, info);
}
#define malloc tracked_malloc
#define free tracked_free
#define CreateFileW tracked_create
#define CloseHandle tracked_close
#define GetFileInformationByHandle tracked_info
#include "../native_stdlib/whole_file_shim.c"
#undef malloc
#undef free
#undef CreateFileW
#undef CloseHandle
#undef GetFileInformationByHandle
long long os_rename_file(const char *from, const char *to) {
    wchar_t *a = plum_whole_wide(from), *b = plum_whole_wide(to);
    long long rc = a && b && MoveFileExW(a, b, MOVEFILE_REPLACE_EXISTING) ? 0 : -1;
    tracked_free(a); tracked_free(b);
    return rc;
}
static void junction(void) {
    assert(CreateDirectoryW(L"junction", NULL));
    WCHAR full[4096], substitute[4104];
    assert(GetFullPathNameW(L"outside", 4096, full, NULL));
    assert(swprintf(substitute, 4104, L"\\??\\%ls", full) > 0);
    struct {
        DWORD tag; WORD length, reserved;
        WORD sub_offset, sub_length, print_offset, print_length;
        WCHAR path[8192];
    } data = {0};
    size_t n = wcslen(substitute);
    data.tag = IO_REPARSE_TAG_MOUNT_POINT;
    data.sub_length = (WORD)(n * sizeof(WCHAR));
    data.print_offset = (WORD)((n + 1) * sizeof(WCHAR));
    memcpy(data.path, substitute, (n + 1) * sizeof(WCHAR));
    data.length = (WORD)(8 + (n + 2) * sizeof(WCHAR));
    HANDLE h = CreateFileW(L"junction", GENERIC_WRITE, 0, NULL, OPEN_EXISTING,
                          FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS, NULL);
    DWORD bytes;
    assert(h != INVALID_HANDLE_VALUE);
    assert(DeviceIoControl(h, FSCTL_SET_REPARSE_POINT, &data, data.length + 8, NULL, 0, &bytes, NULL));
    assert(CloseHandle(h));
}
int main(void) {
    WCHAR tmp[MAX_PATH], root[MAX_PATH], cwd[4096];
    assert(GetCurrentDirectoryW(4096, cwd));
    assert(GetTempPathW(MAX_PATH, tmp));
    assert(GetTempFileNameW(tmp, L"pdo", 0, root));
    assert(DeleteFileW(root) && CreateDirectoryW(root, NULL) && SetCurrentDirectoryW(root));
    assert(CreateDirectoryW(L"held", NULL) && CreateDirectoryW(L"outside", NULL));
    deny_swap = 1;
    assert(plum_write_generated_file("held/page", "new", 3) == 0 && !deny_swap);
    assert(buffers == 0 && handles == 0);
    // Sharing restrictions must end with the call: no retained handle may
    // prevent later renames or writes to the directory.
    assert(MoveFileW(L"held", L"parked") && MoveFileW(L"parked", L"held"));
    HANDLE writer = CreateFileW(L"held", GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL,
        OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, NULL);
    assert(writer != INVALID_HANDLE_VALUE && CloseHandle(writer));
    // Fail each allocation boundary; all native allocations and ancestor
    // handles must be gone before retrying, including partial lock walks.
    for (int i = 1; i < 30; i++) {
        allocation = 0; fail_at = i;
        plum_write_generated_file("held/page", "new", 3);
        assert(buffers == 0 && handles == 0);
    }
    fail_at = 0;
    junction();
    assert(plum_write_generated_file("junction/page", "bad", 3) != 0);
    assert(plum_write_generated_file("junction/new/page", "bad", 3) != 0);
    assert(GetFileAttributesW(L"outside/page") == INVALID_FILE_ATTRIBUTES);
    assert(GetFileAttributesW(L"outside/new") == INVALID_FILE_ATTRIBUTES);
    assert(buffers == 0 && handles == 0);
    assert(RemoveDirectoryW(L"junction"));
    assert(DeleteFileW(L"held/page") && RemoveDirectoryW(L"held") && RemoveDirectoryW(L"outside"));
    assert(SetCurrentDirectoryW(cwd) && RemoveDirectoryW(root));
    puts("generated output: Windows junctions, swap prevention and allocation cleanup verified");
}
