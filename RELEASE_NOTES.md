Plum is a small, statically typed, compiled language.

A small release, and an honest one about why it exists: 0.0.6 shipped a
language you could not read the clock from.

## `Time.now()`

Seconds since the Unix epoch, from the system clock.

```plum
let main (): Unit = println(Time.now().to_string())
```

Seconds, and an `Int`, because that is what every platform's `time()`
agrees on. Anything richer — a calendar date, a timezone, a formatted
string — is a library on top of this rather than a runtime concern. The
primitive is the part a program cannot write for itself; turning epoch
seconds into `Thu, 27 Aug 2026 01:40:28 GMT` is about forty lines of
ordinary Plum, and belongs in the program that wants it.

## Why it was missing, which is the more useful half

The runtime **declared** `time` and never called it. That is worse than
simply not having the function, because the symbol was taken: an
`extern "C"` block naming `time` was rejected by LLVM as a duplicate
declaration. So the clock was unreachable through the standard library
*and* unreachable through the FFI, and the error pointed at the user's
own code.

This was found by porting a real program — a small ADR management CLI —
from Zig, which is a better test of a language than any amount of
staring at its standard library. Everything else that program needed
existed and composed on the first type-check: `Result` propagation,
`Array.filter`/`sort_string`/`fold`/`join`, `String.replace`,
`list_dir`, `Os.make_dir`, `args()`.

**`bootstrap/check-declares` is new** and fails the build if the runtime
declares a symbol it never calls. `time` was the only one. It is a
one-line check for a bug that costs a user an afternoon, and it now
runs in CI on every push.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/bradcypert/plum/main/install.sh | sh
```

It picks the right archive for your platform, verifies it against the
published checksum, installs `plum` into `~/.local/bin`, and runs it to
prove it works. It **does not edit your shell configuration** — if that
directory is not on your `PATH` it prints the line to add and stops.
`PLUM_PREFIX` and `PLUM_VERSION` override where and which.

Or download an archive directly:

| Platform | Archive |
|---|---|
| Linux x86_64 | `plum-0.0.7-x86_64-linux.tar.gz` |
| Linux arm64 | `plum-0.0.7-arm64-linux.tar.gz` |
| macOS Apple Silicon | `plum-0.0.7-arm64-macos.tar.gz` |
| macOS Intel | `plum-0.0.7-x86_64-macos.tar.gz` |
| Windows x86_64 | `plum-0.0.7-x86_64-windows.tar.gz` |

You need **`clang`** on your PATH; the compiler shells out to it to
assemble and link. Nothing else is required.

## Everything from 0.0.6 is still here

0.0.6 was the memory release: rebuilding a struct or an enum recycles
the old cell, `Array.map` builds into its source, capture-free closures
and constant literals are not allocated at all. A benchmark of
2,000,000 struct updates and 200,000 `Array.map` passes runs in 22 ms
and allocates five times. Nothing in this release changes any of that.

## What is actually checked

74 corpus fixtures under AddressSanitizer with leak detection, 102
lexer/parser goldens, 11 property tests, ten recorded allocation
counts, every project in `examples/` against its recorded output, and a
real language-server session — on Linux x86_64 and arm64, macOS, and
Windows.

`Time.now()` has its own fixture, which checks the three ways a clock
binding actually breaks: not being wired to `time()` at all and
returning 0, being truncated to 32 bits, and two calls disagreeing.
