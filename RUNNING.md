# Running Plum programs

## Running a program

Plum programs live in a project directory. A directory *is* a module
(see "Modules" below). `plum new` scaffolds a minimal starter project:

```sh
plum new myapp
```

```
myapp/
  main.plum
```

```plum fragment
// myapp/main.plum
let main (): Unit = println("hello, plum")
```

Compile and run it in one step:

```sh
plum run myapp
```

(The bare `plum myapp` form, with no `run`, still works too, for backward
compatibility; `plum run` is the recommended, explicit spelling,
symmetric with `plum build`.)

Or keep the binary:

```sh
plum build myapp -o myapp/out
./myapp/out
```

`plum build`'s `-o`/`--output` is optional; if omitted, the output
binary is named after the project directory itself (written to the
current working directory), mirroring `go build`/`cargo build --bin`.

### Building for another platform

`plum build` takes a `--target <triple>`:

```sh
PLUM_CC="zig cc" plum build myapp --target aarch64-linux-musl
```

Plum does not implement cross-compilation so much as get out of its
way. The IR it emits carries no target triple and no datalayout, so
`clang --target=` retargets it directly, and `Os.platform()` is
compiled in by the C compiler's own `#ifdef`, so it reports the
target's platform, not the build machine's, with no work on our part.

What Plum does **not** ship is a sysroot. `clang` can emit code for any
target but cannot link one without that target's libc, so `--target`
needs a C driver that has one. `PLUM_CC` is where you name it; `zig cc`
above, or a corporate cross toolchain. This is the same arrangement
Rust's ecosystem settled on with `cargo-zigbuild`, and it is why a
native build still needs nothing but `clang`: nobody pays for a feature
they do not use. See [DESIGN.md](DESIGN.md)'s cross-compilation section
for the measurements behind that choice (bundling Zig's sysroot would
be a ~240x increase on a 712 KB archive).

Two conveniences: a Windows target gets a `.exe` suffix when you do not
pass `-o`, and a non-64-bit target is refused outright rather than
built. The second is not politeness: cell layout assumes 8-byte slots,
so a 32-bit target would not fail to link, it would silently
miscompile.

`plum run` and `plum test` are always native. They execute what they
build, and this machine cannot run a foreign binary.

`run` and `build` are the SAME path: both compile to a native binary
and differ only in whether it is kept. Nothing can behave one way under
`run` and another under `build`, because there is no second engine for
it to behave differently in.

Both use the same `main` entry point: a single `Unit`-typed parameter
(`let main (): ... = ...`, invoked by the CLI itself, not called from
your own source) returning `Unit` or any printable value (the CLI
prints whatever `main` returns).

`plum help` (also `--help` and `-h`) prints usage. With no arguments
at all, `plum` does the same. Extra arguments after `help` are ignored
rather than treated as a file to compile.

Errors from either path point at real source locations:

```
error: type error: operator: type mismatch: expected Str, found Int
  --> <root>:3:15
  |
3 |     let bad = "hello" + 1;
  |               ^^^^^^^^^^^
```

## Examples

Real, runnable projects under [`examples/`](examples/), one per theme —
each with its output recorded in `expected.txt` and checked by
`bootstrap/example-sweep` (except `asteroids`, which opens a window and
is only built, not run; see its own entry below):

- [`adts_and_matching`](examples/adts_and_matching/main.plum) —
  structs, enums, exhaustive `match`, guard clauses.
- [`option_result`](examples/option_result/main.plum) covers `Option`/
  `Result` combinators for error handling with no null anywhere.
- [`json_and_files`](examples/json_and_files/main.plum) builds a
  `JsonValue`, stringify it, round-trip it through a real file.
- [`concurrency`](examples/concurrency/main.plum) covers `spawn`/`.join()`,
  channels, `send`/`recv`.
- [`generics_and_assoc_fns`](examples/generics_and_assoc_fns/main.plum)
  covers generic structs and `Type.func(args)` associated functions on your
  own types.
- [`shared_mutability`](examples/shared_mutability/main.plum) —
  `Ref[T]`, the opt-in escape hatch for state that's genuinely shared
  or mutated in place.
- [`contracts`](examples/contracts/main.plum) covers `require`/`ensure`
  function contracts: preconditions and postconditions checked at the
  call boundary, contrasted with `option_result`'s `Result`-based
  handling for genuinely expected failure.
- [`currying`](examples/currying/main.plum) covers partial application at
  call sites: an under-applied call becomes a real function value over
  the remaining parameters, composing with ordinary closures and
  higher-order functions for free.
- [`asteroids`](examples/asteroids/main.plum) is a full playable
  Asteroids clone against real [raylib](https://www.raylib.com/), the
  one example that links native C (`native/raylib_shim.c` bridges
  raylib's real ABI (32-bit `float`/`unsigned char` fields) across
  `extern "C"`'s closed, ABI-safe type surface; see that file's own
  doc comment). Build/run with `make`/`make run` inside the example's
  own directory (needs raylib installed and on your linker path, not
  `plum run`/`plum build` directly; see [its own
  README](examples/asteroids/README.md) for install/build steps and
  controls). The most complete demonstration of functional
  game-state-as-value-not-mutation in the whole repo.
