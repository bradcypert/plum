// args() copies each borrowed native argument into a Plum cell. On Windows,
// parse the Unicode command line and keep the vector/conversion scratch scoped
// to that one args() call. POSIX argv remains borrowed, including non-UTF-8.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#if defined(_WIN32)
#include "path_shim.h"

typedef struct { int count; wchar_t **wide; char *text; } PlumArgs;
static void plum_args_failure(void) {
    fputs("plum: cannot read Windows command-line arguments (invalid Unicode or allocation failure)\n", stderr);
    exit(1);
}
#endif

void *plum_args_native_open(void) {
#if defined(_WIN32)
    // Use the documented Windows parser without adding a static Shell32
    // dependency to every seed/harness link. The returned LocalAlloc block
    // is independent of the module and is released with LocalFree.
    HMODULE module = LoadLibraryExW(L"shell32.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32);
    typedef wchar_t **(WINAPI *ParseArgs)(LPCWSTR, int *);
    ParseArgs parse = NULL;
    FARPROC symbol = module ? GetProcAddress(module, "CommandLineToArgvW") : NULL;
    memcpy(&parse, &symbol, sizeof(parse));
    int count = 0;
    wchar_t **wide = parse ? parse(GetCommandLineW(), &count) : NULL;
    if (module) FreeLibrary(module);
    if (!wide) { plum_args_failure(); return NULL; }
    PlumArgs *args = malloc(sizeof(*args));
    if (!args) { LocalFree(wide); plum_args_failure(); return NULL; }
    args->count = count; args->wide = wide; args->text = NULL;
    return args;
#else
    return NULL;
#endif
}

long long plum_args_native_count(void *context, long long fallback) {
#if defined(_WIN32)
    (void)fallback;
    return ((PlumArgs *)context)->count;
#else
    (void)context;
    return fallback;
#endif
}

void plum_args_native_drop(void *context) {
#if defined(_WIN32)
    PlumArgs *args = context;
    free(args->text); args->text = NULL;
#else
    (void)context;
#endif
}
void plum_args_native_close(void *context) {
#if defined(_WIN32)
    PlumArgs *args = context;
    if (args) { plum_args_native_drop(args); LocalFree(args->wide); free(args); }
#else
    (void)context;
#endif
}

const char *plum_args_native_data(void *context, long long index, char **fallback) {
#if defined(_WIN32)
    (void)fallback;
    PlumArgs *args = context;
    plum_args_native_drop(args);
    int n = index >= 0 && index < args->count ? WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS,
        args->wide[index], -1, NULL, 0, NULL, NULL) : 0;
    if (n > 0) {
        args->text = malloc((size_t)n);
        if (args->text && plum_path_utf8(args->wide[index], args->text, (size_t)n)) return args->text;
    }
    plum_args_native_close(args);
    plum_args_failure(); return NULL;
#else
    (void)context;
    return fallback[index];
#endif
}
