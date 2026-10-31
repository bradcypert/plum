// Run the real Windows parser/converter with scoped allocation accounting.
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>
#include <windows.h>
#include <dirent.h>
#include <sys/stat.h>

static int allocations, fail_at, live, modules, vectors_freed;
static void *test_malloc(size_t n) {
    if (++allocations == fail_at) return NULL;
    void *p = malloc(n); if (p) live++; return p;
}
static void test_free(void *p) { if (p) { assert(live > 0); live--; } free(p); }
static HMODULE WINAPI test_load(LPCWSTR name, HANDLE file, DWORD flags) {
    HMODULE module = LoadLibraryExW(name, file, flags);
    if (module) modules++;
    return module;
}
static BOOL WINAPI test_unload(HMODULE module) { modules--; return FreeLibrary(module); }
static HLOCAL WINAPI test_local_free(HLOCAL block) {
    HLOCAL result = LocalFree(block);
    assert(result == NULL); vectors_freed++; return result;
}
static void test_exit(int code) {
    assert(code == 1 && live == 0 && modules == 0 && vectors_freed == 1);
    exit(code);
}
#define malloc test_malloc
#define free test_free
#define LoadLibraryExW test_load
#define FreeLibrary test_unload
#define LocalFree test_local_free
#define exit test_exit
#include "../native_stdlib/argv_shim.c"
#undef malloc
#undef free
#undef LoadLibraryExW
#undef FreeLibrary
#undef LocalFree
#undef exit

int main(int argc, char **argv) {
    if (argc > 1) {
        if (!strcmp(argv[1], "--oom-open")) fail_at = 1;
        void *context = plum_args_native_open();
        if (!strcmp(argv[1], "--oom-data")) fail_at = allocations + 1;
        if (strcmp(argv[1], "--child")) {
            plum_args_native_data(context, 2, argv);
            assert(!"malformed input/allocation failure must terminate clearly");
        }
        plum_args_native_close(context);
        vectors_freed = 0;
        const char *want[] = {"--child", "\xe6\x97\xa5\xf0\x9f\x8c\x8d", "", "a b", "a\tb"};
        for (int round = 0; round < 256; round++) {
            context = plum_args_native_open();
            assert(plum_args_native_count(context, argc) == 6 && live == 1 && modules == 0);
            for (int i = 1; i < 6; i++) {
                const char *text = plum_args_native_data(context, i, argv);
                assert(!strcmp(text, want[i - 1]) && live == 2);
                plum_args_native_drop(context);
                assert(live == 1);
            }
            plum_args_native_close(context);
            assert(live == 0 && modules == 0 && vectors_freed == round + 1);
        }
        return 0;
    }
    wchar_t self[4096], command[8192];
    assert(GetModuleFileNameW(NULL, self, 4096));
    const wchar_t *cases[] = {
        L"--child \"\x65e5\xd83c\xdf0d\" \"\" \"a b\" \"a\tb\"",
        L"--oom-open \"\x65e5\xd83c\xdf0d\"",
        L"--oom-data \"\x65e5\xd83c\xdf0d\"",
        L"--invalid \"\xd800\""
    };
    for (int i = 0; i < 4; i++) {
        assert(swprintf(command, 8192, L"\"%ls\" %ls", self, cases[i]) > 0);
        STARTUPINFOW startup = {0}; startup.cb = sizeof(startup);
        PROCESS_INFORMATION process = {0};
        assert(CreateProcessW(NULL, command, NULL, NULL, FALSE, 0, NULL, NULL, &startup, &process));
        assert(WaitForSingleObject(process.hProcess, 30000) == WAIT_OBJECT_0);
        DWORD code; assert(GetExitCodeProcess(process.hProcess, &code));
        assert(code == (i == 0 ? 0 : 1));
        CloseHandle(process.hThread); CloseHandle(process.hProcess);
    }
    puts("Windows argv: Unicode, empty arguments, scoped scratch and failure cleanup passed");
}
