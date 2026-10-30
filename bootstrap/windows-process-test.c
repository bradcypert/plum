// Run by platform-smoke on Windows. Include the real shim so allocation
// failures and live scratch bytes can be checked without a production ABI.
#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <io.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>
#include <windows.h>
#include <shellapi.h>

static size_t live_bytes, live_allocs, peak_bytes;
static int allocation_attempt, fail_allocation;
static int fail_snapshot, live_snapshots;
static wchar_t *snapshot_override;
static int expect_no_wait_scratch;
typedef union { size_t size; max_align_t alignment; } Allocation;

static void *checked_malloc(size_t n) {
    if (++allocation_attempt == fail_allocation) return NULL;
    Allocation *p = malloc(sizeof(*p) + n);
    assert(p != NULL);
    p->size = n;
    live_bytes += n;
    live_allocs++;
    if (live_bytes > peak_bytes) peak_bytes = live_bytes;
    return p + 1;
}
static void checked_free(void *ptr) {
    if (ptr == NULL) return;
    Allocation *p = (Allocation *)ptr - 1;
    assert(live_allocs > 0 && live_bytes >= p->size);
    live_bytes -= p->size;
    live_allocs--;
    free(p);
}
static void *checked_calloc(size_t n, size_t size) {
    assert(size == 0 || n <= SIZE_MAX / size);
    void *p = checked_malloc(n * size);
    if (p != NULL) memset(p, 0, n * size);
    return p;
}
static char *checked_strdup(const char *s) {
    size_t n = strlen(s) + 1;
    char *p = checked_malloc(n);
    if (p != NULL) memcpy(p, s, n);
    return p;
}
static LPWCH checked_snapshot(void) {
    if (fail_snapshot) return NULL;
    LPWCH p = snapshot_override ? snapshot_override : GetEnvironmentStringsW();
    if (p != NULL) live_snapshots++;
    return p;
}
static BOOL checked_free_snapshot(LPWCH p) {
    assert(live_snapshots > 0);
    live_snapshots--;
    return p == snapshot_override ? TRUE : FreeEnvironmentStringsW(p);
}
static DWORD checked_wait(HANDLE h, DWORD ms) {
    // Check lifetime BEFORE the child wait, not merely at function exit.
    if (expect_no_wait_scratch) assert(live_bytes == 0 && live_allocs == 0 && live_snapshots == 0);
    return WaitForSingleObject(h, ms);
}
#define FreeEnvironmentStringsW checked_free_snapshot
#define WaitForSingleObject checked_wait
#define malloc checked_malloc
#define calloc checked_calloc
#define free checked_free
#define strdup checked_strdup
#define GetEnvironmentStringsW checked_snapshot
#include "../native_stdlib/process_shim.c"
#include "../native_stdlib/whole_file_shim.c"
#undef WaitForSingleObject
#undef FreeEnvironmentStringsW
#undef malloc
#undef calloc
#undef free
#undef strdup
#undef GetEnvironmentStringsW

static void no_scratch(void) { assert(live_bytes == 0 && live_allocs == 0 && live_snapshots == 0); }

static void test_builders(void) {
    char *args[] = {"tool", "", "hello world", "\xe6\x97\xa5\xf0\x9f\x8c\x8d", "a\\\"b", "end \\", NULL};
    wchar_t *line = plum_win_command_line(args);
    assert(line != NULL);
    int argc;
    wchar_t **argv = CommandLineToArgvW(line, &argc);
    assert(argv != NULL && argc == 6);
    assert(wcscmp(argv[1], L"") == 0);
    assert(wcscmp(argv[2], L"hello world") == 0);
    assert(wcscmp(argv[3], L"\x65e5\xd83c\xdf0d") == 0);
    assert(wcscmp(argv[4], L"a\\\"b") == 0);
    assert(wcscmp(argv[5], L"end \\") == 0);
    LocalFree(argv);
    checked_free(line);
    no_scratch();

    const char *invalid[] = {"\xc0\xaf", "\xed\xa0\x80", "\xf4\x90\x80\x80", "\xe2\x82", NULL};
    for (int i = 0; invalid[i]; i++) {
        char *bad_args[] = {"tool", (char *)invalid[i], NULL};
        assert(plum_win_command_line(bad_args) == NULL);
        char *bad_env[] = {(char *)invalid[i], NULL};
        wchar_t *block = (wchar_t *)1;
        assert(plum_win_env_block(bad_env, &block) == -1 && block == NULL);
        no_scratch();
    }
    char *oversize = malloc(32768);
    assert(oversize != NULL);
    memset(oversize, 'x', 32767);
    oversize[32767] = 0;
    char *long_args[] = {oversize, NULL};
    assert(plum_win_command_line(long_args) == NULL);
    oversize[32766] = 0;
    line = plum_win_command_line(long_args);
    assert(line != NULL && wcslen(line) == 32766);
    checked_free(line);
    free(oversize);
    no_scratch();

    // The limit counts UTF-16 units, not UTF-8 bytes. Exercise both
    // three-byte BMP characters and supplementary surrogate pairs.
    for (int supplementary = 0; supplementary <= 1; supplementary++) {
        const char *cp = supplementary ? "\xf0\x9f\x8c\x8d" : "\xe6\x97\xa5";
        size_t width = supplementary ? 4 : 3;
        size_t count = supplementary ? 16383 : 32766;
        char *text = malloc(count * width + 1);
        assert(text != NULL);
        for (size_t i = 0; i < count; i++) memcpy(text + i * width, cp, width);
        text[count * width] = 0;
        char *unicode_args[] = {text, NULL};
        line = plum_win_command_line(unicode_args);
        assert(line != NULL && wcslen(line) == 32766);
        checked_free(line);
        free(text);
        no_scratch();
    }

    assert(SetEnvironmentVariableW(L"Plum_Mixed", L"parent"));
    assert(SetEnvironmentVariableW(L"Plum_\x65e5", L"parent"));
    char *extra[] = {"z_plum=last", "PLUM_MIXED=first", "a_plum=first", "plum_mixed=final",
                     "PLUM_\xe6\x97\xa5=\xf0\x9f\x8c\x8d", "plum_empty=", NULL};
    wchar_t *block = NULL;
    assert(plum_win_env_block(extra, &block) == 0 && block != NULL);
    int matches = 0, unicode = 0, empty = 0;
    PlumWinEnvEntry prev = {0};
    for (wchar_t *p = block; *p; p += wcslen(p) + 1) {
        wchar_t *eq = wcschr(p + (*p == L'='), L'=');
        assert(eq != NULL);
        PlumWinEnvEntry current = {.text = p, .key_len = (int)(eq - p)};
        if (prev.text) assert(plum_win_env_key_compare(&prev, &current) < 0);
        prev = current;
        if (wcscmp(p, L"plum_mixed=final") == 0) matches++;
        if (wcscmp(p, L"PLUM_\x65e5=\xd83c\xdf0d") == 0) unicode++;
        if (wcscmp(p, L"plum_empty=") == 0) empty++;
    }
    assert(matches == 1 && unicode == 1 && empty == 1);
    checked_free(block);
    no_scratch();
    assert(plum_win_env_block(NULL, &block) == 0 && block == NULL);
    char *malformed[] = {"missing_equals", "=reserved=value", "=", NULL};
    for (int i = 0; malformed[i]; i++) {
        char *one[] = {malformed[i], NULL};
        assert(plum_win_env_block(one, &block) == -1 && block == NULL);
        no_scratch();
    }
    wchar_t inherited[] = L"=C:=C:\\work\0Path=parent\0PLUM_keep=\x65e5\0";
    snapshot_override = inherited;
    char *path_overlay[] = {"PATH=custom", NULL};
    assert(plum_win_env_block(path_overlay, &block) == 0);
    const wchar_t want[] = L"=C:=C:\\work\0PATH=custom\0PLUM_keep=\x65e5\0";
    assert(memcmp(block, want, sizeof(want)) == 0);
    checked_free(block);
    wchar_t empty_parent[] = L"";
    snapshot_override = empty_parent;
    assert(plum_win_env_block(path_overlay, &block) == 0);
    assert(memcmp(block, L"PATH=custom\0", sizeof(L"PATH=custom\0")) == 0);
    checked_free(block);
    snapshot_override = NULL;
    no_scratch();
    fail_snapshot = 1;
    assert(plum_win_env_block(extra, &block) == -1 && block == NULL);
    fail_snapshot = 0;
    no_scratch();

    // Fail every allocation, including failures after earlier conversions
    // succeeded. Check cleanup immediately, before the next operation.
    for (int fail = 1;; fail++) {
        allocation_attempt = 0;
        fail_allocation = fail;
        int result = plum_win_env_block(extra, &block);
        int attempts = allocation_attempt;
        if (result == 0) checked_free(block);
        else assert(block == NULL);
        no_scratch();
        if (fail > attempts) break;
        assert(result == -1);
    }
    for (int fail = 1; fail <= 3; fail++) {
        allocation_attempt = 0;
        fail_allocation = fail;
        line = plum_win_command_line(args);
        if (fail <= 2) assert(line == NULL);
        else assert(line != NULL);
        checked_free(line);
        no_scratch();
    }
    fail_allocation = 0;
}

static void test_spawns(void) {
    wchar_t original[4096], base[4096], path[4096];
    assert(GetModuleFileNameW(NULL, original, 4096));
    assert(GetTempPathW(4096, base));
    swprintf(path, 4096, L"%lsplum \x65e5\xd83c\xdf0d-%lu.exe", base, GetCurrentProcessId());
    assert(CopyFileW(original, path, FALSE));
    char program[16384];
    assert(WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, path, -1, program, sizeof(program), NULL, NULL));
    const char *joined = "--child\t\xe6\x97\xa5\xf0\x9f\x8c\x8d\t\thello world\ta\\\"b\tend \\";
    const char *env = "PLUM_MIXED=first\tplum_mixed=final\tPLUM_\xe6\x97\xa5=\xf0\x9f\x8c\x8d";
    char *direct_args[] = {program, "--child", "\xe6\x97\xa5\xf0\x9f\x8c\x8d", "", "hello world", "a\\\"b", "end \\", NULL};
    char *direct_env[] = {"PLUM_MIXED=final", "PLUM_\xe6\x97\xa5=\xf0\x9f\x8c\x8d", NULL};
    char out_path[PLUM_PROC_PATH_MAX], err_path[PLUM_PROC_PATH_MAX];
    expect_no_wait_scratch = 1;
    assert(plum_spawn_capture(program, direct_args, out_path, err_path,
                              direct_env, NULL, 0, 0, 1, NULL) == 0);
    expect_no_wait_scratch = 0;
    remove(out_path); remove(err_path);
    no_scratch();
    // Failure while unpacking a requested overlay must also fail closed.
    for (int fail = 3; fail <= 4; fail++) {
        fail_allocation = fail;
        allocation_attempt = 0;
        assert(process_run_ex(program, joined, 6, env, 3, "", 0, 0) == -1);
        no_scratch();
        allocation_attempt = 0;
        assert(process_spawn(program, joined, 6, env, 3, "", 0, 0) == -1);
        no_scratch();
    }
    fail_allocation = 0;
    for (int i = 0; i < 8; i++) {
        long long h = process_run_ex(program, joined, 6, env, 3, "", 0, 0);
        assert(h >= 0 && process_exit_code(h) == 0);
        assert(strcmp(process_stdout_data(h), "ok\n") == 0);
        process_free(h);
        no_scratch();
        h = process_spawn(program, joined, 6, env, 3, "", 0, 0);
        assert(h >= 0 && process_child_wait(h, -1) == 1 && process_exit_code(h) == 0);
        assert(strcmp(process_stdout_data(h), "ok\n") == 0);
        // Collection must delete the captures after closing Windows handles.
        char *out = _strdup(plum_process_slots[h].child->out_path);
        char *err = _strdup(plum_process_slots[h].child->err_path);
        assert(GetFileAttributesA(out) == INVALID_FILE_ATTRIBUTES);
        assert(GetFileAttributesA(err) == INVALID_FILE_ATTRIBUTES);
        process_child_free(h);
        free(out); free(err);
        no_scratch();
    }
    assert(SetEnvironmentVariableW(L"PLUM_MIXED", L"final"));
    assert(SetEnvironmentVariableW(L"PLUM_\x65e5", L"\xd83c\xdf0d"));
    expect_no_wait_scratch = 1;
    assert(plum_spawn_inherit(program, direct_args) == 0);
    expect_no_wait_scratch = 0;
    assert(process_run_inherit(program, joined, 6) == 0);
    no_scratch();
    assert(process_run_ex(program, "\xff", 1, "", 0, "", 0, 0) == -1);
    assert(process_spawn(program, joined, 6, "KEY=\xff", 1, "", 0, 0) == -1);
    assert(process_run_inherit("\xff", "", 0) == -1);
    assert(process_run_inherit(program, "\xff", 1) == -1);
    assert(process_run_ex("plum-no-such-program-108.exe", "", 0, "KEY=value", 1, "", 0, 0) == -1);
    no_scratch();
    assert(DeleteFileW(path));
}

int main(void) {
    int argc;
    wchar_t **argv = CommandLineToArgvW(GetCommandLineW(), &argc);
    assert(argv != NULL);
    if (argc > 1 && wcscmp(argv[1], L"--child") == 0) {
        assert(argc == 7);
        assert(wcscmp(argv[2], L"\x65e5\xd83c\xdf0d") == 0);
        assert(wcscmp(argv[3], L"") == 0);
        assert(wcscmp(argv[4], L"hello world") == 0);
        assert(wcscmp(argv[5], L"a\\\"b") == 0);
        assert(wcscmp(argv[6], L"end \\") == 0);
        wchar_t value[64];
        assert(GetEnvironmentVariableW(L"Plum_Mixed", value, 64) && wcscmp(value, L"final") == 0);
        assert(GetEnvironmentVariableW(L"Plum_\x65e5", value, 64) && wcscmp(value, L"\xd83c\xdf0d") == 0);
        LocalFree(argv);
        // Preserve LF even when captured by the Windows CRT.
        assert(_setmode(_fileno(stdout), _O_BINARY) != -1);
        fwrite("ok\n", 1, 3, stdout);
        return 0;
    }
    LocalFree(argv);
    test_builders();
    test_spawns();
    printf("Windows process Unicode, overlays, allocation failures: ok (peak scratch %zu bytes)\n", peak_bytes);
    return 0;
}
