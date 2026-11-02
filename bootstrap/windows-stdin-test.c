// An isolated native console, with a parent watchdog. Unlike redirected
// stdin fixtures this reaches Windows' cooked ReadFile and its input records.
#undef NDEBUG
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>
#include <windows.h>
#include <stdatomic.h>

static atomic_int fail_read, fail_event, event_calls, fail_thread;
static BOOL WINAPI probe_read(HANDLE input, void *buffer, DWORD count,
                              DWORD *got, OVERLAPPED *overlapped) {
    if (atomic_exchange(&fail_read, 0)) {
        SetLastError(ERROR_ACCESS_DENIED);
        return FALSE;
    }
    return ReadFile(input, buffer, count, got, overlapped);
}
static HANDLE WINAPI probe_event(SECURITY_ATTRIBUTES *attributes, BOOL manual,
                                  BOOL initial, const WCHAR *name) {
    if (atomic_fetch_add(&event_calls, 1) + 1 == atomic_load(&fail_event)) {
        SetLastError(ERROR_NOT_ENOUGH_MEMORY);
        return NULL;
    }
    return CreateEventW(attributes, manual, initial, name);
}
static HANDLE WINAPI probe_thread(SECURITY_ATTRIBUTES *attributes, SIZE_T stack,
                                   LPTHREAD_START_ROUTINE start, void *arg,
                                   DWORD flags, DWORD *id) {
    if (atomic_load(&fail_thread)) { SetLastError(ERROR_NOT_ENOUGH_MEMORY); return NULL; }
    return CreateThread(attributes, stack, start, arg, flags, id);
}
#define ReadFile probe_read
#define CreateEventW probe_event
#define CreateThread probe_thread
#include "../native_stdlib/io_shim.c"
#undef ReadFile
#undef CreateEventW
#undef CreateThread

static DWORD handles(void) {
    DWORD count;
    assert(GetProcessHandleCount(GetCurrentProcess(), &count));
    return count;
}
static void key(HANDLE input, WCHAR ch, WORD virtual_key, BOOL down) {
    INPUT_RECORD record = {0};
    record.EventType = KEY_EVENT;
    record.Event.KeyEvent.bKeyDown = down;
    record.Event.KeyEvent.wRepeatCount = 1;
    record.Event.KeyEvent.wVirtualKeyCode = virtual_key;
    record.Event.KeyEvent.uChar.UnicodeChar = ch;
    DWORD written;
    assert(WriteConsoleInputW(input, &record, 1, &written) && written == 1);
}
static void text(HANDLE input, const WCHAR *s) {
    while (*s) { key(input, *s, *s == L'\r' ? VK_RETURN : 0, TRUE); s++; }
}
static void expect(long long result, const char *expected) {
    assert(result > 0);
    assert(stdin_result_len(result) == (long long)strlen(expected));
    assert(memcmp(stdin_result_data(result), expected, strlen(expected)) == 0);
    stdin_result_release(result);
}
static void timed_out(long long timeout) {
    long long start = plum_now_ns();
    assert(stdin_read_owned(0, timeout, 1) == PLUM_STDIN_TIMED_OUT);
    assert(plum_now_ns() - start < 2000000000LL);
}
static void console_tests(void) {
    HANDLE input = CreateFileW(L"CONIN$", GENERIC_READ | GENERIC_WRITE,
                               FILE_SHARE_READ | FILE_SHARE_WRITE, NULL,
                               OPEN_EXISTING, 0, NULL);
    assert(input != INVALID_HANDLE_VALUE);
    assert(SetStdHandle(STD_INPUT_HANDLE, input));
    assert(SetConsoleCP(CP_UTF8));
    DWORD saved;
    assert(GetConsoleMode(input, &saved));
    DWORD cooked = ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT |
                   ENABLE_EXTENDED_FLAGS | ENABLE_WINDOW_INPUT | ENABLE_MOUSE_INPUT;
    assert(SetConsoleMode(input, cooked));
    assert(FlushConsoleInputBuffer(input));

    // Startup failures release all partially created handles.
    DWORD baseline_handles = handles();
    for (int fail = 1; fail <= 3; fail++) {
        atomic_store(&event_calls, 0);
        atomic_store(&fail_event, fail < 3 ? fail : 0);
        atomic_store(&fail_thread, fail == 3);
        assert(stdin_read_owned(0, 20000000, 1) == PLUM_STDIN_ERROR);
        assert(handles() == baseline_handles);
    }
    atomic_store(&fail_event, 0);
    atomic_store(&fail_thread, 0);

    // A wake caused entirely by non-character records must still expire.
    INPUT_RECORD noise[3] = {0};
    noise[0].EventType = MOUSE_EVENT;
    noise[1].EventType = WINDOW_BUFFER_SIZE_EVENT;
    noise[2].EventType = KEY_EVENT;
    noise[2].Event.KeyEvent.bKeyDown = FALSE;
    noise[2].Event.KeyEvent.wRepeatCount = 1;
    noise[2].Event.KeyEvent.uChar.UnicodeChar = L'x';
    DWORD written;
    assert(WriteConsoleInputW(input, noise, 3, &written) && written == 3);
    timed_out(20000000);
    HANDLE worker = plum_stdin.console.thread;
    assert(worker && plum_stdin.console.pending);
    size_t capacity = plum_stdin.cap;
    DWORD active_handles = handles();

    // The original bug: a character arrives, but no Enter follows it.
    // Repeated timeouts must keep one worker, one request and fixed storage.
    text(input, L"par");
    for (int i = 0; i < 50; i++) {
        timed_out(1000000);
        assert(plum_stdin.console.thread == worker);
        assert(plum_stdin.console.pending && plum_stdin.cap == capacity);
        assert(handles() == active_handles);
    }
    text(input, L"tial\r");
    expect(stdin_read_owned(0, 2000000000LL, 1), "partial");
    text(input, L"\r");
    expect(stdin_read_owned(0, 2000000000LL, 1), "");

    // Preserve Windows editing, echo mode and UTF-8 conversion rather than
    // implementing an incomplete line editor or INPUT_RECORD decoder.
    text(input, L"ab\bc\r");
    expect(stdin_read_owned(0, 2000000000LL, 1), "ac");
    text(input, L"\u00e9\u4e2d\r");
    expect(stdin_read_owned(0, 2000000000LL, 1), "\xc3\xa9\xe4\xb8\xad");
    DWORD after;
    assert(GetConsoleMode(input, &after) && after == cooked);

    atomic_store(&fail_read, 1);
    assert(stdin_read_owned(0, 2000000000LL, 1) == PLUM_STDIN_ERROR);
    assert(!plum_stdin.eof);
    text(input, L"after error\r");
    expect(stdin_read_owned(0, 2000000000LL, 1), "after error");

    // Raw-mode keys still use Windows' VT stream translation.
    assert(SetConsoleMode(input, ENABLE_VIRTUAL_TERMINAL_INPUT));
    key(input, 0, VK_UP, TRUE);
    long long raw = stdin_read_owned(4096, 2000000000LL, 0);
    // A stream is allowed to return a short prefix of an escape sequence.
    char arrow[4] = {0};
    size_t used = 0;
    while (used < 3) {
        assert(raw > 0);
        size_t n = (size_t)stdin_result_len(raw);
        assert(n > 0 && n <= 3 - used);
        memcpy(arrow + used, stdin_result_data(raw), n);
        used += n;
        stdin_result_release(raw);
        if (used < 3) raw = stdin_read_owned(4096, 2000000000LL, 0);
    }
    assert(strcmp(arrow, "\033[A") == 0);
    key(input, 3, 'C', TRUE);
    expect(stdin_read_owned(4096, 2000000000LL, 0), "\003");

    assert(plum_console_wait_ms(-1) == INFINITE);
    assert(plum_console_wait_ms(LLONG_MAX) == 0xfffffffeUL);

    // Shutdown must cancel and join an unfinished cooked read without
    // freeing memory the worker can still access, including the start race.
    for (int i = 0; i < 20; i++) {
        assert(SetConsoleMode(input, cooked));
        timed_out(1000000);
        plum_console_close(&plum_stdin.console);
        assert(handles() == baseline_handles);
    }
    assert(SetConsoleMode(input, saved));
    free(plum_stdin.bytes);
    assert(CloseHandle(input));
}
int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "--console") == 0) {
        console_tests();
        return 0;
    }
    WCHAR program[32768], command[32768];
    DWORD length = GetModuleFileNameW(NULL, program, 32768);
    assert(length && length < 32768);
    assert(swprintf(command, 32768, L"\"%ls\" --console", program) > 0);
    STARTUPINFOW startup = {0};
    startup.cb = sizeof(startup);
    startup.dwFlags = STARTF_USESTDHANDLES;
    startup.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
    startup.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
    startup.hStdError = GetStdHandle(STD_ERROR_HANDLE);
    PROCESS_INFORMATION child = {0};
    assert(CreateProcessW(program, command, NULL, NULL, TRUE, CREATE_NEW_CONSOLE,
                          NULL, NULL, &startup, &child));
    DWORD wait = WaitForSingleObject(child.hProcess, 30000);
    if (wait != WAIT_OBJECT_0) {
        TerminateProcess(child.hProcess, 99);
        fputs("FAIL: Windows console stdin exceeded its watchdog deadline\n", stderr);
    }
    DWORD code;
    assert(GetExitCodeProcess(child.hProcess, &code));
    CloseHandle(child.hThread);
    CloseHandle(child.hProcess);
    assert(wait == WAIT_OBJECT_0 && code == 0);
    puts("Windows console stdin: deadlines, partial lines, editing, VT keys, bounded worker, cleanup: ok");
}
