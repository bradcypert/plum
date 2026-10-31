// Native Windows gate for #109. Use real wide filesystem APIs as the oracle.
// Track shim-owned scratch and inject failure at every malloc in each probe.
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <wchar.h>
#include <windows.h>
#include <dirent.h>
#include <sys/stat.h>
#include <pthread.h>

static struct { void *ptr; size_t size; } tracked[2048];
static size_t live, peak;
static int attempt, fail_at;
static void *test_malloc(size_t size) {
    if (++attempt == fail_at) return NULL;
    void *p = malloc(size);
    if (p) {
        size_t i = 0;
        while (i < 2048 && tracked[i].ptr) i++;
        assert(i < 2048);
        tracked[i].ptr = p; tracked[i].size = size;
        live += size;
        if (live > peak) peak = live;
    }
    return p;
}
static void test_free(void *p) {
    for (size_t i = 0; i < 2048; i++) if (p && tracked[i].ptr == p) {
        live -= tracked[i].size; tracked[i].ptr = NULL; break;
    }
    free(p);
}
#define malloc test_malloc
#define free test_free
#include "../native_stdlib/os_shim.c"
#include "../native_stdlib/dir_shim.c"
#include "../native_stdlib/file_shim.c"
#include "../native_stdlib/whole_file_shim.c"
#undef malloc
#undef free

static const char *folder = "\xe6\x97\xa5-\xf0\x9f\x8c\x8d";
static const char *file = "\xe6\x97\xa5-\xf0\x9f\x8c\x8d/\xe9\x9b\xaa.txt";
static const char *copy = "\xe6\x97\xa5-\xf0\x9f\x8c\x8d/copy.txt";
static const wchar_t *wide_folder = L"\x65e5-\xd83c\xdf0d";
static const wchar_t *wide_file = L"\x65e5-\xd83c\xdf0d/\x96ea.txt";

static void check_contents(void) {
    FILE *f = _wfopen(wide_file, L"rb");
    assert(f);
    char buf[8] = {0};
    assert(fread(buf, 1, 7, f) == 7 && !memcmp(buf, "payload", 7));
    assert(fclose(f) == 0);
}

static void probe(void) {
    // Reads and repeatable writes may fail only at injected allocations.
    long long n;
    char *data = plum_read_whole_file(file, &n);
    if (data) { assert(n == 7 && !memcmp(data, "payload", 7)); test_free(data); }
    else assert(fail_at);
    long long h = file_open(file, 0);
    if (h > 0) assert(file_close_checked(h) == 0);
    else assert(fail_at);
    long long d = dir_open(folder);
    if (d >= 0) {
        int found = 0; long long size;
        while ((size = dir_next_size(d)) > 0) {
            assert((size_t)size == strlen(dir_entry_data(d)));
            found |= !strcmp(dir_entry_data(d), "\xe9\x9b\xaa.txt");
        }
        assert(size == 0 && found);
        dir_close(d);
    } else assert(fail_at);
    if (os_stat_load(file) == 0) assert(os_stat_size() == 7);
    else assert(fail_at);
    long long exists = os_path_exists(file);
    assert(exists == 1 || (fail_at && exists == -1));
    assert(path_is_dir(folder) == 1 || fail_at);
    assert(plum_write_whole_file(file, "payload", 7) == 0 || fail_at);
    const char *digest = plum_cache_hash_file(file);
    assert(strlen(digest) == 64 || fail_at);
    assert(plum_cache_copy(file, copy, "") == 0 || fail_at);
    if (os_path_exists(copy) == 1) assert(os_remove_file(copy) == 0 || fail_at);

}

int main(void) {
    wchar_t cwd[4096];
    assert(_wgetcwd(cwd, 4096));
    char root[4096]; snprintf(root, sizeof(root), "%s", os_temp_dir());
    assert(*root && os_chdir(root) == 0);
    assert(os_make_dir(folder) == 0);
    assert(GetFileAttributesW(wide_folder) != INVALID_FILE_ATTRIBUTES);
    assert(plum_write_whole_file(file, "payload", 7) == 0);
    check_contents();
    assert(live == 0);

    // Independent stream buffers and no per-entry allocation.
    long long a = dir_open(folder), b = dir_open(folder);
    assert(a >= 0 && b >= 0 && dir_next_size(a) > 0);
    char saved[1024]; strcpy(saved, dir_entry_data(a));
    int allocations = attempt;
    while (dir_next_size(b) > 0) {}
    assert(attempt == allocations && !strcmp(saved, dir_entry_data(a)));
    dir_close(a); dir_close(b); assert(live == 0);

    // Returned cwd is UTF-8 and accepted by every other path operation.
    char absolute[4096]; snprintf(absolute, sizeof(absolute), "%s/%s", root, folder);
    assert(os_chdir(absolute) == 0);
    assert(strstr(os_cwd(), folder));
    assert(os_chdir(root) == 0);

    // First determine the number of scratch allocations on a successful path.
    attempt = 0; probe(); int count = attempt;
    assert(live == 0);
    for (int i = 1; i <= count; i++) {
        attempt = 0; fail_at = i;
        probe(); fail_at = 0;
        assert(os_chdir(root) == 0 && live == 0);
        check_contents();
        os_remove_file(copy);
    }
    for (int i = 0; i < 256; i++) { probe(); assert(live == 0); }

    for (int failure = 0; failure <= 2; failure++) {
        assert(plum_write_whole_file(copy, "payload", 7) == 0);
        attempt = 0; fail_at = failure;
        long long rc = os_rename_file(copy, file);
        fail_at = 0;
        assert(rc == (failure ? -1 : 0));
        assert(live == 0); check_contents();
        if (failure) assert(os_remove_file(copy) == 0);
    }

    const char *bad[] = {"\xff", "\xc0\xaf", "\xed\xa0\x80", "\xf4\x90\x80\x80"};
    for (size_t i = 0; i < sizeof(bad) / sizeof(*bad); i++) {
        assert(file_open(bad[i], 1) == -1);
        assert(os_path_exists(bad[i]) == -1 && errno == EILSEQ);
        assert(os_stat_load(bad[i]) == -1 && os_make_dir(bad[i]) == -1);
        assert(os_remove_file(bad[i]) == -1 && os_chdir(bad[i]) == -1);
        assert(dir_open(bad[i]) == -1);
        assert(os_rename_file(file, bad[i]) == -1);
        assert(plum_write_whole_file(bad[i], "wrong", 5) == -1);
        assert(os_copy_tree(bad[i], "unused") == -1);
        assert(live == 0); check_contents();
    }
    // NTFS can store names that UTF-8 cannot represent. Enumeration must
    // report a read error, not a replacement name or apparent exhaustion.
    wchar_t invalid_name[] = L"\x65e5-\xd83c\xdf0d/\xd800";
    HANDLE invalid = CreateFileW(invalid_name, GENERIC_WRITE, 0, NULL, CREATE_NEW,
                                 FILE_ATTRIBUTE_NORMAL, NULL);
    assert(invalid != INVALID_HANDLE_VALUE && CloseHandle(invalid));
    long long bad_dir = dir_open(folder), size;
    assert(bad_dir >= 0);
    while ((size = dir_next_size(bad_dir)) > 0) {}
    assert(size == -1 && errno == EILSEQ && !*dir_entry_data(bad_dir));
    dir_close(bad_dir);
    assert(DeleteFileW(invalid_name) && live == 0);

    char out[8] = "stale";
    assert(!plum_path_utf8(L"\xd800", out, sizeof(out)) && !*out);
    assert(!plum_path_utf8(L"\x65e5\x65e5\x65e5", out, sizeof(out)) && !*out);

    // Cache staging, hashing and retention share the directory boundary.
    char stage[4096]; strcpy(stage, plum_cache_temp(folder));
    assert(*stage && os_path_exists(stage) == 1);
    assert(os_remove_tree(stage) == 0);
    char ir[4096], entry[4096], cached[4096];
    snprintf(ir, sizeof(ir), "%s/ir", folder);
    snprintf(entry, sizeof(entry), "%s/%064d", ir, 0);
    snprintf(cached, sizeof(cached), "%s/data", entry);
    assert(os_make_dir(ir) == 0 && os_make_dir(entry) == 0);
    assert(plum_write_whole_file(cached, "payload", 7) == 0);
    assert(plum_cache_prune(folder, 0) == 1);
    assert(os_path_exists(entry) == 0 && os_remove_tree(ir) == 0 && live == 0);

    // System paths must round-trip under a Unicode temporary-directory root.
    wchar_t temp_root[4096], old_temp[4096], old_tmp[4096];
    DWORD nt = GetEnvironmentVariableW(L"TEMP", old_temp, 4096);
    DWORD np = GetEnvironmentVariableW(L"TMP", old_tmp, 4096);
    assert(nt < 4096 && np < 4096);
    assert(GetFullPathNameW(wide_folder, 4096, temp_root, NULL));
    assert(SetEnvironmentVariableW(L"TEMP", temp_root) && SetEnvironmentVariableW(L"TMP", temp_root));
    char temporary[4096]; strcpy(temporary, os_temp_dir());
    assert(*temporary && strstr(temporary, folder) && os_path_exists(temporary) == 1);
    assert(os_remove_tree(temporary) == 0);
    assert(SetEnvironmentVariableW(L"TEMP", nt ? old_temp : NULL));
    assert(SetEnvironmentVariableW(L"TMP", np ? old_tmp : NULL));
    assert(*os_self_exe() && os_path_exists(os_self_exe()) == 1);
    assert(os_remove_file(file) == 0 && os_remove_tree(folder) == 0);
    assert(_wchdir(cwd) == 0 && os_remove_tree(root) == 0 && live == 0);
    printf("Windows UTF-8 filesystem: ok; peak tracked scratch %zu bytes\n", peak);
}
