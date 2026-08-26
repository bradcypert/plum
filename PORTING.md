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
| 2 | `bootstrap/platform-smoke` green in CI. Binaries published. | macOS arm64, macOS x86_64, Windows x86_64 |
| 3 | Expected to work. Untested, unpublished, no promise. | Linux arm64 |

macOS arm64 and Windows x86_64 run on every push. macOS x86_64 runs
only on a release tag: Intel Mac runners are scarce enough that the job
queued for hours, and a straggler blocks log downloads for the whole
run. Publishing an Intel binary still requires it to pass.

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
| `os_shim.c` | ✅ | ✅ Win32 branches |
| `thread_shim.c` | ✅ pthread | ✅ MinGW winpthreads, `-lpthread` |
| `dir_shim.c` | ✅ dirent | ✅ MinGW's `dirent.h`, unchanged |
| `process_shim.c` | ✅ fork/waitpid | ✅ `CreateProcess` |
| `net_shim.c` | ✅ BSD sockets | ✅ Winsock, `-lws2_32` |

Every ✅ above is exercised by a CI leg that builds and runs every
execution fixture on that platform. `dir_shim.c` and `thread_shim.c` needed no
Windows code at all — MinGW-w64 supplies `dirent.h` and pthreads, which
is why neither is on the list of things that had to be written.

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
  `./sh` wrapper. Builds and runs every execution fixture through
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
bootstraps a compiler, that compiler builds a compiler, and every
execution fixture builds and prints the right answer on Apple Silicon. It took two
runs — the first found the locale bug described above.

The guess about what would break was wrong, which is worth recording.
Float formatting was expected to differ between glibc and Apple's libc
and did not; every float fixture passed first time. The failure was
character case mapping, from a hardcoded libc constant. Predicting
which part of a port breaks is not something this project has been
good at.

## Left to do

### Windows — done

Windows x86_64 went green on **2026-08-25**: 44 of 44 programs build
and run under MSYS2/MinGW, and the `continue-on-error` marker came off
the CI leg in the same commit. The language server followed the same
day, once `lsp-smoke` could run there — and turned up a bug on its
first attempt.

**`uri_to_path` stripped a fixed seven characters from `file://`.**
Right on Unix, where the path component's leading `/` is the root; and
wrong on Windows, where a client sends `file:///C:/x/a.plum` and gets
back `/C:/x/a.plum`, which opens nothing. `path_to_uri` had the mirror
bug, producing `file://C:/x` and making `C:` the URI's AUTHORITY. Any
real Windows editor would have hit this. Nothing but the Windows CI leg
exercises that path, so nothing but the Windows CI leg would have found
it.

The harness had its own version of the same confusion: it handed the
compiler MSYS paths. Under MSYS2 an `/tmp/...` path is translated when
passed as an ARGUMENT and not when it is buried inside a JSON string,
which is where an LSP session puts it — so `platform-smoke` was never
affected while `lsp-smoke` could not open a single file. Every path in
that harness is now converted with `cygpath -m` up front.

The toolchain reasoning, kept because it is the decision everything
else followed from:

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
   MSYS2.~~ **Done.**
4. ~~Get that leg green.~~ **Done, 2026-08-25 — 43 of 43.** It took
   three rounds, and every one was worth more than the analysis that
   would have replaced it: a second `fork` site nobody had read, a
   `sys/socket.h` left outside a guard, and CRLF output hidden behind
   a CRLF checkout.
5. ~~Rewrite `net_shim.c` against Winsock.~~ **Done.**

   This was previously described here as "not on the critical path".
   **That was wrong.** `write_shims` writes *every* embedded shim into
   each build and hands them all to `clang`, so `net_shim.c` is
   compiled into every `plum build` whether the program opens a socket
   or not. Nothing would have built on Windows until it compiled.
6. ~~Add the release matrix entry.~~ **Done** — `release.yml` builds
   and publishes `plum-<version>-x86_64-windows.tar.gz`.

### What was verified before Windows CI could run it

All of the below was checked on Linux while the Windows leg was still
red. It is recorded because it is the part that made three CI rounds
enough instead of ten:

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

### Linux arm64 — CI added 2026-08-26, not yet published

A `linux-arm64` job runs on `ubuntu-24.04-arm`. It is deliberately the
HEAVIEST of the non-reference legs: bootstrap-check, the full corpus
under AddressSanitizer with `detect_leaks=1`, platform-smoke,
lsp-smoke and the properties.

That is the opposite of how macOS and Windows are treated, and for a
reason. Those platforms cannot run leak checking at all — LeakSanitizer
does not exist on Darwin. This one is Linux, so it can. A new
ARCHITECTURE is exactly where a refcounting or alignment miscompile
would appear, and a leak in a refcounted language is a miscompile
rather than untidiness. Checking only that programs print the right
bytes would miss the class of bug most worth looking for here.

`bootstrap/cross-check` also compile-checks `aarch64-linux-gnu` from a
Linux x86_64 box, which is free and catches the shim-portability class
without waiting for CI.

**Nothing is published for it yet.** The rule stands: a platform earns
a release job by being green in CI first. When that leg has passed, add
it to `release.yml` and move it up the tier table.

### Linux arm64 — the original note

Nearly free once macOS arm64 is green, since that proves the compiler
produces correct code for the architecture. Mostly a runner change.

## Cross-compiling from Linux

`zig cc` cross-compiles every shim, and links the whole compiler, for
macOS arm64, macOS x86_64 and Windows x86_64 — from a Linux box, with
no Xcode SDK and no Windows toolchain, in **about two seconds**. Zig
vendors the libc headers and link stubs for all three.
`bootstrap/cross-check` does exactly this and skips cleanly where `zig`
is absent.

**It does not replace a platform CI leg, and must not be treated as
one.** It proves that code compiles and links. Nothing it produces is
ever executed, so it says nothing about behaviour — not the locale bug,
not the exponent padding, not whether `CreateProcess` actually starts
`clang`. Every bug this port has hit, except the compile errors, would
have sailed straight through it. The tier rule stands: a platform is
published only once something in CI *runs* real programs on it.

Nor should release artifacts be built this way, though they could be.
A cross-linked binary is one no machine has ever executed, produced by
a different toolchain than the one the tests ran under. The current
arrangement builds each platform's binary on that platform, right after
the harness passed there, which is the property worth keeping.

What it *is* worth: closing the compile-error feedback loop from a CI
round trip down to a second. Three of this port's failures were compile
errors of one shape — a POSIX header or call left outside a platform
guard, invisible on Linux where the guard is inert.

### Cost, and what the real cost was

GitHub Actions is free for public repositories, this one included, on
every runner size. There was no bill to reduce.

The real cost was **latency, and only on Intel macOS**: `macos-15`
(arm64) finishes in under a minute, while `macos-13` sat queued for
hours. Worse than being slow, a straggling job keeps the whole run
marked in-progress, and GitHub will not serve *any* job's logs until
the run completes — so one runner blocked the diagnosis of every other
leg.

**And it was never going to arrive.** The job eventually ended at
`24h0m1s` — GitHub's job timeout, not a runner. `macos-13` had been
**retired**: `actions/runner-images` publishes only `macos-15` and
`macos-26`, each with an x86_64 and an arm64 variant. A `runs-on`
naming an image that no longer exists does not fail fast; it waits a
full day and then dies.

Two changes came out of that. Intel macOS is now `macos-15-intel` — the
x86_64 image of a current OS, which does exist — and **every job in
both workflows sets `timeout-minutes`**, so a runner that never arrives
costs minutes rather than a day. Intel macOS remains release-only:
publishing an Intel binary requires it to pass, and a tag is a place
where waiting is acceptable while a push is not.

## The bug that was hidden by another bug

Windows output was CRLF all along. Microsoft's CRT opens `stdout` in
text mode, so every `\n` a Plum program wrote became `\r\n`.

It went unnoticed because a second problem cancelled it out. Git for
Windows defaults to `core.autocrlf=true`, so the checked-in
`expected.txt` recordings were *also* checked out as CRLF — and two
wrong things compared equal. 40 of 43 fixtures passed for the wrong
reason.

Adding `.gitattributes` with `eol=lf` fixed the checkout and removed
the cancellation, and 38 fixtures failed at once. The apparent
regression was the port getting *more* honest, not less.

Two things made it unusually hard to see:

- **MSYS2's bash strips a trailing `\r\n`** in command substitution. So
  the last line of any captured output came back clean while every
  earlier line kept its `\r`. One-line fixtures passed; multi-line ones
  failed with the last line matching and everything above it differing.
- **No character-level view showed it.** A diff printed `< 800` against
  `> 800`. A pass that rendered carriage returns as `<CR>` printed
  nothing, because it ran on the runner where the sed saw... something
  it did not match. Only `od -c` in `bootstrap/platform-smoke` made it
  visible, and that dump is now permanent.

The fix is `plum_set_binary_stdio` in `compat_shim.c`, called at
startup. It is right independently of this bug: Plum emits UTF-8 bytes,
its corpus compares output byte for byte, the same program must print
the same bytes everywhere, and `plum emit-llvm` writes IR to stdout —
which text mode was corrupting too.

**The lesson worth keeping:** a green test can mean two errors that
cancel. This one survived a full CI run looking healthy.

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
