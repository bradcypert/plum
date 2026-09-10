---
title: "Install"
blurb: "One binary, and clang."
---

```sh
curl -fsSL https://raw.githubusercontent.com/bradcypert/plum/main/install.sh | sh
```

That picks the right archive for your platform, checks it against the
published checksum, installs `plum` into `~/.local/bin`, and runs it to
prove it works. It **does not edit your shell configuration** — if that
directory is not on your `PATH` it prints the line to add and stops.
`PLUM_PREFIX` and `PLUM_VERSION` override where and which.

You need **`clang`** on your `PATH`; the compiler shells out to it to
assemble and link what it emits. Nothing else is required — the C shims
Plum programs use are embedded in the compiler itself.

Or take an archive from
[Releases](https://github.com/bradcypert/plum/releases) directly. It is a
single binary.

```sh
tar -xzf plum-0.0.26-arm64-macos.tar.gz
./plum-0.0.26-arm64-macos/plum version
```

## Your first program

```sh
plum new hello
cd hello
plum run .
```

From there, the [tutorial](/tutorial/) is a twenty-minute tour to a
program with tests. Every snippet in it is a complete program, and each
one is compiled and run against the output it claims on every commit.

## Platforms

A platform is published only once something in CI builds and runs real
programs on it. Nothing here is merely expected to work.

| Platform | Status |
|---|---|
| Linux x86_64 | Full test suite, including leak checking under ASan |
| Linux arm64 | Full test suite, including leak checking under ASan |
| macOS arm64 | The whole execution corpus built and run in CI, plus the language server |
| macOS x86_64 | Same, checked on release tags |
| Windows x86_64 | The whole execution corpus built and run in CI, plus the language server |

macOS and Windows are a step down from Linux, and it is worth knowing
why: Plum is reference counted, so a leak is a *miscompile* rather than
untidiness, and LeakSanitizer does not exist on Darwin. Both Linux
targets run it — which is why arm64, a different architecture and so the
likeliest place for a refcounting or alignment miscompile, is held to the
same bar as x86_64 rather than a lower one. [Porting](/porting/) covers
what that costs and what is left.

## Building the compiler from source

Same requirement: `clang`, and nothing else.

```sh
./bootstrap/from-seed -o plum          # no Rust, no Plum needed
./plum build bootstrap/self_host -o plum
```

The first line builds a compiler from `bootstrap/seed/plum.ll` — the
self-hosted compiler shipped as LLVM IR, because building a compiler
written in Plum requires a Plum compiler to start from. The second line
rebuilds it with itself, and that is the one you keep.

## Editor support

`plum lsp` speaks the Language Server Protocol over stdin and stdout:
diagnostics, hover, go-to-definition, and completion. Point your editor's
LSP client at it for files matching `*.plum`.
