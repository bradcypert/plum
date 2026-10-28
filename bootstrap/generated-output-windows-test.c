// Included by generated-output-test.c on native Windows. Junctions need no
// Developer Mode privilege, unlike symbolic links.
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>
#include <winternl.h>
#include <winioctl.h>
#include <direct.h>

static int buffers, handles, allocation, fail_at, deny_swap, fail_rename;
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
    else if (!fail_at) fprintf(stderr, "CreateFileW(%ls, access=0x%lx, share=0x%lx): error %lu\n", p,
                              (unsigned long)a, (unsigned long)s, (unsigned long)GetLastError());
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
static NTSTATUS NTAPI tracked_publish(HANDLE file, PIO_STATUS_BLOCK io, PVOID info, ULONG size,
                                      FILE_INFORMATION_CLASS kind) {
    assert(kind == FileRenameInformation);
    FILE_RENAME_INFORMATION *rename_info = info;
    assert(rename_info->RootDirectory == NULL);
    assert(wcschr(rename_info->FileName, L'\\') == NULL);
    if (fail_rename) return (NTSTATUS)0xc0000022;
    typedef NTSTATUS (NTAPI *Rename)(HANDLE, PIO_STATUS_BLOCK, PVOID, ULONG, FILE_INFORMATION_CLASS);
    Rename rename_file = (Rename)(void *)GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtSetInformationFile");
    assert(rename_file);
    NTSTATUS status = rename_file(file, io, info, size, kind);
    if (status < 0 && !fail_at) fprintf(stderr, "NtSetInformationFile(FileRenameInformation): status 0x%lx\n",
                                      (unsigned long)status);
    return status;
}
static FARPROC WINAPI tracked_proc(HMODULE module, LPCSTR name) {
    if (!strcmp(name, "NtSetInformationFile")) return (FARPROC)(void *)tracked_publish;
    return GetProcAddress(module, name);
}
#define malloc tracked_malloc
#define free tracked_free
#define CreateFileW tracked_create
#define CloseHandle tracked_close
#define GetFileInformationByHandle tracked_info
#define GetProcAddress tracked_proc
#include "../native_stdlib/whole_file_shim.c"
#undef malloc
#undef free
#undef CreateFileW
#undef CloseHandle
#undef GetFileInformationByHandle
#undef GetProcAddress
long long os_rename_file(const char *from, const char *to) {
    wchar_t *a = plum_whole_wide(from), *b = plum_whole_wide(to);
    long long rc = a && b && MoveFileExW(a, b, MOVEFILE_REPLACE_EXISTING) ? 0 : -1;
    if (rc && !fail_at) fprintf(stderr, "MoveFileExW(%s, %s): error %lu\n", from, to, (unsigned long)GetLastError());
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
static void contents(const char *want) {
    FILE *f = fopen("held/page", "rb");
    assert(f);
    for (size_t i = 0; want[i]; i++) assert(fgetc(f) == want[i]);
    assert(fgetc(f) == EOF && fclose(f) == 0);
    WIN32_FIND_DATAW data;
    HANDLE find = FindFirstFileW(L"held/.*", &data);
    // Inspect only our reserved temporary-name prefix; . and .. may appear.
    if (find != INVALID_HANDLE_VALUE) {
        do { assert(wcsncmp(data.cFileName, L".plum-", 6) != 0); }
        while (FindNextFileW(find, &data));
        assert(FindClose(find));
    }
}
int main(void) {
    WCHAR tmp[MAX_PATH], root[MAX_PATH], cwd[4096];
    assert(GetCurrentDirectoryW(4096, cwd));
    assert(GetTempPathW(MAX_PATH, tmp));
    assert(GetTempFileNameW(tmp, L"pdo", 0, root));
    assert(DeleteFileW(root) && CreateDirectoryW(root, NULL) && SetCurrentDirectoryW(root));
    assert(CreateDirectoryW(L"held", NULL) && CreateDirectoryW(L"outside", NULL));
    deny_swap = 1;
    long long result = plum_write_generated_file("held/page", "new", 3);
    if (result) fprintf(stderr, "initial generated write: result=%lld, swap hook pending=%d, error=%lu, errno=%d\n",
                        result, deny_swap, (unsigned long)GetLastError(), errno);
    assert(result == 0);
    assert(!deny_swap);
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
        fail_at = 0;
        assert(plum_write_generated_file("held/page", "old", 3) == 0);
        allocation = 0; fail_at = i;
        long long rc = plum_write_generated_file("held/page", "new", 3);
        assert(buffers == 0 && handles == 0);
        contents(rc == 0 ? "new" : "old");
    }
    fail_at = 0;
    assert(plum_write_generated_file("held/page", "old", 3) == 0);
    fail_rename = 1;
    assert(plum_write_generated_file("held/page", "bad", 3) != 0);
    fail_rename = 0;
    assert(buffers == 0 && handles == 0);
    contents("old");
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
