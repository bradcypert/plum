# Plum

Plum is a small ML-family language: expression-oriented, statically and
mostly-inferred typed, no null anywhere, algebraic data types with
exhaustive pattern matching, and refcounted (not garbage-collected)
memory management with a Perceus-style functional-but-in-place
optimizer. It runs two ways: an interpreter for fast iteration, and a
native LLVM backend for compiled binaries — both are kept behaviorally
identical and are tested independently.

**Plum is self-hosted**: the compiler is written in Plum
(`bootstrap/self_host/`), compiles itself to a fixed point, and needs no
Rust toolchain to build. A Rust implementation (`crates/`) came first
and bootstrapped it; its BACKEND was deleted on 2026-08-21 and what
survives is an interpreter used as a test oracle — see "The Rust
interpreter" below.

See [DESIGN.md](DESIGN.md) for the full design history and rationale
behind every decision below, and [MAINTENANCE.md](MAINTENANCE.md) for
how to change the compiler without breaking it — the test harnesses,
when to refresh the bootstrap seed, and the traps that have caught
people before. This README is the practical, "how do I actually use
it" companion.

## Installing

Grab a release from
[Releases](https://github.com/bradcypert/plum/releases) — a single
binary. You need **`clang`** on your `PATH`; the compiler shells out to
it to assemble and link what it emits. Nothing else is required: the C
shims Plum programs use are embedded in the compiler itself.

```sh
tar -xzf plum-0.0.1-x86_64-linux.tar.gz
./plum-0.0.1-x86_64-linux/plum version
```

### Platforms

A platform is published only once something in CI builds and runs real
programs on it. Nothing here is merely expected to work.

| Platform | Status |
|---|---|
| Linux x86_64 | Full test suite, including leak checking under ASan |
| macOS arm64 | 42 programs built and run in CI. The language server does not work yet |
| macOS x86_64 | Same |
| Linux arm64 | Untested, unpublished |
| Windows | Not yet supported. WSL works |

macOS is a step down from Linux and it is worth knowing why: Plum is
refcounted, so a leak is a miscompile rather than untidiness, and
LeakSanitizer does not exist on Darwin. Correctness is established on
Linux. See [PORTING.md](PORTING.md) for what that costs and what is
left.

## Building the toolchain

Same requirement — `clang`, and nothing else.

```sh
./bootstrap/from-seed -o plum          # clang only, no Rust
./plum build bootstrap/self_host -o plum
```

The first line builds a compiler from `bootstrap/seed/plum.ll` — the
self-hosted compiler shipped as LLVM IR, because building a compiler
written in Plum requires a Plum compiler to start from. The second line
then rebuilds it with itself, which is the compiler you keep.

The rest of this doc assumes `plum` is on your `PATH`; substitute the
full path otherwise.

### The Rust interpreter

There is exactly one compiler: the self-hosted one. `crates/` holds a
Rust front end and INTERPRETER — no code generator, since 2026-08-21 —
and it exists for one reason.

```sh
cargo build --workspace --release       # produces target/release/plum
```

That binary can `run`, `test`, `lsp`, `new` and dump tokens/ASTs. It
cannot `build`: `plum build`, `plum emit-llvm` and `plum compile-ir`
were removed with the backend. Compiling is the self-hosted compiler's
job now.

**Why keep it at all.** `bootstrap/interp-check` runs every execution
fixture through the interpreter and compares it to the compiled answer.
That is a comparison against an independent implementation of the
SEMANTICS, and it earns its place: integer division by zero was
undefined in both backends and printed a different wrong number in
each, while the interpreter had reported `division by zero` all along;
floats printed `0.3` for `0.1 + 0.2` in both backends, where the
interpreter printed `0.30000000000000004` and was right. Comparing two
code generators could not see either — they agreed, and were both
wrong.

Nothing else needs it. Every other harness in `bootstrap/` runs with no
Rust toolchain present.

**It is meant to go eventually.** See DESIGN.md's "Deleting the Rust
backend" for what would have to be true first.

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

`plum test` COMPILES the project once and runs each test in its own
process — a runtime failure is a hard abort with no way to keep going
in the same process, so isolation is not optional.

The Rust `plum test` interprets instead, one process for the whole run.
Both are exercised by `bootstrap/test-smoke`, and are expected to agree
on the same project; if they ever don't, that is a real bug worth
reporting. (There was a `plum test --native` flag on the Rust side
until 2026-08-21; it went with the backend.)

## Editor support

Both compilers serve an LSP out of the `plum` binary itself (the same
shape `gopls` takes for Go), speaking LSP over stdio — but they do not
offer the same things, and this is the one place the Rust
implementation is still ahead:

| | self-hosted | Rust |
|---|---|---|
| diagnostics | on open and save | live, as you edit |
| hover | inferred type of any identifier | resolved type of any expression |
| go-to-definition | locals, params, top-level names | locals, params, fields, variants |
| completion | — | keywords, scope, struct fields |

The self-hosted server asks the TYPE CHECKER, so hovering a local or a
parameter shows the type it was actually inferred to have — `doubled:
Point` — and go-to-definition on a local jumps to its binding, not to
whatever top-level name it happens to share. Top-level names fall back
to a by-name index, which is what supplies a function's full signature
on hover.

Two limits: it needs the project to type-check cleanly, and it re-checks
per request (26ms on a small project, ~0.9s on the compiler's own 14k
lines). Field names and enum variants are not yet resolved — hovering
`.x` in `p.x` answers for `p`.

If editor support is what you care about most today, build the Rust
implementation and point your editor at that binary. Everything below
describes it.

Two pieces, independent of each other:

- **`plum lsp`** — an LSP server served straight out of the `plum`
  binary itself, speaking LSP over stdio. Diagnostics (parse/resolution/type errors, reported live
  as you edit), hover (shows the resolved type under your cursor),
  go-to-definition (variables, params, `let`s, function/global calls,
  struct/enum names, `.field` access, enum variant references), and
  completion — keywords + every function/global/struct/enum/extern
  name in scope (including the whole standard library) generally, and
  a struct's own fields right after typing `.`. Every file that fails
  to PARSE gets its own diagnostic simultaneously (fix three broken
  files, see all three); module-resolution/type errors still cap out
  at one at a time — a later function's error can genuinely depend on
  an earlier one's real, resolved signature (mutual recursion), so
  reporting more than one there risks a misleading cascade, not just
  extra convenience — this matches how the rest of the compiler
  reports errors today (every `CompileError` surface in this codebase
  stops at the first, not just the LSP), not an LSP-specific
  limitation; hover/go-to-definition need the project to type-check
  cleanly first, same reason. General completion falls back to the
  last successfully checked snapshot when the current buffer doesn't
  (typing itself usually leaves it that way); dot completion works
  around this differently — see DESIGN.md's "Completion" and "Multiple
  diagnostics" sections for the details.
  Fix-and-recheck is fast in
  practice.
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

### Struct updates

There's no field mutation (`p.x = 5` isn't valid) — structs are updated
functionally, by spreading the rest of an old value's fields into a new
one, with the spread always last:

```
let p2 = Point { x: 9.0, ..p }
```

For a NESTED field, a dotted path in the field key avoids having to
hand-reconstruct every intermediate level:

```
struct Vec2 { x: Float, y: Float }
struct Ship { position: Vec2, rotation: Float }
struct Game { ship: Ship, score: Int }

let move_ship (g: Game) (nx: Float) (ny: Float): Game =
    Game { ship.position.x: nx, ship.position.y: ny, ..g }
```

This desugars, before type inference runs, into the fully-nested
version you'd otherwise write by hand (`Game { ship: Ship { position:
Vec2 { x: nx, y: ny, ..g.ship.position }, ..g.ship }, ..g }`) — paths
sharing a prefix merge into one nested literal per level. Requires the
literal to also have a `..` spread (nothing else to read the
intermediate values from), and every intermediate segment (`ship`,
`position`) must have a concrete struct type, not a still-generic type
parameter.

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
let doubled = Array.map(xs, |x| x * 2);          // [2, 4, 6]
let evens = Array.filter(xs, |x| x % 2 == 0);    // [2]
let total = Array.fold(xs, 0, |acc, x| acc + x); // 6
let ys = xs.push(4);                             // [1, 2, 3, 4] — xs itself is untouched
```

`map`/`filter`/`fold` are the one part of the standard library that's
implemented as compiler primitives (not ordinary Plum functions) —
called as `Array.map(xs, f)`, never `xs.map(f)`. Dot-call syntax
(`value.name(...)`) is reserved exclusively for the small, fixed set of
zero-argument core value conversions (`.to_string()`, `.to_int()`,
`.round_to_int()`, `.to_float()`, `.as_cstr()`, plus true mutation-shaped
array/string operations like `.push()`/`.len()`) — every stdlib function
that takes real arguments, `map`/`filter`/`fold` included, is always
`Type.func(value, ...args)`. This keeps the rule simple to remember
("does it take extra arguments? then it's `Type.func(...)`") and avoids
ambiguity between field access and method dispatch.

Array mutation-shaped methods (`.push()`, `.pop()`, `.set()`,
`.remove()`) are all *functional* — they return a new array rather than
mutating in place, but the compiler applies a reuse-in-place
optimization (FBIP) under the hood when it can prove the original array
is no longer needed, so this is often as cheap as a real mutation
without giving up value semantics.

### Pipe

`x |> f(a, b)` inserts `x` as `f`'s *last* argument — `f(a, b, x)` — and
`x |> f` (no parens) means `f(x)`. Chains read top to bottom instead of
inside out:

```
[1, 2, 3]
    |> Array.map(_, |x| x * 2)
    |> Array.filter(_, |x| x > 2)
    |> Array.fold(_, 0, |acc, x| acc + x)   // 10
```

Since most stdlib functions take their array/subject *first*, not last,
a bare `_` in one of `f`'s arguments marks where `x` actually goes
instead of appending it — that's what `_` is doing in every call above:
without it, `[1,2,3] |> Array.map(f)` would (wrongly) mean
`Array.map(f, [1,2,3])`. At most one `_` per call; a plain single-
argument call like `xs |> Array.reverse` doesn't need one.

**Pipe + `Result.and_then`/`Result.map` is the house style for
chaining fallible calls** — Plum has no `?`/early-return (deliberately
not built, see DESIGN.md's own section: it would need a `return`
statement the language doesn't have at all, plus a `From`-style error-
conversion mechanism the closed trait set has no room for):

```
tcp_write(fd, request)
    |> Result.and_then(_, |ignored| read_response(fd))
    |> Result.and_then(_, parse_response)
```

reads top-to-bottom instead of the nested-`match` alternative
(`match x { Err(e) => Err(e), Ok(v) => match ... }`). It has one real
limit worth knowing: a later step needing a value from TWO steps back
can't stay flat — wrap that one step in a closure so the earlier
binding stays in scope via capture (`Result.and_then(_, |head| Result
.map(read_body(head), |body| Response { head, body }))`).

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

**Interpolation**: `"${...}"` inside any double-quoted string, no
prefix needed —

```
let name = "world";
let n = 41;
println("hello, ${name}! n=${n + 1}")   // hello, world! n=42
```

is exactly `"hello, ".concat(name.to_string()).concat("! n=").concat((n
+ 1).to_string())` — pure syntax sugar over `.concat()`/`.to_string()`
(both already generic over every type), resolved entirely by the
lexer/parser, so it works everywhere a string literal does. A bare `$`
not followed by `{` is always literal; `\$` escapes one that would
otherwise start an interpolation. `${...}`'s contents can be any
ordinary expression (arithmetic, field access, calls, ...) but can't
itself contain a block expression, a closure with a block body, or a
nested string with its own `${...}` — pull those into a variable first.

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

`.as_cstr()` goes `String -> CStr` (for passing Plum strings out to C);
`.as_string()` goes the other way, `CStr -> String` (for turning a C
function's returned string data — e.g. a socket's `tcp_recv` — into a
real, usable Plum value). `CStr` otherwise has no operations of its
own.

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
  Array[...]`, `Map.from_arrays`. A real hash table (amortized `O(1)`
  average case) — `Array`-of-buckets, resizing at a 0.75 load factor,
  built on the new `String.hash` primitive (see below). `insert`
  overwrites an existing key rather than shadowing it; `len` is the
  unique-key count.
- **`Set[T]`** — `Set.new`, `Set.insert`, `Set.contains`, `Set.remove`,
  `Set.len`, `Set.union`/`Set.intersection`/`Set.difference`,
  `Set.from_array`, `Set.to_array`. A thin wrapper around `Map[T,
  Unit]`, same hash-table performance.
- **`String.hash(s): Int`** — a real FNV-1a hash, always non-negative.
  The one new compiler primitive `Map`/`Set` are built on; any type's
  generic hash is just `String.hash(x.to_string())`, reusing `.to_
  string()`'s own existing structural recursion rather than needing a
  second one.
- **JSON** — `json_parse(s: String): Result[JsonValue, String]` /
  `json_stringify(v: JsonValue): String`, where `JsonValue` is a plain
  enum (`JsonNull`/`JsonBool`/`JsonNumber`/`JsonString`/`JsonArray`/
  `JsonObject`) you `match` on directly — no separate accessor helpers.
  Supports the escapes Plum's own string-literal lexer can itself
  produce (`\"`, `\\`, `\/`, `\n`, `\r`, `\t`); `\uXXXX`/`\b`/`\f` are
  rejected with a clear `Err` on parse rather than mishandled silently.
- **TCP sockets** — `tcp_listen_on(port): Result[Int, String]`, `tcp_
  connect_to(host, port): Result[Int, String]`, `tcp_accept_connection
  (fd): Result[Int, String]`, `tcp_write(fd, data): Result[Int, String]`,
  `tcp_read(fd, max_len): String`, `tcp_close_connection(fd): Unit` —
  blocking, fd-based (an `Int` is the connection). **Unix-only (Linux/
  macOS)**, same documented scope as extern-symbol-resolution has
  elsewhere (see DESIGN.md). `tcp_read` is NUL-terminated (not binary-
  safe — fine for line-oriented protocols like HTTP, not arbitrary
  binary payloads) and returns `""` on both a clean peer-close and a
  hard socket error — a real, deliberate v1 scope trade, not a bug (see
  DESIGN.md's "TCP sockets" section for the full reasoning). UDP isn't
  in yet — deferred pending its own design for `recvfrom`'s sender-
  address problem.
- **HTTP client** — `http_get(url): Result[HttpResponse, String]`,
  `http_post(url, body): Result[HttpResponse, String]`, and the general
  `http_request(method, url, headers, body): Result[HttpResponse,
  String]` (`headers: Array[HttpHeader]`), where `HttpResponse { status:
  Int, headers: Array[HttpHeader], body: String }`. Built entirely on
  top of the TCP module above, no compiler magic. **`http://` only —
  `https://` is rejected with a clear `Err`**, not silently attempted;
  TLS is deliberately deferred as its own future design question. A
  `Transfer-Encoding` (chunked) response is also rejected with a clear
  `Err` rather than mis-parsed — only `Content-Length`-framed or
  read-until-close responses are supported. See DESIGN.md's "HTTP
  client" section for the full scope writeup, including a real
  interpreter-only recursion-depth caveat for very large responses
  under the Rust `plum run` (compiled code has no such limit).
- **HTTP server** — `http_serve_once(port, handler): Result[Unit,
  String]` (listens, handles exactly one connection, returns — a real
  one-shot server on its own) and `http_serve(port, handler): Result
  [Unit, String]` (the real long-running server: accept, handle, close,
  repeat, forever). `handler: (HttpRequest) -> HttpResponse`, where
  `HttpRequest { method: String, path: String, headers: Array
  [HttpHeader], body: String }`. **Concurrent — spawn-per-connection**:
  `http_serve`/`http_serve_loop` spawn a real OS-thread task per
  accepted connection (both backends), so one slow client can't stall
  another behind it; `handler` must be a plain top-level function or a
  closure capturing nothing (a closure that closes over live local
  state can't cross the `spawn` boundary — a clear error in the
  interpreter, a clean runtime abort in native codegen). Every request
  gets exactly one response, connection always closed afterward (no
  keep-alive). See DESIGN.md's "HTTP server" and "Native-codegen
  zero-capture closure fix" sections for the full writeup, including a
  real request/response body-framing asymmetry bug found via an actual
  deadlock (a bodyless `GET` with no `Content-Length` means different
  things on the two sides of a connection).
- **OS: directory listing + subprocess exec** — `list_dir(path): Result
  [Array[String], String]` (entry names, `.`/`..` already skipped),
  `is_directory(path): Result[Bool, String]`, and `run_process(program,
  args): Result[ProcessResult, String]` where `ProcessResult { exit_
  code: Int, stdout: String, stderr: String }` — a non-zero exit code
  is an ordinary `Ok`, `Err` only means the process could never even be
  started. Subprocess output is captured via temp files (not pipes),
  deliberately, to avoid the classic pipe-deadlock class of bug. See
  DESIGN.md's "OS module" section for the full writeup, including a
  real, SEPARATE bug found (and filed, not fixed here) while testing
  this: a top-level global `let` used twice with a heap-consuming
  operation like `.as_cstr()` corrupts under native codegen.

All of the above are ordinary Plum source, not compiler magic — you
could write equivalents yourself. They currently live in a shared
prelude rather than real `use`-based modules; that's a known, deliberate
v1 simplification (see DESIGN.md), not a permanent design point.

## Interpreter vs. native codegen

This section describes the RUST implementation, which has both an
interpreter and a native backend. The self-hosted compiler has only the
native backend: its `plum run` COMPILES to a temporary binary and
executes it, so `run` and `build` cannot disagree there — they are the
same path. (It once had a tree-walking interpreter too. Keeping two
implementations of the semantics in one compiler meant every feature had
to be written twice, and the second half kept not happening: `run` fell
seven features behind `build` before anyone noticed.)

`plum <project>` and `plum build <project>` COMPILE the same way and
differ only in whether the binary is kept.

The Rust `plum run` interprets, and `bootstrap/interp-check` requires
it to agree with the compiled answer on every execution fixture. Two
deliberate exceptions:

- **Tail-call elimination** is a compiled-only guarantee (real LLVM
  `musttail`). The interpreter's evaluator has none and can exhaust the
  native stack on deeply recursive programs the compiled version runs
  in constant space.
- **OS error message text** (e.g. a failed `read_file`) legitimately
  differs in wording — the interpreter surfaces Rust's own
  `std::io::Error` text, compiled code surfaces glibc's `strerror`.
  Both describe the same real OS error.

A third exception used to be listed here — that `Ref[T]` was
interpreter-only because native codegen had no representation for it.
That stopped being true in 2026-08 and the sentence stayed. It is
exercised compiled by `bootstrap/exec_corpus/refs` and by the
`shared_mutability` example.

## Examples

Real, runnable projects under [`examples/`](examples/), one per theme —
each with its output recorded in `expected.txt` and checked by
`bootstrap/example-sweep` (except `asteroids`, which opens a window and
is only built, not run — see its own entry below):

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
- [`contracts`](examples/contracts/main.plum) — `require`/`ensure`
  function contracts: preconditions and postconditions checked at the
  call boundary, contrasted with `option_result`'s `Result`-based
  handling for genuinely expected failure.
- [`currying`](examples/currying/main.plum) — partial application at
  call sites: an under-applied call becomes a real function value over
  the remaining parameters, composing with ordinary closures and
  higher-order functions for free.
- [`asteroids`](examples/asteroids/main.plum) — a full playable
  Asteroids clone against real [raylib](https://www.raylib.com/), the
  one example that links native C (`native/raylib_shim.c` bridges
  raylib's real ABI — 32-bit `float`/`unsigned char` fields — across
  `extern "C"`'s closed, ABI-safe type surface; see that file's own
  doc comment). Build/run with `make`/`make run` inside the example's
  own directory (needs raylib installed and on your linker path, not
  `plum run`/`plum build` directly — see [its own
  README](examples/asteroids/README.md) for install/build steps and
  controls). The most complete demonstration of functional
  game-state-as-value-not-mutation in the whole repo.

`examples/overview.plum` is a separate, older syntax sketch from early
in the project's design — illustrative only, not a runnable project.

## Status

Plum is a work in progress, and self-hosted: the compiler is written in
Plum, compiles itself to a byte-identical fixed point, and builds
without a Rust toolchain. The core language and LLVM backend are
substantially complete (scalars, control flow, closures, generics,
arrays, strings, concurrency, FFI), and the standard library reaches
parity between the two implementations.

Every project in `examples/` builds and runs identically under both,
which `./bootstrap/example-sweep` checks — that sweep, rather than any
list kept by hand, is the honest answer to "what still differs". See
DESIGN.md for the full history.
