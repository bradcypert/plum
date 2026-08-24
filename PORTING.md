# Porting Plum to other platforms

Plum began as Linux x86_64 only. This is the record of what that
actually meant, what has been fixed, and what is left — written from
measurements of this tree rather than from expectations, because the
first attempt at guessing (see `MAINTENANCE.md`, "hand-kept gap lists
were wrong three times") was wrong three times.

## Support tiers

The rule the release workflow already enforced, now stated as a
promise: **a platform is published only once something in CI builds and
runs real programs on it.** Nothing ships that is merely expected to
work.

| Tier | Meaning | Platforms |
|---|---|---|
| 1 | Full harness suite green in CI. Binaries published. | Linux x86_64 |
| 2 | `bootstrap/platform-smoke` green in CI. Binaries published. | macOS arm64, macOS x86_64 |
| 3 | Expected to work. Untested, unpublished, no promise. | Linux arm64 |
| — | Known not to work. | Windows |

Tier 2 is a real step down from tier 1 and the difference is worth
knowing: tier 1 runs 61 fixtures under AddressSanitizer with
`detect_leaks=1`, and Plum is refcounted, so a leak there is a
*miscompile*, not untidiness. LeakSanitizer does not exist on Darwin at
all. Tier 2 therefore establishes "these programs build and print the
right answer" and not "the refcounting is correct". Correctness is
established on Linux and assumed to carry.

## What is actually platform-specific

The generated code depends on 48 external symbols:

- **31 libc.** 29 are plain standard C, present everywhere.
- **14 supplied by our own C shims** in `native_stdlib/`.
- **3 LLVM intrinsics** (the checked-arithmetic ones), target-neutral.

The compiler **emits no LLVM target triple**, so `clang` targets
whatever host it runs on. That was already true and is the single
biggest reason this port is small rather than large.

### The two libc symbols that are not portable

| Symbol | Linux | macOS | Windows |
|---|---|---|---|
| `malloc_usable_size` | glibc | `malloc_size` | `_msize` |
| `dprintf` | POSIX | POSIX | absent |

Both are now filled in `native_stdlib/compat_shim.c`, which is the only
file in the project that knows a target's name.

**Why fills rather than renaming the call in the runtime.** There is a
bootstrap cycle. `bootstrap/seed/plum.ll` is a checked-in compiler
shipped as IR, and that IR already contains the glibc name; on a Mac
the seed would fail to *link*, so there would be no compiler with which
to build the compiler that stops emitting the name. Defining the
missing symbol breaks the cycle with no seed regeneration. Renaming
remains available later as ordinary cleanup.

### The six shims

| Shim | macOS | Windows |
|---|---|---|
| `compat_shim.c` | ✅ | ✅ |
| `io_shim.c` | ✅ stdio only | ✅ stdio only |
| `thread_shim.c` | ✅ pthread | MinGW winpthreads, or a Win32 rewrite |
| `dir_shim.c` | ✅ dirent | MinGW `dirent.h`, or `FindFirstFile` |
| `process_shim.c` | ✅ fork/waitpid | **rewrite** — no `fork` even under MinGW |
| `net_shim.c` | ✅ BSD sockets | **rewrite** — Winsock needs `WSAStartup`, `SOCKET`, `closesocket` |

### Unix commands the compiler shells out to

Found by `grep -rn 'run_process("' bootstrap/self_host/`. These are not
in the shims; they are in the compiler's own Plum source, and no shim
rewrite covers them.

| Call | Sites | macOS | Windows |
|---|---|---|---|
| `clang` | 2 | ✅ intended, stays | ✅ |
| `mktemp -d` | 5 | ✅ | ✗ |
| `rm -rf` / `rm -f` | 6 | ✅ | ✗ |
| `cp -r` | 1 | ✅ | ✗ |
| `mkdir` | 1 | ✅ | ✗ |
| `/proc/self/exe` | 3 | ✗ | ✗ |

`/proc/self/exe` is the one that matters before Windows does. It is
Linux-only, used by the **language server** to re-invoke itself for
`check`, `query` and `defs`. So on macOS today, `build`, `run` and
`test` work and **the LSP does not**. macOS needs `_NSGetExecutablePath`;
Windows needs `GetModuleFileName`.

## Done

- `native_stdlib/compat_shim.c`, wired into `bootstrap/gen-shims` so it
  is embedded in the compiler and written into every user build.
- All six harness scripts that hardcoded the shim list updated. That
  list was duplicated in six places and is a standing drift trap.
- **The seed regenerated**, which was mandatory rather than tidiness:
  the seed embeds the shim sources it writes out when *it* builds
  something, so a seed without `compat_shim.c` bootstraps a compiler
  that cannot link on a Mac.
- `bootstrap/check-seed` now asserts the seed *carries* every shim in
  `native_stdlib/`. It previously compared only what the seed
  *produces*, which is invisible to this class of bug on Linux — every
  harness passed while the seed was, in fact, unusable on macOS.
- `bootstrap/platform-smoke`: POSIX `sh`, no ASan, no GNU `timeout`, no
  `./sh` wrapper. Builds and runs all 42 execution fixtures through
  `plum build` — the path a user takes.
- `bootstrap/package-release`: packaging plus unpack-and-use
  verification, extracted from inline YAML because `sha256sum` is
  GNU-only.
- `ci.yml` gained a `platforms` matrix (macos-15 arm64, macos-13 x86_64).
- `release.yml` split into `linux` / `macos` / `publish`.

**None of the macOS work is verified yet.** It is verified when the CI
legs above go green, and not before. Expect the first run to find
something — glibc and Apple's libc differ most in `snprintf` and
float formatting, which is exactly what the fixtures compare.

## Left to do

### macOS: the language server

Replace `/proc/self/exe`. Two options:

- **A shim that returns the executable's path** (`_NSGetExecutablePath`
  on Darwin, `readlink("/proc/self/exe")` on Linux). Small, matches the
  existing shim pattern, keeps the subprocess isolation the LSP gets
  today.
- **Stop spawning a subprocess at all** and call the check/query/defs
  functions in-process. Better engineering — faster, no self-path
  problem anywhere — but a real refactor.

Either way this needs a **two-generation** bootstrap (a new prelude
function cannot be called by `main.plum` until the generation after it
is added) and therefore two seed refreshes. See `MAINTENANCE.md`.

### Windows

The toolchain decision comes first and determines everything after it.

- **MinGW-w64 via MSYS2** — recommended. `dirent`, pthreads and bash all
  keep working, so the work shrinks to `process_shim.c`, `net_shim.c`,
  the self-path, and the `mktemp`/`rm`/`cp`/`mkdir` shell-outs.
- **clang-cl / MSVC** — properly native, considerably more work: all
  four shims rewritten against Win32 plus a harness story.
- **WSL only** — document Windows as supported through WSL and stop.
  Zero work, and defensible for a 0.0.x language.

Under the MinGW route, in order:

1. Add an OS shim providing temp-directory creation, recursive delete,
   file delete, directory create, tree copy, and executable path.
   Replace all 16 shell-out sites. **Testable on Linux**, and an
   improvement there too — fewer processes per build.
2. Rewrite `process_shim.c` without `fork` (`CreateProcess`, or
   `_spawnvp` keeping the existing temp-file capture).
3. Rewrite `net_shim.c` against Winsock.
4. Add a `windows-latest` CI leg running `platform-smoke` under MSYS2.
5. Add the release matrix entry once that is green.

Step 1 is the one to do first regardless of the toolchain choice: it is
required by every route except WSL-only, it benefits Linux and macOS,
and it can be verified here.

### Linux arm64

Nearly free once macOS arm64 is green, since that proves the compiler
produces correct code for the architecture. Mostly a runner change.
