// Run by platform-smoke on Windows. Include the real shim so allocation
// failures and live scratch bytes can be checked without a production ABI.
#include <assert.h>
#include <limits.h>
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
static int fail_exit_query, exit_queries;
// Script only the OS wait boundary: the real child state, collection and
// allocator remain in use, so multi-day waits take no wall-clock time.
static int scripted_wait, wait_calls, wait_stop_call, wait_allocation_baseline;
static DWORD wait_stop_status;
static long long wait_remaining, wait_consumed;
static size_t wait_live_baseline;
static int stdin_write_mode, stdin_write_calls, process_creations;
static const char *stdin_base;
static size_t stdin_offset, stdin_expected;
static wchar_t failed_input_path[4096];
typedef union { size_t size; max_align_t alignment; } Allocation;

static BOOL checked_write(HANDLE h, LPCVOID data, DWORD count, LPDWORD written, LPOVERLAPPED overlapped) {
    if (stdin_write_mode == 0) return WriteFile(h, data, count, written, overlapped);
    assert(count > 0 && count <= 1024 * 1024 && overlapped == NULL);
    stdin_write_calls++;
    if (stdin_write_mode == 1) {
        // A reserved, uncommitted range checks >4 GiB accounting without
        // allocating physical memory or writing gigabytes to the CI disk.
        assert((const char *)data == stdin_base + stdin_offset);
        assert(count <= stdin_expected - stdin_offset);
        *written = stdin_write_calls == 1 && count > 1 ? count / 2 : count;
        stdin_offset += *written;
        return TRUE;
    }
    if (stdin_write_calls == 2 && stdin_write_mode >= 3) {
        assert(GetFinalPathNameByHandleW(h, failed_input_path, 4096, FILE_NAME_NORMALIZED));
        *written = 0;
        if (stdin_write_mode == 3) { SetLastError(ERROR_DISK_FULL); return FALSE; }
        return TRUE;
    }
    // Exercise real short writes too, including embedded NUL/CR/LF/Ctrl-Z.
    if (count > 65537) count = 65537;
    return WriteFile(h, data, count, written, overlapped);
}
static BOOL checked_create_process(LPCWSTR app, LPWSTR line, LPSECURITY_ATTRIBUTES pa,
                                  LPSECURITY_ATTRIBUTES ta, BOOL inherit, DWORD flags,
                                  LPVOID env, LPCWSTR cwd, LPSTARTUPINFOW si, LPPROCESS_INFORMATION pi) {
    process_creations++;
    return CreateProcessW(app, line, pa, ta, inherit, flags, env, cwd, si, pi);
}

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
static BOOL checked_exit_code(HANDLE h, LPDWORD code) {
    exit_queries++;
    if (fail_exit_query) {
        // Deliberately leave the output untouched on failure.
        SetLastError(ERROR_ACCESS_DENIED);
        return FALSE;
    }
    return GetExitCodeProcess(h, code);
}
static DWORD checked_wait(HANDLE h, DWORD ms) {
    // Check lifetime BEFORE the child wait, not merely at function exit.
    if (expect_no_wait_scratch) assert(live_bytes == 0 && live_allocs == 0 && live_snapshots == 0);
    if (scripted_wait) {
        assert(allocation_attempt == wait_allocation_baseline && live_bytes == wait_live_baseline);
        DWORD expected = wait_remaining > 0x7FFFFFFELL ? 0x7FFFFFFE : (DWORD)wait_remaining;
        assert(ms == expected && ms != INFINITE);
        wait_calls++;
        if (wait_calls == wait_stop_call) return wait_stop_status;
        wait_remaining -= ms;
        wait_consumed += ms;
        return WAIT_TIMEOUT;
    }
    return WaitForSingleObject(h, ms);
}
#define FreeEnvironmentStringsW checked_free_snapshot
#define WaitForSingleObject checked_wait
#define GetExitCodeProcess checked_exit_code
#define malloc checked_malloc
#define calloc checked_calloc
#define free checked_free
#define strdup checked_strdup
#define GetEnvironmentStringsW checked_snapshot
#define WriteFile checked_write
#define CreateProcessW checked_create_process
#include "../native_stdlib/process_shim.c"
#include "../native_stdlib/whole_file_shim.c"
#undef CreateProcessW
#undef WriteFile
#undef WaitForSingleObject
#undef GetExitCodeProcess
#undef FreeEnvironmentStringsW
#undef malloc
#undef calloc
#undef free
#undef strdup
#undef GetEnvironmentStringsW

static void no_scratch(void) { assert(live_bytes == 0 && live_allocs == 0 && live_snapshots == 0); }

static void test_stdin_accounting(void) {
    const size_t lengths[] = {0, 1, 1024 * 1024 - 1, 1024 * 1024, 1024 * 1024 + 1,
                             (size_t)UINT32_MAX - 1, (size_t)UINT32_MAX,
                             (size_t)UINT32_MAX + 1, (size_t)UINT32_MAX + 18};
    size_t largest = lengths[sizeof(lengths) / sizeof(lengths[0]) - 1];
    char *reserved = VirtualAlloc(NULL, largest, MEM_RESERVE, PAGE_NOACCESS);
    assert(reserved != NULL);
    stdin_base = reserved;
    stdin_write_mode = 1;
    for (size_t i = 0; i < sizeof(lengths) / sizeof(lengths[0]); i++) {
        stdin_offset = stdin_write_calls = 0;
        stdin_expected = lengths[i];
        int allocations_before = allocation_attempt;
        assert(plum_win_write_stdin(INVALID_HANDLE_VALUE, reserved, (long long)lengths[i]) == 0);
        assert(stdin_offset == lengths[i]);
        assert((stdin_write_calls == 0) == (lengths[i] == 0));
        assert(allocation_attempt == allocations_before);
        no_scratch();
    }
    stdin_write_calls = 0;
    assert(plum_win_write_stdin(INVALID_HANDLE_VALUE, NULL, 0) == 0);
    assert(plum_win_write_stdin(INVALID_HANDLE_VALUE, NULL, 1) == -1);
    assert(plum_win_write_stdin(INVALID_HANDLE_VALUE, reserved, -1) == -1);
    assert(stdin_write_calls == 0);
    stdin_write_mode = 0;
    assert(VirtualFree(reserved, 0, MEM_RELEASE));
}

static void test_stdin_spawns(const char *program) {
    const size_t length = 2 * 1024 * 1024 + 17;
    char *input = malloc(length);
    assert(input != NULL);
    for (size_t i = 0; i < length; i++) input[i] = (char)(i % 256);
    for (int async = 0; async <= 1; async++) {
        for (int mode = 0; mode <= 4; mode++) {
            stdin_write_mode = mode;
            stdin_write_calls = 0;
            // Mode 1 is reserved for synthetic accounting above.
            if (mode == 1) continue;
            int before = process_creations;
            long long h = async ? process_spawn(program, "--stdin-child", 1, "", 0, input, length, 1) :
                                 process_run_ex(program, "--stdin-child", 1, "", 0, input, length, 1);
            if (mode >= 3) {
                assert(h == -1 && process_creations == before && stdin_write_calls == 2);
                assert(GetFileAttributesW(failed_input_path) == INVALID_FILE_ATTRIBUTES);
            } else {
                assert(h >= 0 && process_creations == before + 1);
                if (async) assert(process_child_wait(h, -1) == 1);
                assert(process_exit_code(h) == 0 && process_capture_status(h) == 0);
                process_free(h);
            }
            no_scratch();
            for (int i = 0; i < PLUM_PROC_MAX_SLOTS; i++) assert(!plum_process_slots[i].in_use);
        }
        stdin_write_mode = 0;
        long long h = async ? process_spawn(program, "--stdin-empty", 1, "", 0, "", 0, 1) :
                             process_run_ex(program, "--stdin-empty", 1, "", 0, "", 0, 1);
        assert(h >= 0);
        if (async) assert(process_child_wait(h, -1) == 1);
        assert(process_exit_code(h) == 0);
        process_free(h);
        no_scratch();
    }
    free(input);
}

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

static long long await_status(long long h, int method) {
    // The caller has already observed the process handle signaled.
    if (method == 0) return process_poll(h);
    return process_child_wait(h, method == 1 ? -1 : 30000000000LL);
}

static void test_exit_statuses(const char *program) {
    const unsigned long codes[] = {0x80000000UL, 0xffffffffUL, 0, 1, 259, 0x7fffffffUL};
    for (size_t i = 0; i < sizeof(codes) / sizeof(codes[0]); i++) {
        char args[64];
        snprintf(args, sizeof(args), "--exit-child\t%lu", codes[i]);
        for (int failure = 0; failure <= 1; failure++) {
            fail_exit_query = failure;
            long long h = process_run(program, args, 2);
            if (failure) assert(h == -1);
            else {
                assert(h >= 0 && process_exit_code(h) == (long long)codes[i]);
                process_free(h);
            }
            no_scratch();
            h = process_run_ex(program, args, 2, "", 0, "", 0, 0);
            if (failure) assert(h == -1);
            else {
                assert(h >= 0 && process_exit_code(h) == (long long)codes[i]);
                process_free(h);
            }
            assert(process_run_inherit(program, args, 2) == (failure ? -1 : (long long)codes[i]));
            no_scratch();
            fail_exit_query = 0;
            for (int method = 0; method < 3; method++) {
                h = process_spawn(program, args, 2, "", 0, "", 0, 0);
                assert(h >= 0);
                HANDLE child = (HANDLE)(intptr_t)plum_process_slots[(h - 1) % PLUM_PROC_MAX_SLOTS].child->pid;
                assert(WaitForSingleObject(child, 30000) == WAIT_OBJECT_0);
                fail_exit_query = failure;
                if (failure) {
                    assert(await_status(h, method) == -1);
                    assert(!plum_process_slots[(h - 1) % PLUM_PROC_MAX_SLOTS].child->finished);
                    assert(plum_process_slots[(h - 1) % PLUM_PROC_MAX_SLOTS].child->pid == (long long)(intptr_t)child);
                    assert(plum_process_slots[(h - 1) % PLUM_PROC_MAX_SLOTS].out_data == NULL);
                    fail_exit_query = 0;
                }
                assert(await_status(h, method) == 1);
                assert(process_exit_code(h) == (long long)codes[i]);
                int queries = exit_queries;
                assert(process_poll(h) == 1 && process_child_wait(h, -1) == 1);
                assert(exit_queries == queries);
                process_child_free(h);
                no_scratch();
            }
        }
    }
    // Dropping a child discards its result, so status retrieval failure
    // must not prevent releasing its resources or allocate capture buffers.
    long long h = process_spawn(program, "--exit-child\t0", 2, "", 0, "", 0, 0);
    assert(h >= 0);
    assert(WaitForSingleObject((HANDLE)(intptr_t)plum_process_slots[(h - 1) % PLUM_PROC_MAX_SLOTS].child->pid, 30000) == WAIT_OBJECT_0);
    fail_exit_query = 1;
    assert(process_poll(h) == -1);
    int attempts = allocation_attempt;
    int queries = exit_queries;
    process_child_free(h);
    assert(allocation_attempt == attempts && exit_queries == queries);
    fail_exit_query = 0;
    no_scratch();
    for (int i = 0; i < PLUM_PROC_MAX_SLOTS; i++) assert(!plum_process_slots[i].in_use);
}

static void arm_wait(long long nanos, int stop_call, DWORD status) {
    wait_remaining = nanos / 1000000LL + (nanos % 1000000LL != 0);
    wait_calls = 0;
    wait_consumed = 0;
    wait_stop_call = stop_call;
    wait_stop_status = status;
    wait_allocation_baseline = allocation_attempt;
    wait_live_baseline = live_bytes;
    scripted_wait = 1;
}

// An inheritable capability unrelated to stdio must stay in the parent.
static void test_handle_inheritance(const char *program) {
    SECURITY_ATTRIBUTES sa = {sizeof(sa), NULL, TRUE};
    HANDLE event = CreateEventW(&sa, TRUE, FALSE, NULL);
    assert(event != NULL);
    char args[96];
    snprintf(args, sizeof(args), "--probe-handle\t%llu", (unsigned long long)(uintptr_t)event);
    for (int mode = 0; mode < 4; mode++) {
        if (mode == 3) assert(process_run_inherit(program, args, 2) == 0);
        else {
            long long h = mode == 0 ? process_run(program, args, 2) :
                mode == 1 ? process_run_ex(program, args, 2, "", 0, "", 0, 0) :
                            process_spawn(program, args, 2, "", 0, "", 0, 0);
            assert(h >= 0);
            if (mode == 2) assert(process_child_wait(h, -1) == 1);
            assert(process_exit_code(h) == 0);
            process_free(h);
        }
        assert(WaitForSingleObject(event, 0) == WAIT_TIMEOUT);
        no_scratch();
    }
    // OOM after duplicating all three handles must release both heap scratch
    // and the duplicated kernel handles without starting the child.
    DWORD before, after;
    assert(GetProcessHandleCount(GetCurrentProcess(), &before));
    int creations = process_creations;
    wchar_t command[] = L"plum-must-not-start.exe";
    PROCESS_INFORMATION child;
    fail_allocation = allocation_attempt + 1;
    assert(!plum_win_create_process(command, NULL, event, event, event, &child));
    fail_allocation = 0;
    assert(process_creations == creations);
    no_scratch();
    assert(GetProcessHandleCount(GetCurrentProcess(), &after) && before == after);
    CloseHandle(event);
}

static void test_wait_timeouts(const char *program) {
    long long h = process_spawn(program, "--exit-child\t0", 2, "", 0, "", 0, 0);
    assert(h >= 0);
    HANDLE child = (HANDLE)(intptr_t)plum_process_slots[(h - 1) % PLUM_PROC_MAX_SLOTS].child->pid;
    // The child has really exited, but scripted waits model it still running.
    assert(WaitForSingleObject(child, 30000) == WAIT_OBJECT_0);
    const long long cap = 0x7FFFFFFELL * 1000000LL;
    const long long durations[] = {0, 1, 999999, 1000000, 1000001,
                                  cap - 1, cap, cap + 1, 2 * cap, 2 * cap + 1, LLONG_MAX};
    for (size_t i = 0; i < sizeof(durations) / sizeof(durations[0]); i++) {
        long long nanos = durations[i];
        long long millis = nanos / 1000000LL + (nanos % 1000000LL != 0);
        int queries = exit_queries;
        arm_wait(nanos, 0, WAIT_TIMEOUT);
        assert(process_child_wait(h, nanos) == 0);
        assert(wait_remaining == 0 && wait_consumed == millis);
        assert(wait_calls == (millis == 0 ? 1 : (millis + 0x7FFFFFFELL - 1) / 0x7FFFFFFELL));
        assert(!plum_process_slots[(h - 1) % PLUM_PROC_MAX_SLOTS].child->finished && exit_queries == queries);
        assert(allocation_attempt == wait_allocation_baseline && live_bytes == wait_live_baseline);
        scripted_wait = 0;
    }
    // A failed later chunk must leave the child available for another wait.
    arm_wait(2 * cap + 1, 2, WAIT_FAILED);
    assert(process_child_wait(h, 2 * cap + 1) == -1 && wait_calls == 2);
    assert(!plum_process_slots[(h - 1) % PLUM_PROC_MAX_SLOTS].child->finished);
    scripted_wait = 0;
    // Signalling during a later chunk collects exactly once, preserving output.
    int queries = exit_queries;
    arm_wait(2 * cap + 1, 2, WAIT_OBJECT_0);
    assert(process_child_wait(h, 2 * cap + 1) == 1 && wait_calls == 2);
    assert(exit_queries == queries + 1 && process_exit_code(h) == 0);
    assert(process_capture_status(h) == 0);
    scripted_wait = 0;
    assert(process_child_wait(h, 0) == 1 && process_child_wait(h, LLONG_MAX) == 1);
    assert(exit_queries == queries + 1);
    process_child_free(h);
    no_scratch();
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
    assert(plum_path_remove(out_path) == 0 && plum_path_remove(err_path) == 0);
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
        char *out = _strdup(plum_process_slots[(h - 1) % PLUM_PROC_MAX_SLOTS].child->out_path);
        char *err = _strdup(plum_process_slots[(h - 1) % PLUM_PROC_MAX_SLOTS].child->err_path);
        assert(plum_path_attributes(out) == INVALID_FILE_ATTRIBUTES);
        assert(plum_path_attributes(err) == INVALID_FILE_ATTRIBUTES);
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
    test_stdin_spawns(program);
    test_handle_inheritance(program);
    test_exit_statuses(program);
    test_wait_timeouts(program);
    assert(DeleteFileW(path));
}

int main(void) {
    int argc;
    wchar_t **argv = CommandLineToArgvW(GetCommandLineW(), &argc);
    assert(argv != NULL);
    if (argc == 3 && wcscmp(argv[1], L"--probe-handle") == 0) {
        HANDLE event = (HANDLE)(uintptr_t)wcstoull(argv[2], NULL, 10);
        int inherited = SetEvent(event);
        LocalFree(argv);
        return inherited ? 9 : 0;
    }
    if (argc == 3 && wcscmp(argv[1], L"--exit-child") == 0) {
        DWORD code = (DWORD)wcstoul(argv[2], NULL, 10);
        LocalFree(argv);
        ExitProcess(code);
    }
    if (argc == 2 && (wcscmp(argv[1], L"--stdin-child") == 0 || wcscmp(argv[1], L"--stdin-empty") == 0)) {
        size_t expected = wcscmp(argv[1], L"--stdin-empty") == 0 ? 0 : 2 * 1024 * 1024 + 17;
        LocalFree(argv);
        unsigned char buffer[4096];
        size_t offset = 0;
        DWORD count;
        while (1) {
            assert(ReadFile(GetStdHandle(STD_INPUT_HANDLE), buffer, sizeof(buffer), &count, NULL));
            if (count == 0) break;
            assert(offset + count <= expected);
            for (DWORD i = 0; i < count; i++) assert(buffer[i] == (offset + i) % 256);
            offset += count;
        }
        assert(offset == expected);
        return 0;
    }
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
    test_stdin_accounting();
    test_builders();
    // Capture creation/readback/removal must work below a Unicode TEMP root.
    wchar_t temp[4096], root[4096], old_temp[4096], old_tmp[4096];
    DWORD nt = GetEnvironmentVariableW(L"TEMP", old_temp, 4096);
    DWORD np = GetEnvironmentVariableW(L"TMP", old_tmp, 4096);
    assert(nt < 4096 && np < 4096 && GetTempPathW(4096, temp));
    assert(swprintf(root, 4096, L"%lsplum-\x65e5\xd83c\xdf0d-%lu", temp, GetCurrentProcessId()) > 0);
    assert(CreateDirectoryW(root, NULL));
    assert(SetEnvironmentVariableW(L"TEMP", root) && SetEnvironmentVariableW(L"TMP", root));
    test_spawns();
    assert(SetEnvironmentVariableW(L"TEMP", nt ? old_temp : NULL));
    assert(SetEnvironmentVariableW(L"TMP", np ? old_tmp : NULL));
    assert(RemoveDirectoryW(root));
    printf("Windows process Unicode, overlays, chunked stdin/waits, full exit status, failure cleanup: ok (peak scratch %zu bytes)\n", peak_bytes);
    return 0;
}
