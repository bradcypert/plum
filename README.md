# Plum

Plum is a small ML-family language: expression-oriented, statically and
mostly-inferred typed, no null anywhere, algebraic data types with
exhaustive pattern matching, and refcounted (not garbage-collected)
memory management with a Perceus-style functional-but-in-place
optimizer. It runs two ways: an interpreter for fast iteration, and a
native LLVM backend for compiled binaries — both are kept behaviorally
identical and are tested independently.

See [DESIGN.md](DESIGN.md) for the full design history and rationale
behind every decision below. This README is the practical, "how do I
actually use it" companion.

## Building the toolchain

Plum is implemented in Rust as a Cargo workspace. You'll need a Rust
toolchain and, for compiling to native binaries, `clang` on your `PATH`
(the LLVM backend shells out to it to assemble and link).

```sh
cargo build --workspace --release
```

The `plum` binary is produced at `target/release/plum` (or
`target/debug/plum` for a debug build). The rest of this doc assumes
`plum` is on your `PATH`; substitute the full path otherwise.

## Running a program

Plum programs live in a project directory — a directory *is* a module
(see "Modules" below). `plum new` scaffolds a minimal starter project:

```sh
plum new myapp
```

```
myapp/
  main.plum
```

```
// myapp/main.plum
let main (): Unit = println("hello, plum")
```

Run it directly, through the interpreter:

```sh
plum run myapp
```

(The bare `plum myapp` form — no `run` — still works too, for backward
compatibility; `plum run` is the recommended, explicit spelling,
symmetric with `plum build`.)

Or compile it to a real native executable and run that:

```sh
plum build myapp -o myapp/out
./myapp/out
```

`plum build`'s `-o`/`--output` is optional; if omitted, the output
binary is named after the project directory itself (written to the
current working directory), mirroring `go build`/`cargo build --bin`.

Both paths run the exact same `main` entry point — a single `Unit`-
typed parameter (`let main (): ... = ...`, invoked by the CLI itself,
not called from your own source) returning `Unit` or any printable
value (the CLI prints whatever `main` returns). Use the interpreter
while iterating (no `clang` round trip, instant feedback) and `build`
when you want a real binary to ship or benchmark.

With no arguments at all, `plum` runs a built-in one-expression smoke
test — useful as a zero-setup sanity check that the toolchain itself is
working, not something real programs rely on.

Errors from either path point at real source locations:

```
error: type error: operator: type mismatch: expected Str, found Int
  --> <root>:3:15
  |
3 |     let bad = "hello" + 1;
  |               ^^^^^^^^^^^
```

## Testing

Any top-level function whose name starts with `test_` is a test — no
attribute, no registration, no `pub` required:

```
// myapp/main.plum
let main (): Unit = println("hello, plum")

let test_addition (): Unit = assert_eq(2 + 2, 4)
let test_something_is_true (): Unit = assert(1 < 2)
```

```sh
plum test myapp
```

```
running 2 tests
test test_addition ... ok
test test_something_is_true ... ok

test result: ok. 2 passed; 0 failed
```

`assert(cond)` fails with `"assertion failed"` if `cond` is `false`.
`assert_eq(a, b)`/`assert_ne(a, b)` require `a`/`b` to be the same
`Eq`-comparable, `Show`-able type, and print both values on failure
(via `.to_string()`, so a struct/enum failure is readable, not just
"not equal"):

```
test test_area_is_wrong ... FAILED

failures:

---- test_area_is_wrong ----
assertion failed: left != right
  left:  12.56636
  right: 12

test result: FAILED. 0 passed; 1 failed
```

A failing test is just an ordinary runtime error under the hood — any
other error (an array-index-out-of-bounds, a division by zero, ...)
inside a test function fails it the same way, not only a failed
`assert`. Tests in a non-root module are reported under their
qualified name (`shapes.test_area`, ...), same as anywhere else a
qualified name is used.

`plum test` runs every discovered test through the interpreter by
default — fast, one process for the whole run. `plum test --native`
compiles and runs each test as its own native subprocess instead:
slower, but exercises the real LLVM backend, and — unlike the
interpreter, where one test's runtime error is just an ordinary,
catchable `Result` — a native runtime failure is a hard process abort
with no way to recover and keep going in the same process, so each
test genuinely needs its own process to get an isolated pass/fail
result at all. Both should always agree on the same project; if they
ever don't, that's a real bug worth reporting.

## Editor support

Two pieces, independent of each other:

- **`plum lsp`** — an LSP server served straight out of the `plum`
  binary itself (the same shape `gopls` takes for Go), speaking LSP
  over stdio. Diagnostics only for now (parse/resolution/type errors,
  reported live as you edit) — no hover, go-to-definition, or
  completion yet. Reports one error at a time, not every error in a
  project at once — this matches how the rest of the compiler reports
  errors today (every `CompileError` surface in this codebase stops at
  the first error, not just the LSP), not an LSP-specific limitation.
  Fix-and-recheck is fast in practice.
- **[`tools/tree-sitter-plum`](tools/tree-sitter-plum)** — a
  [tree-sitter](https://tree-sitter.github.io/) grammar for syntax
  highlighting/indentation, transcribed from `GRAMMAR.md`. A genuinely
  separate implementation from `plum-syntax`'s real parser (see that
  directory's own README for the scope note and its two documented,
  deliberate simplifications) — exists purely to drive editor
  highlighting, not a second source of truth for the language's actual
  syntax rules.

**Neovim** is the only editor this is packaged and verified for so
far — see [`editors/nvim`](editors/nvim) for a ready-to-use runtime
bundle (LSP config + tree-sitter highlighting) and setup instructions.
Other editors aren't packaged yet; both pieces above are general enough
(stdio LSP, a standard tree-sitter grammar) that another editor's own
LSP client / tree-sitter integration should be able to point at them
directly, but that hasn't been tried.

## Language tour

### Functions, inference, recursion

```
// No `return`. No mandatory parameter types — full inference fills
// them in from how the function is used and its own body.
let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }

// Explicit annotations are legal and equivalent — recommended for
// top-level/exported signatures so the public API is pinned on
// purpose rather than drifting with whatever currently infers.
let double (n: Int): Int = n * 2
```

Tail calls are guaranteed eliminated by the native backend (compiled to
a real LLVM `musttail` call, i.e. a loop, not a growing call stack).
The interpreter does not share this guarantee — see "Interpreter vs.
native codegen" below.

### Types: `Int`, `Float`, `Bool`, `String`, `Unit`

`String` is the surface-syntax keyword for text (its type is
occasionally referred to as `Str` in compiler-internal contexts, but
`String` is what you write in source). `Int`/`Float` conversions are
explicit, never implicit: `n.to_float()` (widening — always succeeds,
though not always *exact* for very large `Int` values, since `Float`'s
53-bit mantissa can't represent every `i64` value precisely), `x.
to_int()` (truncates toward zero), `x.round_to_int()` (rounds to the
nearest integer first, same convention `Float.round()` itself uses).
Both `Float`-to-`Int` conversions are saturating, never undefined
behavior — `NaN` becomes `0`, and a value outside `Int`'s range becomes
whichever bound it overshot.

### Algebraic data types and pattern matching

```
struct Point { x: Float, y: Float }

enum Shape {
    Circle(Point, Float),
    Rectangle(Point, Point),
}

let area (s: Shape): Float = match s {
    Circle(_, r) => 3.14159 * r * r,
    Rectangle(a, b) => (b.x - a.x) * (b.y - a.y),
}
```

`match` is exhaustive — the compiler rejects a `match` missing a
variant, unless a trailing wildcard (`_ => ...`) catches the rest.
Struct/enum equality (`==`) and `.to_string()` are structural and work
recursively through nested structs/enums/arrays, generated for every
type automatically — no `derive` needed or available.

Field access (`.radius`, `.x`, ...) needs its receiver's type to
already be known at that point in inference — an unannotated
function/closure parameter that's only ever used for field access
won't infer a struct type from that alone, so give it an explicit type
annotation (as `area` does above).

### Generics

```
struct Pair[T] { first: T, second: T }

let swap[T] (p: Pair[T]): Pair[T] = Pair { first: p.second, second: p.first }
```

Bounded type parameters (`[T: Eq]`) are supported where a generic
function needs `==` on its parameter — see `Map`/`Set` in the standard
library below for real examples.

### Associated functions: `Type.func(args)`

```
struct Point { x: Int, y: Int }

let Point.add (a: Point) (b: Point): Point = Point { x: a.x + b.x, y: a.y + b.y }

let main (): Unit = println(Point.add(Point { x: 1, y: 2 }, Point { x: 10, y: 20 }))
```

`let Type.func (...) = ...` declares a real, per-type associated
function — called as `Type.func(args)`, with the "receiver" passed as
an ordinary first argument, not `receiver.func(args)` method-dispatch
syntax. This works for any struct/enum you declare, not just the
standard library's own types (which is exactly how `Option.map`,
`Array.reverse`, `Map.get`, and the rest of the standard library below
are themselves built — see the [Standard
library](#standard-library) section).

Two types can each declare a function with the same name (`Point.add`
and `Circle.add` coexist fine) — there's no collision, since each
lives in its own type's namespace. This is unrelated to (and doesn't
change) qualified enum-variant construction — `Type.Variant(args)`
(e.g. `Shape.Circle(radius)`, always legal even without a `use`, the
same as a bare `Circle(radius)`) still constructs a variant, not an
associated-function call. The two are disambiguated by capitalization:
an associated function name is always lowercase, a variant tag is
always UpperCamelCase.

### Option and Result — no null, anywhere

```
enum Option[T] { Some(T), None }
enum Result[T, E] { Ok(T), Err(E) }
```

These are ordinary generic enums, available in every program with no
`use`/declaration of your own — the same as if you'd written them
yourself at the top of the file. There's no `?`-operator/early-return
sugar yet; propagate a `Result` with an explicit `match`:

```
let read_two (): Result[String, String] = match read_file("a.txt") {
    Err(e) => Err(e),
    Ok(a) => match read_file("b.txt") {
        Err(e) => Err(e),
        Ok(b) => Ok(a.concat(b)),
    },
}

let main (): Unit = println(read_two(()))
```

Note the call site: `read_two(())`, not `read_two()`. Every function
takes exactly one (possibly curried) argument — a `()` parameter list
in a declaration is shorthand for one `Unit`-typed parameter, not zero
parameters, so calling it explicitly passes the unit value `()`.

For the common cases, combinators avoid writing the `match` out by
hand — see the [Standard library](#standard-library) section below for
the full list:

```
let doubled = Option.map(Some(21), |x| x * 2);          // Some(42)
let total = Result.unwrap_or(read_file("a.txt"), "");    // "" if the file is missing
```

### Arrays

```
let xs = [1, 2, 3];
let doubled = xs.map(|x| x * 2);          // [2, 4, 6]
let evens = xs.filter(|x| x % 2 == 0);    // [2]
let total = xs.fold(0, |acc, x| acc + x); // 6
let ys = xs.push(4);                      // [1, 2, 3, 4] — xs itself is untouched
```

Array mutation-shaped methods (`.push()`, `.pop()`, `.set()`,
`.remove()`) are all *functional* — they return a new array rather than
mutating in place, but the compiler applies a reuse-in-place
optimization (FBIP) under the hood when it can prove the original array
is no longer needed, so this is often as cheap as a real mutation
without giving up value semantics.

### Strings

```
let s = "hello";
s.len()                    // 5
s.concat(" world")         // "hello world"
s.split(",")                // Array[String]
s.trim()
s.to_upper() / s.to_lower()
s.starts_with("he") / s.ends_with("lo") / s.contains("ell")
s.replace("l", "L")
s.runes()                  // Array[Int] — Unicode codepoints
s[0]                       // indexing returns a raw byte, not a character
```

There is currently no substring/slice primitive and no codepoint-to-
string conversion in either direction — see the standard library's
JSON implementation for the `chars_of`/one-character-`String`-array
pattern used to work around this.

### Local mutability, `if`/blocks as expressions

```
let go (): Int = {
    let mut total = 0;
    let mut i = 0;
    for i in 0..10 {
        total = total + i;
    };
    total
}
```

`if`/`match`/blocks are all expressions — the last expression in a
block (no trailing `;`) is its value. `else` always requires either
`else if` or a `{ }` block; a bare `else <expr>` isn't valid syntax.

### Concurrency

`spawn`/`.join()` for tasks, channels (`Sender`/`Receiver`) with
`send`/`recv`/`select` for communication between them. See DESIGN.md's
"Concurrency" section for the full memory-ownership story around
sending heap values across task boundaries.

### FFI

```
extern "C" {
    fn strlen(s: CStr) -> Int;
}

let go (): Int = unsafe { strlen("hello".as_cstr()) }
```

Extern calls are only allowed inside an `unsafe { }` block. The extern
type surface is intentionally closed: `Int`/`Float`/`Bool`/`CStr`/a
callback/a struct made of those — no raw pointers, no C-variadic
functions, no extern global variables.

## Modules

A directory *is* a module — no `mod foo;` declaration anywhere. Every
`.plum` file in a directory shares one namespace; subdirectories become
nested child modules, discovered from the file tree itself.

```
myapp/
  main.plum
  shapes/
    circle.plum
    rectangle.plum   // both files are just the `shapes` module
```

```
// shapes/circle.plum
pub struct Circle { radius: Float }
pub let area (c: Circle): Float = 3.14159 * c.radius * c.radius
let internal_helper (c: Circle): Float = c.radius * 2.0   // private, no `pub`
```

```
// main.plum
use shapes;
let main (): Unit = println(shapes.area(shapes.Circle { radius: 2.0 }))
```

Everything is private by default; `pub` opts a `let`/`struct`/`enum`/
individual struct field into visibility outside its own module. `use`
is qualify-by-default (Go-style) — `shapes.area`, not a bare `area` —
so call sites stay self-explanatory without cross-referencing imports.
`use shapes.Circle;` (importing one specific name unqualified) is
available as an escape hatch for names used constantly in a file.

## Standard library

Currently available with no `use` needed (all merged into every
program's prelude):

- **`Option[T]`/`Result[T, E]`** and their constructors (`Some`/`None`/
  `Ok`/`Err`), plus combinators declared as real [associated
  functions](#associated-functions-typefuncargs): `Option.map`,
  `Option.and_then`, `Option.unwrap_or`, `Option.unwrap_or_else`,
  `Option.is_some`, `Option.is_none`, `Option.ok_or`; `Result.map`,
  `Result.map_err`, `Result.and_then`, `Result.unwrap_or`, `Result.
  unwrap_or_else`, `Result.is_ok`, `Result.is_err`.
- **`Int`/`Float` numbers** — same associated-function treatment
  (`<`/`>` only type-check against a concrete numeric type, not a
  generic bound, so `min`/`max`/`abs`/`clamp` need one realization per
  type — `Int.min`/`Float.min` simply coexist, no collision): `Int.min`,
  `Int.max`, `Int.abs`, `Int.clamp`; `Float.min`, `Float.max`, `Float.
  abs`, `Float.clamp`, `Float.floor`, `Float.ceil`, `Float.round`,
  `Float.pow`, `Float.sqrt` (the last five wrap real libm functions via
  `extern "C"`).
- **`Array[T]`** — `Array.is_empty`, `Array.first`/`Array.last:
  Option[T]`, `Array.reverse`, `Array.concat`, `Array.take`/`Array.
  drop`, `Array.slice`, `Array.find: Option[T]`, `Array.find_index:
  Option[Int]`, `Array.any`/`Array.
  all`, `Array.index_of: Option[Int]`, `Array.contains` (both
  `Eq`-bounded), `Array.sort_by(arr, |a, b| ...)` (takes an explicit
  "is `a` less-or-equal `b`" comparator — no generic `Ord` bound
  exists), `Array.zip: Array[Zipped[A, B]]` (`Zipped { first, second }`,
  a plain struct — no tuple codegen support yet), `Array.sum_int`/
  `Array.sum_float`.
- **`String`** — `String.is_empty`, `String.slice` (codepoint-safe, not
  raw byte indexing — never splits a multi-byte character), `String.
  repeat(s, n)`, `String.trim_start`/`String.trim_end` (ASCII
  whitespace only — narrower than the existing, real-Unicode-aware
  `.trim()`), `String.index_of: Option[Int]`, `String.lines`, `String.
  parse_int: Result[Int, String]`/`String.parse_float: Result[Float,
  String]`.
- **`println(x)`/`print(x)`** — print any value's `.to_string()`
  (`print` with no trailing newline, `println` with one).
- **`read_file(path): Result[String, String]`** / **`write_file(path,
  contents): Result[Unit, String]`** — whole-file I/O, no streaming/
  stateful file handle. Failure surfaces as `Err`, never a crash.
- **`Map[K, V]`** — `Map.new`, `Map.insert`, `Map.get: Option[V]`,
  `Map.contains`, `Map.remove`, `Map.len`, `Map.keys`/`Map.values:
  Array[...]`, `Map.from_arrays`. Association-list based (`O(n)`), no
  hashing — fine for small maps, not a performance-critical hash table.
- **`Set[T]`** — `Set.new`, `Set.insert`, `Set.contains`, `Set.remove`,
  `Set.len`, `Set.union`/`Set.intersection`/`Set.difference`,
  `Set.from_array`, `Set.to_array`. Same `O(n)` caveat as `Map`.
- **JSON** — `json_parse(s: String): Result[JsonValue, String]` /
  `json_stringify(v: JsonValue): String`, where `JsonValue` is a plain
  enum (`JsonNull`/`JsonBool`/`JsonNumber`/`JsonString`/`JsonArray`/
  `JsonObject`) you `match` on directly — no separate accessor helpers.
  Supports the escapes Plum's own string-literal lexer can itself
  produce (`\"`, `\\`, `\/`, `\n`, `\r`, `\t`); `\uXXXX`/`\b`/`\f` are
  rejected with a clear `Err` on parse rather than mishandled silently.

All of the above are ordinary Plum source, not compiler magic — you
could write equivalents yourself. They currently live in a shared
prelude rather than real `use`-based modules; that's a known, deliberate
v1 simplification (see DESIGN.md), not a permanent design point.

## Interpreter vs. native codegen

`plum <project>` and `plum build <project>` run the *exact same*
program through the *exact same* front end (parse → type-check →
lower → optimize) and only diverge at the very last step. They're
tested independently throughout this codebase and are expected to
agree on every observable result, with two known, deliberate
exceptions:

- **Tail-call elimination** is a native-codegen-only guarantee (real
  LLVM `musttail`). The interpreter's evaluator has no such guarantee
  and can exhaust the native stack on deeply recursive programs the
  compiled version would run in constant stack space.
- **OS error message text** (e.g. a failed `read_file`) legitimately
  differs in wording between the two — the interpreter surfaces Rust's
  own `std::io::Error` text, native codegen surfaces glibc's
  `strerror`. Both correctly describe the same real OS error.
- **`Ref[T]`** (see DESIGN.md's "Mutability and cycles" section) is
  currently interpreter-only — native codegen has no representation
  for it yet, a documented v1 scope boundary, not a bug.

## Examples

Real, runnable projects under [`examples/`](examples/), one per theme —
each verified through both `plum run` and `plum build` (except
`shared_mutability`, which needs the interpreter — see the `Ref[T]`
note above):

- [`adts_and_matching`](examples/adts_and_matching/main.plum) —
  structs, enums, exhaustive `match`, guard clauses.
- [`option_result`](examples/option_result/main.plum) — `Option`/
  `Result` combinators for error handling with no null anywhere.
- [`json_and_files`](examples/json_and_files/main.plum) — build a
  `JsonValue`, stringify it, round-trip it through a real file.
- [`concurrency`](examples/concurrency/main.plum) — `spawn`/`.join()`,
  channels, `send`/`recv`.
- [`generics_and_assoc_fns`](examples/generics_and_assoc_fns/main.plum)
  — generic structs and `Type.func(args)` associated functions on your
  own types.
- [`shared_mutability`](examples/shared_mutability/main.plum) —
  `Ref[T]`, the opt-in escape hatch for state that's genuinely shared
  or mutated in place.

`examples/overview.plum` is a separate, older syntax sketch from early
in the project's design — illustrative only, not a runnable project.

## Status

Plum is a work in progress. The core language and LLVM backend are
substantially complete (scalars, control flow, closures, generics,
arrays, strings, concurrency, FFI); the standard library is being
built out incrementally. See DESIGN.md for exactly what's implemented
versus still open.
