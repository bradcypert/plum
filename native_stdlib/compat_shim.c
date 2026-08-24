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
