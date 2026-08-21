Plum is a small, statically typed, compiled language. This is its first
tagged release.

**The compiler is written in Plum.** It compiles itself to a fixed
point — the compiler it produces emits byte-for-byte identical output
to the compiler that produced it — and building it needs no toolchain
beyond `clang`.

## Install

Download the archive below, unpack it, and put `plum` on your PATH. You
need **`clang`** available; the compiler shells out to it to assemble
and link what it emits. Nothing else is required — the C shims Plum
programs use are embedded in the compiler itself.

```sh
tar -xzf plum-0.0.1-x86_64-linux.tar.gz
./plum-0.0.1-x86_64-linux/plum version
```

Linux x86_64 only for now. macOS and aarch64 are not built because they
are not tested, and a release is the wrong place to find that out.

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

The standard library covers strings, arrays, `Option`/`Result`, maps
and sets, JSON, files, processes, environment variables, TCP sockets,
and an HTTP client and server.

Editor support is a language server with diagnostics, hover types, and
go-to-definition.

## Known limitations

Stated because they will be the first things you hit:

- **Completion and live diagnostics are not implemented.** The language
  server re-checks on save, not as you type.
- **`https://` is not supported** by the HTTP client — plain `http://`
  only.
- **Uppercasing `ß` differs** between compiled and interpreted runs
  (`GRÜßE` vs `GRÜSSE`). A libc limitation, pinned by a test rather
  than left to be discovered.
- **`Array.map`, `Array.filter` and `Array.fold` cannot be passed by
  name** (`let f = Array.map`). They have hand-written type inference
  rather than ordinary signatures, so the reference finds nothing.
  Every other standard-library function works this way, and calling
  these normally is unaffected.
- Integer arithmetic is **checked** — overflow stops the program rather
  than wrapping. This is deliberate, and costs roughly 1.6x on
  arithmetic-dense loops.

## How this release was verified

Every check below runs in CI, and all of them ran on this tag:

- the compiler builds from the checked-in seed with clang alone
- it compiles itself to a byte-identical fixed point
- it builds itself from an unrelated directory with no Rust present
- 61 corpus fixtures compile, run, print the expected output, abort
  where they should, and are ASan-clean
- 41 execution fixtures produce identical results under an independent
  interpreter, with one known divergence pinned in both directions
- 8 example projects match their recorded output byte for byte; the
  ninth opens a window and is built but not run
- the language server answers a real session; `plum test` really runs
  tests; TCP and HTTP work in a compiled binary
- the packaged binary is unpacked and used to build and run a program
  before the release is published

## Caveat

This is 0.0.1. The language is young, the surface will change, and
there is no compatibility promise yet.
