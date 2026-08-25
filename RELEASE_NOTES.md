Plum is a small, statically typed, compiled language. **This release
adds macOS and Windows.**

The compiler is written in Plum. It compiles itself to a fixed point —
the compiler it produces emits byte-for-byte identical output to the
compiler that produced it — and building it needs no toolchain beyond
`clang`.

## Install

Download the archive for your platform, unpack it, and put `plum` on
your PATH. You need **`clang`** available; the compiler shells out to it
to assemble and link what it emits. Nothing else is required — the C
shims Plum programs use are embedded in the compiler itself.

| Platform | Archive |
|---|---|
| Linux x86_64 | `plum-0.0.2-x86_64-linux.tar.gz` |
| macOS Apple Silicon | `plum-0.0.2-arm64-macos.tar.gz` |
| macOS Intel | `plum-0.0.2-x86_64-macos.tar.gz` |
| Windows x86_64 | `plum-0.0.2-x86_64-windows.tar.gz` |

```sh
tar -xzf plum-0.0.2-arm64-macos.tar.gz
./plum-0.0.2-arm64-macos/plum version
```

Windows binaries are built for MSYS2/MinGW and the archive contains
`plum.exe`. Windows has shipped `tar` in-box since Windows 10 1803, so
the same archive format works there.

## New since 0.0.1

**macOS and Windows support.** Every artifact above is built on the
platform it targets, by a CI job that first builds the compiler from
the checked-in seed and then builds and runs 43 real programs with it.
Nothing is cross-compiled and nothing is published that has not been
run.

**Live diagnostics in the language server.** Errors update as you type,
against the unsaved buffer rather than the file on disk. The server
never writes to your file.

**An `Os.` namespace** in the standard library: `Os.temp_dir`,
`Os.make_dir`, `Os.remove_file`, `Os.remove_tree`, `Os.copy_tree`,
`Os.self_exe` and `Os.platform`. These replaced the compiler shelling
out to `mktemp`, `rm`, `cp` and `mkdir`, so `plum build` now starts one
child process — `clang` — where it used to start four.

### Fixes

Three of these were only reachable off Linux, and none of them would
have been found without running the tests on the platform:

- **Character case on macOS.** `"Äöü".to_upper()` returned `Äöü`
  unchanged. The runtime emitted `setlocale(6, "C.utf8")` directly as
  IR, and both halves are glibc-specific: `LC_ALL` is 6 on glibc and
  **0** on macOS, where 6 is `LC_MESSAGES`; and `C.utf8` is not a
  locale macOS has. ASCII was unaffected, so it would have shipped
  quietly.
- **Character case on Windows**, a different cause with the same
  symptom: `wint_t` is 16 bits there, so the `towupper` declaration was
  an ABI mismatch, and MinGW's legacy CRT has no UTF-8 locale anyway.
  Now goes through `CharUpperBuffW`, which needs no locale.
- **Float notation on Windows.** `1e-006` where every other platform
  prints `1e-06` — Microsoft's CRT pads exponents to three digits.
- **Line endings on Windows.** The CRT opens `stdout` in text mode, so
  every `\n` a program wrote became `\r\n`. This also corrupted
  `plum emit-llvm`, which writes IR to stdout.
- **The language server on macOS**, which could never have worked: it
  re-invokes the compiler through `/proc/self/exe`, which does not
  exist there. It now uses `Os.self_exe`.

## What's here

```sh
plum new myapp          # scaffold a project
plum run myapp          # compile and run
plum build myapp -o out # compile to a binary
plum test myapp         # compile once, run each test in its own process
plum check myapp        # type-check only
plum lsp                # language server over stdio
```

The language has algebraic data types with exhaustive `match`, generics
with monomorphization, closures and partial application, records with
functional update, `Ref[T]` for shared mutability, contracts
(`require`/`ensure`), threads and channels, and a C FFI. Memory is
managed by reference counting with in-place reuse — no garbage
collector, no manual `free`.

## What is checked, and where

Linux remains the platform where correctness is established. It runs
the full suite: 62 corpus fixtures — 43 that must run and print exactly
the right bytes, 7 that must abort with the right message, 12 that the
type checker must reject — all under AddressSanitizer with
`detect_leaks=1`, plus a comparison of every execution fixture against
an independent interpreter.

macOS and Windows run the 43 execution fixtures. They do **not** run
leak checking: Plum is reference counted, so a leak is a miscompile
rather than untidiness, and LeakSanitizer does not exist on macOS at
all. Correctness is established on Linux and assumed to carry.

## Known limits

- **The language server is untested on Windows.** It is tested on Linux
  and macOS.
- **Linux arm64 is not published.** It is expected to work — macOS
  arm64 proves the compiler generates correct code for the
  architecture — but nothing tests it.
- This is a 0.0.x release. There is no compatibility promise.
