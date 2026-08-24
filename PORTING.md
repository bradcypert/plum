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
| 2 | `bootstrap/platform-smoke` green in CI. Binaries published. | macOS arm64 (green), macOS x86_64 (runner queue) |
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

The generated code depends on 54 external symbols:

- **31 libc.** 29 are plain standard C, present everywhere.
- **20 supplied by our own C shims** in `native_stdlib/`.
- **3 LLVM intrinsics** (the checked-arithmetic ones), target-neutral.

The compiler **emits no LLVM target triple**, so `clang` targets
whatever host it runs on. That was already true and is the single
biggest reason this port is small rather than large.

### Constants are part of the ABI too

`setlocale(6, "C.utf8")` was emitted straight into the IR. Both halves
are glibc-specific: `LC_ALL` is 6 on glibc and **0** on macOS, where 6
is `LC_MESSAGES`; and `C.utf8` is a glibc locale name macOS does not
have. The call therefore set the wrong category to a locale that did
not exist, and `towupper`/`towlower` silently stopped mapping anything
outside ASCII.

The lesson generalises: a libc *constant* baked into generated IR is as
much a portability hazard as a libc *symbol*, and a quieter one. A
sweep of every literal the runtime passes to libc found `fseek`'s
`0`/`2` (universal in practice) and `fopen`'s modes (already `"rb"`/
`"wb"`, so Windows will not rewrite newlines) — `setlocale` was the
only real one. Both the category and the locale name now live in
`compat_shim.c`, where the C header supplies them.

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

### The seven shims

| Shim | macOS | Windows |
|---|---|---|
| `compat_shim.c` | ✅ | ✅ |
| `io_shim.c` | ✅ stdio only | ✅ stdio only |
| `os_shim.c` | ✅ | ✅ Win32 branches written, untested |
| `thread_shim.c` | ✅ pthread | MinGW winpthreads, or a Win32 rewrite |
| `dir_shim.c` | ✅ dirent | MinGW `dirent.h`, or `FindFirstFile` |
| `process_shim.c` | ✅ fork/waitpid | ✅ `CreateProcess`, written, untested |
| `net_shim.c` | ✅ BSD sockets | ✅ Winsock, written, untested |

### Unix commands the compiler shelled out to — fixed

These were never in the shims. They were in the compiler's own Plum
source, so no amount of shim rewriting would have covered them.

| Call | Was | Now |
|---|---|---|
| `mktemp -d` | 5 sites | `Os.temp_dir` |
| `rm -rf` | 5 sites | `Os.remove_tree` |
| `rm -f` | 1 site | `Os.remove_file` |
| `cp -r` | 1 site | `Os.copy_tree` |
| `mkdir` | 1 site | `Os.make_dir` |
| `/proc/self/exe` | 3 sites | `Os.self_exe` |
| `clang` | 2 sites | unchanged, and intended |

`/proc/self/exe` was the one that mattered before Windows did. It is
Linux-only and the **language server** used it to re-invoke itself for
`check`, `query` and `defs`, so the LSP could never have worked on a
Mac. `native_stdlib/os_shim.c` now answers that question with
`readlink` on Linux, `_NSGetExecutablePath` on Darwin and
`GetModuleFileNameA` on Windows.

Verified by shadowing `mktemp`, `rm`, `cp` and `mkdir` on `PATH` with
scripts that log and exit 1, then building a project: the old compiler
hit them, the new one builds cleanly with zero hits. The only processes
`plum build` now starts are `clang` — down from four.

`plum test` and the language server still re-invoke the compiler
itself, deliberately: `panic_raw` aborts rather than returning, so a
single-process harness would stop at the first failure, and an
in-process type error would take the language server down.

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
  `./sh` wrapper. Builds and runs all 43 execution fixtures through
  `plum build` — the path a user takes.
- `bootstrap/package-release`: packaging plus unpack-and-use
  verification, extracted from inline YAML because `sha256sum` is
  GNU-only.
- `ci.yml` gained a `platforms` matrix (macos-15 arm64, macos-13 x86_64).
- `release.yml` split into `linux` / `macos` / `publish`.
- `native_stdlib/os_shim.c` and an `Os.` prelude namespace
  (`temp_dir`, `self_exe`, `make_dir`, `remove_file`, `remove_tree`,
  `copy_tree`), replacing all 16 shell-out sites. This needed a
  **two-generation** bootstrap — generation 1 carries the new prelude,
  generation 2 is the first that may call it — and a second seed
  refresh. `Os.remove_tree` uses `lstat`, so a symlink is removed
  rather than followed into; tested against a symlink pointing outside
  the tree.

**macOS arm64 is verified.** The `macos-15` CI leg is green: the seed
bootstraps a compiler, that compiler builds a compiler, and 43 real
programs build and print the right answer on Apple Silicon. It took two
runs — the first found the locale bug described above.

The guess about what would break was wrong, which is worth recording.
Float formatting was expected to differ between glibc and Apple's libc
and did not; every float fixture passed first time. The failure was
character case mapping, from a hardcoded libc constant. Predicting
which part of a port breaks is not something this project has been
good at.

## Left to do

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

1. ~~An OS shim replacing the 16 shell-out sites.~~ **Done.** Its
   Windows branches are written but have never been compiled by a
   Windows toolchain, so treat them as a starting point rather than as
   working code.
2. ~~Rewrite `process_shim.c` without `fork`.~~ **Done**, unverified.
   `CreateProcess` with the temp-file capture kept verbatim, because
   the pipe-deadlock reasoning behind it is not platform-specific.
   `_spawnvp` was considered and rejected: the CRT joins `argv` with
   plain spaces and adds no quoting, so it has the same problem with
   an extra layer over it.
3. ~~Add a `windows-latest` CI leg running `platform-smoke` under
   MSYS2.~~ **Done**, marked `continue-on-error` and expected to fail.
4. **Get that leg green.** This is the next real work, and it is
   deliberately not "write more Windows code" — three shims now have
   `_WIN32` branches that have never been compiled, and writing a
   fourth blind would just add to the pile. The CI leg turns them into
   errors with line numbers.
5. ~~Rewrite `net_shim.c` against Winsock.~~ **Done**, unverified.

   This was previously described here as "not on the critical path".
   **That was wrong.** `write_shims` writes *every* embedded shim into
   each build and hands them all to `clang`, so `net_shim.c` is
   compiled into every `plum build` whether the program opens a socket
   or not. Nothing would have built on Windows until it compiled.
6. Add the release matrix entry once step 4 is green.

### What has been verified about the Windows code

Nothing that needs Windows. What could be checked here, was:

- The **command-line quoting** in `process_shim.c` is the highest-risk
  logic in the port — a Windows path routinely contains a space, and
  getting it wrong silently splits one argument into two. The algorithm
  was extracted and tested on Linux against nine cases, including the
  two that are usually wrong: a trailing backslash before the closing
  quote, and runs of backslashes preceding an embedded quote.
- `os_shim.c` was unit-tested on Linux, including that `remove_tree`
  removes a symlink rather than following it into someone else's files.
- The POSIX path of `process_shim.c` was refactored behind the same
  `plum_spawn_capture` boundary the Windows path implements, and all
  twelve harnesses still pass — so the port did not change Linux
  behaviour, rather than being believed not to.
- `net_shim.c` was restructured so both platforms share one copy of
  every function, differing only in a handle type, a close call, an
  error sentinel and a one-time init. `bootstrap/net-smoke` still opens
  real TCP and HTTP connections on Linux afterwards.

### Linux arm64

Nearly free once macOS arm64 is green, since that proves the compiler
produces correct code for the architecture. Mostly a runner change.

## What the second CI run found

macOS arm64 and Linux both went green. Windows got much further: the
whole shim set compiled and linked under MinGW, and `from-seed`
produced a working compiler binary. It then failed building anything,
on this:

```
net_shim.c:48:10: fatal error: 'sys/socket.h' file not found
```

Self-inflicted, and instructive. The Winsock port moved the socket
headers into a platform guard — but only from `netinet/in.h` down.
`sys/types.h` and `sys/socket.h` sat two lines above the edited region
and stayed outside it. The port looked complete and compiled fine on
Linux, where the guard is inert.

`bootstrap/check-shims` now rejects a non-portable header outside a
platform guard, so this cannot recur quietly. `dirent.h`, `pthread.h`
and `unistd.h` are deliberately not on its list: MinGW-w64 provides all
three, which the CI leg proved by linking three shims that use them.

## What the first CI run found

The Windows leg earned its place immediately, and so did the macOS one.

- **`process_run_inherit` was still forking.** `process_shim.c` has a
  second process function, used by `plum run`, outside any platform
  guard. It was missed because only the first one had been read. Found
  as ten compile errors with line numbers, which is exactly the trade
  the leg exists to make.
- **A sweep for the same bug class** then found `rmdir` unguarded in
  `os_shim.c` — MinGW spells it `_rmdir`, like `mkdir`.
- **macOS failed 2 of 43 fixtures**, both non-ASCII case mapping:
  `"Äöü".to_upper()` returned `Äöü` unchanged. The cause had already
  been found by reading the source — see the locale note above — and
  the failure confirmed it precisely. ASCII was unaffected, which is
  why only two fixtures noticed and why this would have shipped
  silently.
- **Linux failed too, and not because of the port.** `check-version`
  had been failing on every run since 2026-08-21: it fell back to
  `GITHUB_REF_NAME`, which on a push to a branch is the *branch* name,
  so it compared `main` against `0.0.1`. Only `GITHUB_REF` carries the
  ref type.

The macOS bootstrap itself — seed to compiler to compiler — passed on
arm64 on the first attempt.
