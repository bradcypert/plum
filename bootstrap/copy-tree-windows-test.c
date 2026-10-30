// Native Windows tests: junctions work without developer-mode privileges.
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>
#include <winternl.h>
#include <winioctl.h>

static int fail_allocation, allocation;
static void *test_malloc(size_t n) {
    if (++allocation == fail_allocation) return NULL;
    return malloc(n);
}
static void *test_calloc(size_t n, size_t size) {
    if (++allocation == fail_allocation) return NULL;
    return calloc(n, size);
}
#define malloc test_malloc
#define calloc test_calloc
#include "../native_stdlib/os_shim.c"
#undef malloc
#undef calloc

static void directory(const char *path) { assert(CreateDirectoryA(path, NULL)); }
static void file(const char *path, const char *text) {
    FILE *f = fopen(path, "wb"); assert(f);
    assert(fwrite(text, 1, strlen(text), f) == strlen(text));
    assert(fclose(f) == 0);
}
static void contents(const char *path, const char *text) {
    FILE *f = fopen(path, "rb"); assert(f);
    for (size_t i = 0; text[i]; i++) assert(fgetc(f) == text[i]);
    assert(fgetc(f) == EOF && fclose(f) == 0);
}
static void junction(const char *name) {
    directory(name);
    WCHAR full[4096], substitute[4104], wide[4096];
    assert(GetFullPathNameW(L"outside", 4096, full, NULL));
    assert(swprintf(substitute, 4104, L"\\??\\%ls", full) > 0);
    assert(MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, name, -1, wide, 4096));
    struct {
        DWORD tag; WORD length, reserved;
        WORD sub_offset, sub_length, print_offset, print_length;
        WCHAR path[8192];
    } data = {0};
    data.tag = IO_REPARSE_TAG_MOUNT_POINT;
    size_t n = wcslen(substitute);
    data.sub_length = (WORD)(n * sizeof(WCHAR));
    data.print_offset = (WORD)((n + 1) * sizeof(WCHAR));
    memcpy(data.path, substitute, (n + 1) * sizeof(WCHAR));
    data.length = (WORD)(8 + (n + 2) * sizeof(WCHAR));
    HANDLE h = CreateFileW(wide, GENERIC_WRITE, 0, NULL, OPEN_EXISTING,
        FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS, NULL);
    assert(h != INVALID_HANDLE_VALUE);
    DWORD bytes;
    assert(DeviceIoControl(h, FSCTL_SET_REPARSE_POINT, &data, data.length + 8, NULL, 0, &bytes, NULL));
    assert(CloseHandle(h));
}
static DWORD handle_count(void) {
    DWORD count; assert(GetProcessHandleCount(GetCurrentProcess(), &count)); return count;
}
#include "tree-io-test.h"

int main(void) {
    WCHAR tmp[MAX_PATH], root[MAX_PATH], cwd[4096];
    assert(GetCurrentDirectoryW(4096, cwd));
    assert(GetTempPathW(MAX_PATH, tmp) && GetTempFileNameW(tmp, L"pcp", 0, root));
    assert(DeleteFileW(root) && CreateDirectoryW(root, NULL) && SetCurrentDirectoryW(root));
    // Native narrow argv may contain ACP filename bytes; enumerated names
    // and LSP paths use UTF-8. Both forms resolve to the same held root.
    WCHAR legacy_directory[] = L"native-\x00fc";
    assert(CreateDirectoryW(legacy_directory, NULL));
    char native_name[128], utf8_name[128];
    BOOL substituted = FALSE;
    assert(WideCharToMultiByte(CP_ACP, 0, legacy_directory, -1, native_name, sizeof(native_name), NULL,
        GetACP() == CP_UTF8 ? NULL : &substituted));
    assert(WideCharToMultiByte(CP_UTF8, 0, legacy_directory, -1, utf8_name, sizeof(utf8_name), NULL, NULL));
    long long utf8_root = plum_tree_root_open(utf8_name); assert(utf8_root);
    char native_identity[80]; strcpy(native_identity, plum_tree_entry_identity(utf8_root));
    if (!substituted) {
        long long native_root = plum_tree_root_open(native_name); assert(native_root);
        assert(!strcmp(native_identity, plum_tree_entry_identity(native_root)));
        plum_tree_entry_close(native_root);
    }
    plum_tree_entry_close(utf8_root);
    assert(RemoveDirectoryW(legacy_directory));
    directory("src"); directory("src/nested"); directory("dst"); directory("outside");
    file("src/nested/data", "copied"); file("outside/keep", "untouched");
    assert(os_copy_tree("src/", "dst/") == 0);
    contents("dst/nested/data", "copied");
    tree_io_tests();
    // Publication allocation failure must retain the original and release
    // every held target/parent/temporary handle.
    file("src/write-fault", "before");
    long long write_root = plum_tree_root_open("src"); assert(write_root);
    long long write_entry = plum_tree_entry_open(write_root, "write-fault", 0); assert(write_entry);
    char write_identity[80]; strcpy(write_identity, plum_tree_entry_identity(write_entry));
    plum_tree_entry_close(write_entry);
    DWORD write_baseline = handle_count();
    allocation = 0; fail_allocation = 1;
    assert(plum_tree_write(write_root, "write-fault", 0, write_identity, "after", 5) == -1);
    fail_allocation = 0;
    assert(handle_count() == write_baseline);
    contents("src/write-fault", "before");
    assert(!plum_tree_write(write_root, "write-fault", 0, write_identity, "after", 5));
    assert(handle_count() == write_baseline);
    contents("src/write-fault", "after");
    plum_tree_entry_close(write_root);
    assert(!remove("src/write-fault"));
    assert(os_copy_tree("src", "src") == -1);
    assert(os_copy_tree("src", "src/nested") == -1);
    assert(os_copy_tree("src/nested", "src") == -1);
    long long held = plum_tree_root_open("src");
    assert(held);
    long long resolved = plum_tree_entry_open(held, "nested/data", 0);
    assert(resolved);
    char bytes[6]; DWORD got;
    assert(ReadFile((HANDLE)(intptr_t)resolved, bytes, sizeof(bytes), &got, NULL));
    assert(got == sizeof(bytes) && !memcmp(bytes, "copied", sizeof(bytes)));
    plum_tree_entry_close(resolved);
    assert(!plum_tree_entry_open(held, "../outside/keep", 0));
    assert(!plum_tree_entry_open(held, "C:/Windows", 0));
    assert(!plum_tree_entry_open(held, "nested/data/", 0));
    plum_tree_entry_close(held);
    directory("one"); directory("output"); file("one/keep", "replacement");
    assert(CreateHardLinkA("output/keep", "outside/keep", NULL));
    assert(os_copy_tree("one", "output") == 0);
    contents("outside/keep", "untouched"); contents("output/keep", "replacement");
    junction("src/escape");
    held = plum_tree_root_open("src");
    assert(held && !plum_tree_entry_open(held, "escape/keep", 0));
    plum_tree_entry_close(held);
    DWORD baseline = handle_count();
    for (int i = 0; i < 50; i++) {
        assert(os_copy_tree("src", "dst") == -1);
        assert(handle_count() == baseline);
    }
    assert(GetFileAttributesA("dst/escape/keep") == INVALID_FILE_ATTRIBUTES);
    assert(os_copy_tree("src/escape/", "dst") == -1);
    assert(os_copy_tree("src", "src/escape/") == -1);
    assert(RemoveDirectoryA("src/escape"));
    directory("dir-source"); directory("dir-source/child"); directory("dir-output");
    file("dir-source/child/new", "new"); junction("dir-output/child");
    assert(os_copy_tree("dir-source", "dir-output") == -1);
    assert(GetFileAttributesA("outside/new") == INVALID_FILE_ATTRIBUTES);
    // Every allocation boundary must unwind all held directory/file handles.
    allocation = 0; fail_allocation = 0;
    assert(os_copy_tree("src", "dst") == 0);
    int boundaries = allocation;
    for (int i = 1; i <= boundaries; i++) {
        allocation = 0; fail_allocation = i;
        assert(os_copy_tree("src", "dst") == -1);
        assert(handle_count() == baseline);
    }
    fail_allocation = 0;
    // Unicode names are enumerated/opened without an ANSI conversion.
    assert(CreateDirectoryW(L"src/\x65e5", NULL));
    assert(os_copy_tree("src", "dst") == 0);
    DWORD unicode_attrs = GetFileAttributesW(L"dst/\x65e5");
    assert(unicode_attrs != INVALID_FILE_ATTRIBUTES && (unicode_attrs & FILE_ATTRIBUTE_DIRECTORY));
    // File symlinks: absence of privilege is an explicit unsupported case.
    if (CreateSymbolicLinkW(L"one/link", L"keep", 2)) {
        held = plum_tree_root_open("one"); assert(held);
        resolved = plum_tree_entry_open(held, "link", 0); assert(resolved);
        plum_tree_entry_close(resolved);
        assert(!plum_tree_entry_open(held, "link", 1));
        plum_tree_entry_close(held);
        long long copied = os_copy_tree("one", "output");
        if (!copied) {
            HANDLE h = CreateFileW(L"output/link", FILE_READ_ATTRIBUTES, FILE_SHARE_READ, NULL,
                OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT, NULL);
            assert(h != INVALID_HANDLE_VALUE);
            FILE_ATTRIBUTE_TAG_INFO attrs;
            assert(GetFileInformationByHandleEx(h, FileAttributeTagInfo, &attrs, sizeof(attrs)));
            assert(attrs.ReparseTag == IO_REPARSE_TAG_SYMLINK);
            CloseHandle(h);
        } else puts("copy-tree: symlink recreation unavailable; explicit copy failure verified");
    } else puts("copy-tree: symbolic-link creation privilege unavailable");
    assert(handle_count() == baseline);
    assert(os_remove_tree("src") == 0 && os_remove_tree("dst") == 0);
    assert(os_remove_tree("one") == 0 && os_remove_tree("output") == 0);
    assert(os_remove_tree("dir-source") == 0 && os_remove_tree("dir-output") == 0);
    assert(os_remove_tree("outside") == 0);
    assert(SetCurrentDirectoryW(cwd) && RemoveDirectoryW(root));
    puts("copy-tree Windows tests: ok");
}
