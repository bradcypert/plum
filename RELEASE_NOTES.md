Plum is a small, statically typed, compiled language.

**Linux on arm64 is published for the first time.** If you are on an
ARM Linux box, 0.0.4 had no binary for you — the installer would 404.
This release fixes that.

The compiler is written in Plum. It compiles itself to a fixed point —
the compiler it produces emits byte-for-byte identical output to the
compiler that produced it — and building it needs no toolchain beyond
`clang`.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/bradcypert/plum/main/install.sh | sh
```

New in this release. It picks the right archive for your platform,
verifies it against the published checksum, installs `plum` into
`~/.local/bin`, and runs it to prove it works. It **does not edit your
shell configuration** — if that directory is not on your `PATH` it
prints the line to add and stops. `PLUM_PREFIX` and `PLUM_VERSION`
override where and which.

Or download an archive directly:

| Platform | Archive |
|---|---|
| Linux x86_64 | `plum-0.0.5-x86_64-linux.tar.gz` |
| **Linux arm64** | `plum-0.0.5-arm64-linux.tar.gz` |
| macOS Apple Silicon | `plum-0.0.5-arm64-macos.tar.gz` |
| macOS Intel | `plum-0.0.5-x86_64-macos.tar.gz` |
| Windows x86_64 | `plum-0.0.5-x86_64-windows.tar.gz` |

You need **`clang`** on your PATH; the compiler shells out to it to
assemble and link. Nothing else is required.

## Linux arm64

It is a **tier 1** platform, alongside Linux x86_64 and above macOS and
Windows — it runs the full suite in CI, including AddressSanitizer with
leak detection, which Darwin cannot do at all. That is deliberate rather
than generous: Plum is reference counted, so a leak is a miscompile, and
a different architecture is the likeliest place for a refcounting or
alignment miscompile to appear. Holding a new architecture to a lower
bar than the reference one would be exactly backwards.

It went green on its first CI run: the compiler reproduced itself, 64
corpus fixtures passed under ASan, 102 lexer/parser goldens matched, and
the language server answered a real session.

## Getting started

**[TUTORIAL.md](TUTORIAL.md)** is new — a twenty-minute tour from
`plum new` to a program with tests, covering structs, enums and
exhaustive matching, `Option`/`Result`, arrays, and what a type error
looks like.

Every snippet in it is a complete program, and `bootstrap/doc-check`
compiles and runs each one against the output the tutorial claims. That
is not decoration: writing it caught a paraphrased error message on the
first pass, and this project has shipped documentation describing
software that no longer existed.

`plum new` now scaffolds a project with a function, a string method and
a test — so `plum test` does something immediately — and prints the
three commands you are most likely to want next.

## Editor support

Completion now offers **keywords** and the **locals in scope**, on top
of project names, the standard library, enum variants, and the members
of whatever precedes a `.`.

Names and keywords survive a buffer that does not type-check, because
they are read without checking it. Locals do not, since finding them
requires the check to succeed — so a broken buffer loses the locals and
keeps everything else, rather than losing completion entirely.

## What is checked

Linux x86_64 and Linux arm64 run the full suite: 64 corpus fixtures —
44 that must run and print exactly the right bytes, 7 that must abort
with the right message, 13 that the type checker must reject — under
AddressSanitizer with `detect_leaks=1`, plus 11 property tests, 102
lexer/parser goldens, and the tutorial's snippets.

macOS and Windows run the execution fixtures and a language-server
session.

## Known limits

- **Completion and hover need a plain identifier before the `.`.**
- **`plum local-install`** does not exist; the install script above is
  the supported path.
- This is a 0.0.x release. There is no compatibility promise.
