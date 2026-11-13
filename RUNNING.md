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

`run` caches validated LLVM IR. With a supported default Clang toolchain,
it also caches compiled objects when it can fingerprint the compiler and
its loaded libraries. C inputs are preprocessed again so header edits and
changes in include search paths are reflected. Every run links a fresh
temporary executable and inherits your arguments, working directory and
terminal streams. Other platforms and custom `PLUM_CC` commands reuse IR
and compile native inputs normally.

Object reuse supports Linux's default Clang/loader and the x86-64 MSYS2
MINGW64 Clang setup used by Windows CI. Other Windows toolchains, custom
`PLUM_CC` commands, and macOS retain IR reuse and compile native objects afresh.

The Windows adapter requires `MSYSTEM=MINGW64`, the GNU Windows target,
and an ASCII executable spelling ending in `mingw64/bin/clang.exe`. It
launches a short, bounded loader probe and hashes the actual executable
and DLL files selected by Windows. Private DLLs must come from that toolchain's
`bin` directory and have no delayed imports; system DLLs must come from
Windows' system directory. Unknown images, unreadable files, unsupported
layouts, and failed probes bypass object reuse. This contract covers the
standard compiler without compile-time plugins, not arbitrary DLL-loading
compiler extensions. Compiler files must remain stable during a command.

Certified Windows object compilation uses the exact probed executable and
an integrated Clang frontend. The effective command is checked against
Clang's configuration-free command; an external assembler/compiler,
plugin, profile input, or unsupported configuration triggers ordinary
compilation. C is still preprocessed afresh, objects are integrity-checked,
and every invocation links a fresh executable. Windows validates emitted
C LLVM IR rather than materializing large Windows-header JSON syntax trees;
assembly and quoted assembler symbols conservatively bypass C object reuse.
Normal builds keep their existing compiler selection and linking behavior.

The cache is under `PLUM_CACHE/build/v1`, or the same default cache root
used for packages (`XDG_CACHE_HOME/plum`, then `~/.cache/plum`). Source
contents, discovered files, dependencies, compile-time embeds, compiler
contents and build modes determine reuse. A failed edit never runs the
previous successful program. Cache I/O failures fall back to compilation.

```sh
PLUM_NO_BUILD_CACHE=1 plum run myapp  # bypass all build artifacts
PLUM_NO_OBJECT_CACHE=1 plum run myapp # reuse IR, compile native inputs
PLUM_CACHE_TRACE=1 plum run myapp     # cache decisions on stderr
PLUM_PROFILE=1 plum run myapp         # phase timings/counts on stderr
PLUM_BUILD_CACHE_MAX_BYTES=536870912 plum run myapp  # 512 MiB budget
plum cache list build
plum cache clean build
```

Build artifacts have a default 1 GiB budget. After cached compilation,
oldest-published IR, object and shim entries are removed until the cache
fits the budget. Set `PLUM_BUILD_CACHE_MAX_BYTES` to a nonnegative byte
limit to override it; zero retains no completed entries. Invalid values
use the default. Cleanup is best effort: concurrent publishers can
briefly exceed the budget, and cache I/O errors never fail a build.
Staging directories older than 24 hours are removed only when their
recorded owner process has exited; live or unknown owners are preserved.
`plum cache clean build` atomically moves the existing build cache aside,
then deletes it. Runs started concurrently may populate a new cache.
These commands leave fetched packages alone; the existing `plum cache list` and `plum cache clean` continue to manage
packages. Generated C shim sources also live in the build cache so their
paths stay stable between object builds. No executable runs from the cache.

Use a cache directory trusted by your user on this machine. Shared writable
or network cache roots are unsupported: integrity hashes detect corruption,
but do not authenticate artifacts against another writer. Cached LLVM IR
can retain earlier `@embed_file` contents until eviction or explicit cleaning.

Shared shims are verified and published as complete directories, so
concurrent builds cannot read another writer's partially extracted header.

Object validation has a cost: it hashes the native toolchain and runs C
preprocessing even on a hit. Initial cache population can be slower than
an uncached run. `PLUM_NO_OBJECT_CACHE=1` keeps IR reuse while avoiding
native-object validation/population. See the
[measured results](docs/issue-63-results.md) for the local tradeoff.

Build inputs and the toolchain must remain stable during one compilation,
as they must for an ordinary build. Observed source changes prevent cache
publication. Corrupt entries are treated as misses and repaired.

Use `plum run myapp/main.plum` to run a single file. Both file and project
forms use the build cache. The self-hosted compiler requires the `run`
subcommand; a bare `plum myapp` invocation prints usage.

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

## Freestanding objects and ELF linking

The experimental `x86_64-unknown-none` profile emits kernel-model x86-64 ELF
objects without extracting hosted OS shims or linking default libraries:

```sh
plum build mykernel --target x86_64-unknown-none --emit-object -o kernel.o
```

The object contains Plum-generated code plus project/dependency `native/*.c`
inputs. Target-specific hosted directories, including `native/posix`, are
excluded. Add explicit C sources with `--link-c file.c`, assembly with
`--link-asm start.S` (or `.s`), and prebuilt objects with `--link-object file.o`.
Repeated native-input options are supported. Paths are relative to the calling
working directory. The final relocatable object is assembled by `ld.lld -r`.

Link an ELF with an explicit linker script:

```sh
plum build mykernel --target x86_64-unknown-none \
  --linker-script kernel.ld --link-asm start.S -o kernel.elf
```

Or link precompiled objects without compiling a Plum project:

```sh
plum link --target x86_64-unknown-none --linker-script kernel.ld \
  --link-object kernel.o --link-object start.o -o kernel.elf
```

These commands require Clang's x86 backend and LLD's `ld.lld` on `PATH`.
The script defines the ELF entry and memory layout. No startup files, libraries,
bootloader, or missing-symbol stubs are supplied. `--release` selects optimized
code; tracing, sanitizers, memory statistics, and library-link flags are rejected
for the freestanding profile. Object output currently requires this exact target.
Without `-o`, output is `kernel.o` or `kernel.elf` respectively. `--emit-object`
and `--linker-script` are mutually exclusive. `link` accepts prebuilt objects only.

**This pipeline does not yet produce a bootable Plum kernel.** It now selects
only the core runtime symbols reached by the generated entry, globals, functions,
and function-pointer initializers. Hosted startup, OS shims and unused runtime
bodies are excluded. A temporary C `main` initializes globals and invokes Plum
`main`; the boot entry and halt-on-return wrapper are tracked in #508.

Supply native implementations of `plum_kernel_alloc`, `plum_kernel_free`,
`plum_kernel_write`, and `plum_kernel_panic`, plus any required `memcpy`, `memmove`,
`memset`, and `memcmp` primitives. Their ABI is in the
[target contract](docs/freestanding-x86_64.md#core-runtime-and-hooks).
The compiler supplies these declarations, not device drivers or an allocator.
Null allocation results panic before any cell/header write. Output passes
borrowed bytes and explicit lengths, so embedded NUL bytes are preserved.
Integer formatting uses stack scratch rather than libc; floating-point
formatting and libm services are unavailable initially.

The supported core includes checked integer arithmetic, ordinary heap values
(strings, bytes, arrays, records, variants, closures and references), their
reference counting and pure operations whose transitive dependencies stay within
these boundaries. Capacity-dependent reuse is disabled until an allocator
capacity contract exists; preferred allocations request their required size.
The hosted allocation policy is unchanged. Explicit application/dependency C
externs remain available as native adapters; their implementations must satisfy
the final link and the kernel's execution environment.

Reachable hosted services (including calls through closures or global
initializers) fail compilation with the unavailable symbol and dependency path.
Unused imports/functions do not force their hosted services into the core.
Filesystem, processes, host clocks, arguments/environment, host entropy/random seeding, locale,
networking/TLS, threads/channels, memory statistics and tracing are unavailable.
Final linking still fails on missing kernel hooks or primitives. On a build
failure after IR generation, `<output>.ll` is retained for inspection.

Freestanding IR uses its own cache identity. C/IR objects reuse the existing
validated object cache with the effective target/compiler flags; custom drivers
and assembly compile uncached. Linker scripts and prebuilt objects are reread
on every link, so changing them never reuses an old ELF. `PLUM_NO_BUILD_CACHE`
and `PLUM_NO_OBJECT_CACHE` retain their usual meaning.

See the [target and ABI contract](docs/freestanding-x86_64.md) for the intended
boot environment, CPU assumptions, runtime hooks, and later verification steps.


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
