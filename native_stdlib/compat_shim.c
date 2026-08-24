// Platform compatibility fills: C functions the generated runtime IR
// calls that do NOT exist under the same name on every target.
//
// The generated code depends on 47 C symbols. 45 of them are plain
// standard C and are present everywhere. This file exists for the two
// that are not, and it is deliberately the ONLY place that knows a
// target's name -- the runtime IR (`codegen/runtime.plum`) stays
// target-neutral, and the compiler emits no LLVM target triple at all,
// so `clang` targets whatever host it is running on.
//
// **Why fills rather than renames.** The obvious fix is to rename the
// call in the runtime to something neutral, e.g. `@plum_usable_size`.
// That cannot be the FIRST move, because of a bootstrap cycle:
// `bootstrap/seed/plum.ll` is a checked-in compiler shipped as IR, and
// that IR already contains the glibc name. On a Mac the seed would
// fail to LINK, so there would be no compiler with which to build the
// compiler that stops emitting the name. Defining the missing symbol
// here breaks the cycle with no seed regeneration: the existing seed
// links unchanged, on every platform, today.
//
// A rename remains available later as ordinary cleanup, on a normal
// seed-refresh cycle. It is not urgent, and this file is not a
// placeholder for it -- `dprintf` below is a genuine fill either way.
//
// On Linux this file is an empty translation unit apart from the
// placeholder below, which exists only because an entirely empty one
// is not strictly conforming C.

// Keeps this a well-formed translation unit on platforms that need
// none of the fills. Never called.
int plum_compat_placeholder(void);
int plum_compat_placeholder(void) { return 0; }

// Which platform this binary is running on, as a lowercase word.
//
// Deliberately NOT named `plum_platform`: a Plum function called
// `platform` mangles to `@plum_platform`, and a collision between a
// runtime symbol and a mangled Plum name is a mistake this project has
// already made once (`@plum_print` against `print`).
//
// It exists because the compiler builds a `clang` command line in Plum
// code, where a C `#ifdef` cannot reach -- and the libraries a link
// needs are not the same everywhere. Returns a string literal with
// static storage; the caller copies it immediately.
const char *plum_host_platform(void);
const char *plum_host_platform(void) {
#if defined(_WIN32)
    return "windows";
#elif defined(__APPLE__)
    return "macos";
#else
    return "linux";
#endif
}

// Puts the process into a UTF-8 locale, so `towupper`/`towlower` map
// non-ASCII codepoints instead of passing them through.
//
// This was `setlocale(6, "C.utf8")` emitted directly as IR until
// 2026-08-24, and it was wrong on macOS twice over -- silently, which
// is the worst way for a locale call to be wrong:
//
//   1. **`LC_ALL` is not 6 everywhere.** It is 6 on glibc and 0 on
//      BSD/Darwin, where 6 is `LC_MESSAGES`. The call therefore set the
//      wrong category and left `LC_CTYPE` alone. Emitting the constant
//      as a literal in IR is what made this possible; here the C
//      header supplies it, so it is right by construction on each
//      platform.
//   2. **`C.utf8` is a glibc locale name.** macOS does not have it, so
//      even with the right category the call would have failed and
//      changed nothing.
//
// The fallbacks matter for the same reason: there is no single locale
// name that exists everywhere. Each branch tries the spellings its
// platform actually ships, most specific first. If every one fails the
// process keeps the "C" locale and ASCII case mapping still works --
// degraded, not broken.
#include <locale.h>

void plum_set_utf8_locale(void);
void plum_set_utf8_locale(void) {
#if defined(_WIN32)
    // UCRT accepts the codepage-only form, which asks for UTF-8
    // without naming a language.
    if (setlocale(LC_ALL, ".UTF-8") == NULL) setlocale(LC_ALL, "en-US.UTF-8");
#elif defined(__APPLE__)
    if (setlocale(LC_ALL, "C.UTF-8") == NULL)
        if (setlocale(LC_ALL, "en_US.UTF-8") == NULL)
            setlocale(LC_ALL, "UTF-8");
#else
    if (setlocale(LC_ALL, "C.utf8") == NULL)
        if (setlocale(LC_ALL, "C.UTF-8") == NULL)
            setlocale(LC_ALL, "en_US.UTF-8");
#endif
}

#if defined(__APPLE__)

// `malloc_usable_size` is a glibc extension. Apple's libc spells the
// same operation `malloc_size`, in a different header. The runtime
// uses it to DERIVE an allocation's capacity rather than storing it
// (see `runtime.plum`'s array-growth comment), so the value must be
// the real usable size, not a guess -- which is exactly what
// `malloc_size` returns.
#include <malloc/malloc.h>
#include <stddef.h>

size_t malloc_usable_size(void *p);
size_t malloc_usable_size(void *p) { return malloc_size(p); }

#elif defined(_WIN32)

// The MSVC and UCRT runtimes spell it `_msize`.
#include <malloc.h>
#include <stddef.h>
#include <stdarg.h>
#include <stdio.h>
#include <io.h>

size_t malloc_usable_size(void *p);
size_t malloc_usable_size(void *p) { return _msize(p); }

// `dprintf` is POSIX, absent from the Windows CRTs. The runtime calls
// it in exactly one place -- the allocation-statistics dump, always to
// fd 2 -- so routing it through `stderr` is faithful rather than an
// approximation. `vfprintf` is used instead of a `write` to a raw fd
// so the formatting is done by the CRT, as the caller expects.
int dprintf(int fd, const char *fmt, ...);
int dprintf(int fd, const char *fmt, ...) {
    va_list ap;
    int n;
    FILE *out = (fd == 2) ? stderr : stdout;
    va_start(ap, fmt);
    n = vfprintf(out, fmt, ap);
    va_end(ap);
    return n;
}

#endif
