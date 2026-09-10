# Plum

**[plumlang.org](https://plumlang.org)** · [Tutorial](https://plumlang.org/tutorial/) · [Language tour](https://plumlang.org/language/) · [API reference](https://plumlang.org/api/index.html)

An ML-family language with algebraic data types, exhaustive matching,
and no garbage collector. Memory is reference counted with reuse
analysis, so code that reads as if it allocates freely mostly doesn't:
rebuilding a value recycles the old cell rather than allocating a new
one. There are no lifetimes to annotate and nothing to prove to the
compiler.

It compiles through LLVM to a native binary. The compiler is written in
Plum, builds itself to a byte-identical fixed point, and needs no
toolchain beyond `clang` to assemble and link what it emits.

```plum
use Json;

struct Repo { name: String, stars: Int }

// A decoder is a value, so composing decoders composes their error
// paths: a failure five levels down still names all five.
let repo (): Json.Decoder[Repo] =
    Json.map2(
        Json.field("name", Json.string()),
        Json.field("stargazers_count", Json.int()),
        |n, s| Repo { name: n, stars: s })

let main (): Unit =
    match Json.decode_string(repo(), "{\"name\":\"plum\",\"stargazers_count\":412}") {
        Ok(r) => println("${r.name} has ${r.stars} stars"),
        Err(e) => println("bad payload: ${e}"),
    }
```

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/bradcypert/plum/main/install.sh | sh
```

You need `clang` on your `PATH`; nothing else. Full instructions,
platform support and building from source are in
[INSTALL.md](INSTALL.md).

```sh
plum new hello && cd hello && plum run .
```

## Documentation

Everything below is also published at
[plumlang.org](https://plumlang.org).

| | |
|---|---|
| [TUTORIAL.md](TUTORIAL.md) | Twenty minutes from `plum new` to a program with tests. Every snippet is compiled and run on every commit. |
| [LANGUAGE.md](LANGUAGE.md) | The tour: functions, types, ADTs, generics, arrays, strings, concurrency, FFI. |
| [MODULES.md](MODULES.md) | How modules and visibility work, and what the standard library offers. |
| [RUNNING.md](RUNNING.md) | Running and building programs, cross-compiling, and the worked examples. |
| [TOOLING.md](TOOLING.md) | `plum test`, `plum fmt`, the language server, debug builds and stack traces. |
| [GRAMMAR.md](GRAMMAR.md) | The whole language as a grammar. |
| [`docs/stdlib/`](docs/stdlib/) | Generated API reference, one page per module. |
| [VISION.md](VISION.md) | What Plum is for, and what it deliberately is not. |
| [PORTING.md](PORTING.md) | What a new platform costs, and what is already paid. |
| [DESIGN.md](DESIGN.md) | The full history: why every decision was made, including the ones that changed. |
| [MAINTENANCE.md](MAINTENANCE.md) | How to change the compiler without breaking it. |

## Status

Plum is a work in progress, and self-hosted: the compiler is written in
Plum, compiles itself to a byte-identical fixed point, and builds
without a Rust toolchain. The core language and LLVM backend are
substantially complete (scalars, control flow, closures, generics,
arrays, strings, concurrency, FFI), and there is one implementation of
all of it. The Rust one was retired on 2026-08-25.

What is actually checked, rather than claimed: 74 corpus fixtures under
AddressSanitizer with leak detection, 102 lexer/parser goldens, 11
property tests, recorded allocation counts for ten memory-model
fixtures, every project in [`examples/`](examples/) against its recorded
output, and a real language-server session, on Linux x86_64 and arm64,
macOS, and Windows. Running `./bootstrap/` is the honest answer to "what
works"; no list kept by hand is.

Published for Linux (x86_64, arm64), macOS (Apple Silicon, Intel) and
Windows x86_64. A platform is published only once something in CI builds
and runs real programs on it; see [INSTALL.md](INSTALL.md#platforms).
