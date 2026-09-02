# Plum

Plum is a small ML-family language: expression-oriented, statically and
mostly-inferred typed, no null anywhere, algebraic data types with
exhaustive pattern matching, and refcounted (not garbage-collected)
memory management with a Perceus-style functional-but-in-place
optimizer. It compiles through LLVM to a native binary — there is no
interpreter and no VM, so `plum run` and `plum build` are the same path
and cannot disagree.

**Plum is self-hosted**: the compiler is written in Plum
(`bootstrap/self_host/`), compiles itself to a fixed point, and needs no
Rust toolchain to build, or to be present at all. A Rust
implementation came first and bootstrapped it; its backend was deleted
on 2026-08-21 and the rest followed on 2026-08-25 — see "There is no
Rust in this repository" below.

See [DESIGN.md](DESIGN.md) for the full design history and rationale
behind every decision below, and [MAINTENANCE.md](MAINTENANCE.md) for
how to change the compiler without breaking it — the test harnesses,
when to refresh the bootstrap seed, and the traps that have caught
people before. This README is the practical, "how do I actually use
it" companion.

## Installing

```sh
curl -fsSL https://raw.githubusercontent.com/bradcypert/plum/main/install.sh | sh
```

That picks the right archive for your platform, checks it against the
published checksum, installs `plum` into `~/.local/bin`, and runs it to
prove it works. It **does not edit your shell configuration** — if that
directory is not on your `PATH` it prints the line to add and stops.
`PLUM_PREFIX` and `PLUM_VERSION` override where and which.

You need **`clang`** on your `PATH`; the compiler shells out to it to
assemble and link what it emits. Nothing else is required: the C shims
Plum programs use are embedded in the compiler itself.

Or take an archive from
[Releases](https://github.com/bradcypert/plum/releases) directly — it
is a single binary.

**New to Plum?** [TUTORIAL.md](TUTORIAL.md) is a twenty-minute tour
from `plum new` to a program with tests. Every snippet in it is a
complete program, and `bootstrap/doc-check` compiles and runs each one
against the output it claims. The rest of this README is the
reference.

Archives are published for Linux on x86_64 and arm64, macOS on Apple
Silicon and Intel, and Windows x86_64. The Windows one contains
`plum.exe` and is built for MSYS2/MinGW.

```sh
tar -xzf plum-0.0.7-arm64-macos.tar.gz
./plum-0.0.7-arm64-macos/plum version
```

### Platforms

A platform is published only once something in CI builds and runs real
programs on it. Nothing here is merely expected to work.

| Platform | Status |
|---|---|
| Linux x86_64 | Full test suite, including leak checking under ASan |
| Linux arm64 | Full test suite, including leak checking under ASan |
| macOS arm64 | the whole execution corpus built and run in CI, plus the language server |
| macOS x86_64 | Same, checked on release tags |
| Windows x86_64 | the whole execution corpus built and run in CI, plus the language server |

macOS and Windows are a step down from Linux and it is worth knowing
why: Plum is refcounted, so a leak is a miscompile rather than
untidiness, and LeakSanitizer does not exist on Darwin. Both Linux
targets run it, which is why arm64 — a different architecture, and so
the likeliest place for a refcounting or alignment miscompile — is
held to the same bar as x86_64 rather than a lower one. See [PORTING.md](PORTING.md) for what that costs and what is
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

### There is no Rust in this repository

There used to be. Plum began as a Rust compiler, and after the
self-hosted one replaced its code generator on 2026-08-21 a Rust front
end and interpreter stayed on as a test oracle: `interp-check` ran every
execution fixture through it and compared answers. It earned that place
twice — integer division by zero was undefined in both code generators
and printed a different wrong number in each, and `0.1 + 0.2` printed
`0.3` in both, where the interpreter was right on both counts.

It was retired on 2026-08-25 — 44,698 lines, a CI job, and the Rust
toolchain dependency. Two things had gone wrong with it:

- **It could not see shared bugs.** An oracle finds *disagreements*. On
  the day it was retired, property tests found two bugs the interpreter
  had *identically* — `parse_int` rejecting `Int`'s own minimum, and
  `parse_float` landing one ulp out. It had agreed with the compiler on
  both for as long as they existed.
- **It lagged the language**, so the newest features — the ones most
  likely to be wrong — were exactly the ones it could not check.

`bootstrap/property-check` replaced it. Properties are written in Plum
and run by `plum test`, so they track the language instead of trailing
it, and they encode invariants known in advance rather than whatever an
implementation happens to produce. See DESIGN.md's "Properties, and two
bugs an oracle could never find".

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

Compile and run it in one step:

```sh
plum run myapp
```

(The bare `plum myapp` form — no `run` — still works too, for backward
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
compiled in by the C compiler's own `#ifdef` — so it reports the
target's platform, not the build machine's, with no work on our part.

What Plum does **not** ship is a sysroot. `clang` can emit code for any
target but cannot link one without that target's libc, so `--target`
needs a C driver that has one. `PLUM_CC` is where you name it — `zig cc`
above, or a corporate cross toolchain. This is the same arrangement
Rust's ecosystem settled on with `cargo-zigbuild`, and it is why a
native build still needs nothing but `clang`: nobody pays for a feature
they do not use. See [DESIGN.md](DESIGN.md)'s cross-compilation section
for the measurements behind that choice (bundling Zig's sysroot would
be a ~240x increase on a 712 KB archive).

Two conveniences: a Windows target gets a `.exe` suffix when you do not
pass `-o`, and a non-64-bit target is refused outright rather than
built. The second is not politeness — cell layout assumes 8-byte slots,
so a 32-bit target would not fail to link, it would silently
miscompile.

`plum run` and `plum test` are always native. They execute what they
build, and this machine cannot run a foreign binary.

`run` and `build` are the SAME path: both compile to a native binary
and differ only in whether it is kept. Nothing can behave one way under
`run` and another under `build`, because there is no second engine for
it to behave differently in.

Both use the same `main` entry point — a single `Unit`-typed parameter
(`let main (): ... = ...`, invoked by the CLI itself, not called from
your own source) returning `Unit` or any printable value (the CLI
prints whatever `main` returns).

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

`bootstrap/test-smoke` exercises it against a fixture that
deliberately uses the things a smoke test is tempted to skip — the
prelude's assertions, a `Ref`, a zero-argument call, a partial
application — and asserts that a failing test both fails and does not
stop the ones after it. `plum test` was silently broken for months
before that fixture existed.

## Formatting

```sh
plum fmt --check src/     # list what needs formatting, non-zero exit
plum fmt --write src/     # format in place
plum fmt one.plum         # to stdout
```

`plum fmt` has seven opinions:

* statements, items and match arms are indented four spaces per block,
  and a closing brace sits one level out from what it closes;
* a run of comment lines is indented with whatever it documents;
* a run of blank lines collapses to one;
* a comma has no space before it and one after, and so does a colon --
  though a colon's LEADING space is left alone, because `require b != 0
  : "message"` spells it that way;
* nothing sits just inside a bracket, so `f( a )` becomes `f(a)`;
* a binary operator gets one space on each side, while a unary minus
  does not: `a-b` becomes `a - b` and `-n` is left alone;
* a line that CONTINUES a construct begun on an earlier line is indented
  four past the line that construct started on.

That last one covers the shapes the formatter used to leave alone -- a
`let` body written on the next line, a `|>` chain broken across lines, a
multi-line call's arguments:

```plum
let bounds (xs: Array[String]): String =
    xs
        |> Array.map(_, String.trim)
        |> Array.join(_, ", ")

let main (): Unit = println(bounds([" a ", " b "]))
```

```
a, b
```

Every link of the chain is inside the one expression that began at `xs`,
so every link is indented from that line.

Everything else is passed through untouched, and it does not reflow: no
line is ever joined or split.

The rules were measured against this repository rather than chosen, and
the test of that is `bootstrap/fmt-check`: every `.plum` file here is
already formatted, so `plum fmt` changes none of them.

It also declines to place lines it cannot name -- a line inside a
multi-line string literal, a hand-aligned line sitting deeper than the
grid answer, an item's `require`/`ensure`/`=` header lines. Those are
places where this repository either says nothing or disagrees with
itself, and a formatter with no evidence should leave your code alone.

**It cannot corrupt a file.** Before writing, `--write` re-lexes its own
output and compares the token sequence to the original's -- whitespace
and comments are exactly what lies between tokens, so files whose tokens
agree in order differ only in formatting. A rule that changed the tokens
would be refused rather than written. Writes go to a temporary file
beside the original and are renamed into place, so an interrupted run
cannot leave a half-written source file.

## Editor support

Formatting is served over LSP as well as on the command line, so an
editor can format on save. It formats the buffer being edited, not the
file on disk -- see the Formatting section above for what the formatter
will and will not do.

Expand-selection is served too (`textDocument/selectionRange`): put the
cursor on a name and grow the selection one construct at a time --
`v`, then `v * 2`, then `a + v * 2`, then the closure, then the call,
then the whole declaration. The chain is the nesting of expressions
around the cursor, which is what the compiler's expression spans record.

`plum lsp` serves an LSP out of the `plum` binary itself (the same
shape `gopls` takes for Go), speaking LSP over stdio. It is in every
published binary, on Linux, macOS and Windows.

| | |
|---|---|
| diagnostics | live, as you edit, against the unsaved buffer |
| hover | the inferred type of any identifier, field or method |
| go-to-definition | locals, params, top-level names |
| completion | project names, the stdlib, enum variants; fields and methods after `.` |

It asks the TYPE CHECKER, so hovering a local or a parameter shows the
type it was actually inferred to have — `doubled: Point` — and
go-to-definition on a local jumps to its binding, not to whatever
top-level name it happens to share. Top-level names fall back to a
by-name index, which is what supplies a function's full signature on
hover.

Completion offers every top-level name in your project, the standard
library's 100-odd public functions, and every enum variant, each with
its signature as the detail. The whole list is returned and your editor
filters it, which is what LSP clients do anyway.

**After a `.`** it offers the members of whatever precedes the dot
instead: a struct's fields with their declared types, and every
function namespaced under that type — so `p.` on a `Point` offers `x`,
`y` and your own `Point.shift`, while `s.` on a `String` offers the
nineteen `String.` functions. The type comes from the checker, so this
works on a local whose type was never written down. The base must be a
plain identifier; `foo().bar.` returns nothing rather than guessing,
and the editor falls back to the whole-project list.

Known limits: hover and go-to-definition need the project to type-check
cleanly, and the server re-checks per request (26ms on a small project,
~0.9s on the compiler's own 14k lines). Hover resolves fields and
methods as well as identifiers — `x` in `p.x` reports `Int`, and
`trim_end` in `s.trim_end()` reports its whole signature — but only
when the base is a plain identifier, the same limit dot completion has.
Hovering `to_string` in `p.x.to_string()` answers nothing rather than
guessing. The language server is exercised by a real LSP
session in CI on Linux, macOS and Windows.

Two pieces, independent of each other:

- **`plum lsp`** — an LSP server served straight out of the `plum`
  binary itself, speaking LSP over stdio.

  **Diagnostics** are live, against the unsaved buffer rather than the
  file on disk, and are attributed to the file the error is IN — which
  is not always the file being edited. Fixing an error publishes an
  empty list for that file, so it clears rather than lingering. One
  error at a time: the checker stops at the first, because a later
  function's error can genuinely depend on an earlier one's resolved
  signature, and reporting a cascade would be worse than reporting one
  real thing.

  **Hover** gives the inferred type of an identifier, a field or a
  method. **Go-to-definition** covers locals, parameters and top-level
  names — a local jumps to its binding, not to whatever top-level name
  it shadows.

  **Completion** offers project names, the standard library, enum
  variants, keywords and the locals in scope; after a `.` it offers
  the members of whatever precedes the dot instead. Names and keywords
  survive a buffer that does not type-check, because they are read
  without checking it; locals do not, since finding them requires the
  check to succeed.

  Hover and go-to-definition need the project to type-check cleanly.
  Fix-and-recheck is fast in practice — 26ms on a small project.
- **[`tools/tree-sitter-plum`](tools/tree-sitter-plum)** — a
  [tree-sitter](https://tree-sitter.github.io/) grammar for syntax
  highlighting/indentation, transcribed from `GRAMMAR.md`. A genuinely
  separate implementation from the compiler's own parser (see that
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

```plum
// No `return` — a function's body IS its value.
//
// A top-level signature is written out in full: parameter types and a
// return type. Inference works INSIDE a function, not across its
// boundary, so a public API is pinned on purpose rather than drifting
// with whatever currently infers.
let sum (n: Int) (acc: Int): Int = if n == 0 { acc } else { sum(n - 1, acc + n) }

let main (): Unit = {
    // Local bindings and closure parameters ARE inferred -- neither
    // `total` nor `v` says what it is.
    let total = Array.fold([1, 2, 3], 0, |a, v| a + v);
    println(sum(3, total).to_string())
}
```

```
12
```

A tail-recursive function runs in constant stack space. A call to the
function itself in tail position is emitted as an LLVM `musttail` call
that returns the result directly, which is a **guarantee at every
optimisation level** rather than something the optimiser may or may not
do for you. `bootstrap/check-build-modes` runs a three-million-deep tail
recursion in the default, `--release` and `--trace` builds, so this
cannot regress quietly.

This holds whatever the parameter types are. Until 2026-09-02 it did
not: a function's slot releases are emitted on the way out, so with a
heap-typed parameter or `let` they sat between the recursive call and
the return of its value, which stopped the recursion becoming a loop.
`count` over two `Int`s was fine while the same function over a `String`
overflowed. Those releases now happen before the call.

### Tuples

```plum
let pair (): (Int, String) = (1, "a")
let swap (p: (Int, String)): (String, Int) = match p { (n, s) => (s, n) }

let main (): Unit = println(match swap(pair()) { (s, n) => s.concat(n.to_string()) })
```

```
a1
```

Tuples work anywhere a type does — nested, inside arrays, as struct
fields, returned from generic functions. The one thing they lack is
`.to_string()`; render the elements instead, as above.

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

**`==` works on anything; `<`, `<=`, `>` and `>=` need an ordered
type.** Equality is structural — arrays, structs, enums and their
payloads, all the way down. Ordering is defined for `Int`, `Float` and
`String` only, and comparing anything else is a compile error naming
the type. Arrays and structs could be given a lexicographic order and
deliberately have not been: nothing needs it, enums have no obvious
answer, and rejecting can be relaxed later while the reverse cannot.

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

**A comparison through a type parameter is checked where the type is
known.** `<` needs an ordered type and `==` needs one that is not a
function, and both rules apply inside a generic just as they do outside
it:

```plum
struct Point { x: Int }

let biggest [T] (a: T) (b: T): T = if a > b { a } else { b }

let fine (): Int = biggest(3, 7)
let broken (): Point = biggest(Point { x: 1 }, Point { x: 2 })

let main (): Unit = println(fine().to_string())
```

```
error: call to biggest: T is Point, but biggest requires T to be ordered
```

The error lands on the **call**, because that is where the type is
chosen. The definition is correct code and stays legal — it is only the
attempt to use it on something unorderable that is not.

The same applies to `.to_string()`: rendering a type parameter requires
the type argument to have a text form, so `show(ref(1))` is rejected at
the call rather than accepted and then refused by the build.

Bounds may also be written down — `[T: Ord]`, `[T: Eq]` and `[T: Show]`
— and are then required of callers whether or not the body compares or
renders anything. Writing one is optional: it pins the requirement into
the signature so a body that stops needing it does not silently widen
what callers may pass.

### Associated functions: `Type.func(args)`

```plum
struct Point { x: Int, y: Int }

let Point.add (a: Point) (b: Point): Point = Point { x: a.x + b.x, y: a.y + b.y }
let Point.show (p: Point): String = p.x.to_string().concat(",").concat(p.y.to_string())

let main (): Unit = {
    let p = Point { x: 1, y: 2 };
    let q = Point { x: 10, y: 20 };
    // The same call, written both ways.
    println(Point.show(Point.add(p, q)));
    println(Point.show(p.add(q)))
}
```

```
11,22
11,22
```

`let Type.func (...) = ...` declares a real, per-type associated
function. It can be called either way: `Type.func(receiver, args)` with
the receiver as an ordinary first argument, or `receiver.func(args)`.
The two are the same call — the second is defined as the first, which is
why `xs.map(f)` works at all (see [Standard
library](#standard-library)).

This works for any struct/enum you declare, not just the standard
library's own types — which is exactly how `Option.map`,
`Array.reverse`, `Map.get`, and the rest of the standard library below
are themselves built.

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
use Os;

let read_two (): Result[String, String] = match Os.read_file("a.txt") {
    Err(e) => Err(e),
    Ok(a) => match Os.read_file("b.txt") {
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
let total = Result.unwrap_or(Os.read_file("a.txt"), "");    // "" if the file is missing
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
Net.write(fd, request)
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

**Bytes or characters?** `.len()` counts BYTES; everything else in the
string library counts characters. `String.slice` can never split a
multi-byte character in half, because it works on codepoints and never
sees bytes at all. When you need a count that matches, use
`String.char_len`:

```
"café".len()               // 5 — bytes
String.char_len("café")    // 4 — characters
String.slice("café", 0, 3) // "caf"
```

Padding counts characters too, which is the point of it — text lined
up in columns by byte count puts an accented name in the wrong place:

```
String.pad_left("7", 5, "0")     // "00007"
String.pad_right("ab", 5, ".")   // "ab..."
```

Both return the string unchanged rather than truncating it when it is
already at least that wide, and unchanged rather than panicking when
the fill is not exactly one character.

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

### Compile-time builtins: `@name(...)`

A leading `@` marks a call the **compiler** performs while reading your
source, rather than one your program performs while running. There is
one today:

`@embed_file("path")` is replaced by that file's contents, as a
`String`, while the source is being parsed:

```
let template (): String = @embed_file("templates/adr.md")
```

The path resolves against **the source file's own directory**, not the
working directory the compiler was launched from, so a build does not
depend on where you started it. A module in a subdirectory embeds
relative to itself.

The argument must be a literal string. The file is read at compile
time, so it cannot depend on a value — and an interpolated string
counts as a value, not a literal:

```
@embed_file("templates/${name}.md")   // error, and says why
```

Embedded text is data. It is never re-lexed as Plum, so `${...}` inside
an embedded file stays exactly as written.

The sigil also keeps builtins out of the identifier namespace, so
nothing is reserved — `embed_file` remains an ordinary name you are
free to define:

```
let embed_file (p: String): String = read_or_default(p)   // fine
```

Text only: the result is a `String`, so this covers templates, schemas,
SQL, fixtures and help text, but not images. A missing file is a
compile error pointing at the call.

### Concurrency

`spawn`/`.join()` for tasks, and channels (`Sender`/`Receiver`) with
`send`/`recv` for communication between them. `channel[T]()` needs its
type argument written out — there is nothing else in the expression to
infer it from.

**`select` is not implemented.** The keyword parses and the checker
then rejects it, which is the worst of both: the syntax looks
supported. It needs a runtime primitive that waits on several channels
at once, which the current one-mutex-and-condvar-per-channel design has
no way to express. Tracked, not shipped.

See DESIGN.md's "Concurrency" section for the full memory-ownership
story around sending heap values across task boundaries.

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

## Debug and release builds

```sh
plum build myapp            # debug: -O0 -g, keeps frames and debug info
plum build myapp --release  # optimised: -O2
plum test myapp --release   # the same choice for tests
```

**Debug is the default**, and the asymmetry is the reason: someone who
wanted the fast binary and got the debuggable one has a slow program and
a flag to learn, while someone who wanted to debug and got the optimised
one has inlined frames and nothing to tell them why.

A debug binary keeps its frames and carries DWARF, so a debugger can
follow it. What it does not yet carry is Plum line information — the
emitted IR has no `!dbg` metadata, so a debugger shows the C runtime's
lines, not yours. Plum function names are real symbols in both modes.

### Stack traces

```sh
plum build myapp --trace
```

A program built with `--trace` prints a stack trace to **stderr** when
it dies — for a failed bounds check, a division by zero, an overflow, a
broken contract, or an explicit `panic_raw`:

```
array index out of bounds        <- stdout, the same in every mode
stack trace:                     <- stderr, only with --trace
  at deepest
  at middle
  at outer
  at main
```

Innermost first, with your own function names. Compiler-generated frames
are hidden, so a failed precondition starts at the function you wrote.
Deep recursion is capped at 256 frames with a count of the rest.

**Tail recursion still runs in constant stack space under `--trace`.**
The trace is a shadow call stack — a frame pushed on entry, popped on
the way out — and where that pop goes decides whether the optimiser can
still turn a tail-recursive call into a loop. Each path through a
function pops its own frame, and a call to the function *itself* in tail
position pops *before* calling, so nothing sits between that call and
the return. The investigation, including two designs that failed, is in
[TRACING.md](TRACING.md).

One consequence is worth knowing: a tail-recursive chain shows **one**
frame, not one per iteration.

```
division by zero
stack trace:
  at count      <- three million calls deep; one frame
  at main
```

That is an accurate description rather than a lost frame. A tail call
really does replace its caller's frame, so the shadow stack replaces
the entry too. A call to a *different* function in tail position keeps
both frames, as `middle` does above.

It remains a flag rather than something the debug build always does,
because a shadow stack still costs a push and a pop per call.

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

```plum
// shapes/circle.plum
pub struct Circle { pub radius: Float }
pub let area (c: Circle): Float = 3.14159 * c.radius * c.radius
let internal_helper (c: Circle): Float = c.radius * 2.0   // private, no `pub`
```

```plum
// main.plum
use shapes;
let main (): Unit = println(shapes.area(shapes.Circle { radius: 2.0 }).to_string())
```

```
12.56636
```

`use` is qualify-by-default (Go-style) — `shapes.area`, not a bare
`area` — so call sites stay self-explanatory without cross-referencing
imports.

**Functions are private by default.** `pub let` opts one into being
callable from outside its module; without it, a qualified call is
rejected:

```plum
// shapes/circle.plum
let secret_helper (): Int = 1
```

```plum
// main.plum
use shapes;
let main (): Unit = println(shapes.secret_helper().to_string())
```

```
error: shapes.secret_helper is private to module `shapes`. Add `pub` to its declaration to use it from the root module
```

**Types are private by default too.** `pub struct` and `pub enum` opt
one into being named from outside its module — in an annotation, in a
literal, and in a pattern:

```plum
// secrets/s.plum
struct Secret { n: Int }
pub let make (): Secret = Secret { n: 7 }
pub let read (s: Secret): Int = s.n
```

```plum
// main.plum
use secrets;

// Fine -- the VALUE may cross. `hold` never names the type.
let hold (): Int = secrets.read(secrets.make())

// Rejected -- the NAME may not.
let named (): secrets.Secret = secrets.make()

let main (): Unit = println(hold().to_string())
```

```
error: struct secrets.Secret is private to module `secrets`
```

A private type that escapes through a `pub` function is **opaque**
outside its module: you can hold it and pass it on, but not take it
apart, because taking it apart means naming it. That is the same shape
a handle type has.

**Fields are private by default too**, independently of their struct:

```plum
// counter/c.plum
pub struct Counter { pub label: String, n: Int }
pub let start (label: String): Counter = Counter { label: label, n: 0 }
pub let count (c: Counter): Int = c.n
```

```plum
// main.plum
use counter;

let label_of (c: counter.Counter): String = c.label   // fine
let count_of (c: counter.Counter): Int = c.n          // rejected

let main (): Unit = println(label_of(counter.start("hits")))
```

```
error: field counter.Counter.n is private to module `counter`
```

A struct with any private field **cannot be constructed from outside
its module**, because a literal has to name every field. That is the
point rather than a side effect: it makes a constructor function the
only way in. The same applies to destructuring — `Counter { label, n }`
and the positional `Counter(label, n)` both name `n`, so both are
refused.

**Two modules may declare the same type name.** A type is identified by
the module that declared it, so `shapes.Circle` and `render.Circle` are
different types, and a bare `Circle` means the one declared where you
wrote it — your own module first, then the root, then the prelude. A
root declaration shadows a prelude one of the same name rather than
colliding with it.

They do not silently unify:

```plum
// inner/p.plum
pub struct P { pub v: Int }
pub let make (): P = P { v: 1 }
```

```plum
// main.plum
use inner;

pub struct P { pub v: Int }

// `P` here is the root module's own, which is not `inner.P`.
let a (): P = inner.make()

let main (): Unit = println("unreachable")
```

```
error: declared return type P doesn't match body type inner.P
```

**Anywhere a type or variant can be named, the module can be part of
the name.** When two modules declare the same enum, that is the only
way to say which one you mean -- in an annotation, an expression, and a
pattern alike:

```plum
// light/shade.plum
pub enum Shade { On, Off }
```

```plum
// dark/lamp.plum
pub struct Lamp { pub watts: Int }
```

```plum
use light;
use dark;

let describe (s: light.Shade): String = match s {
    light.Shade.On => "on",
    light.Shade.Off => "off",
}

let lamp (): dark.Lamp = dark.Lamp { watts: 60 }

let main (): Unit = println(describe(light.Shade.On).concat(" at ").concat(lamp().watts.to_string()))
```

```
on at 60
```

Naming the wrong module is an error rather than a correction: a
`dark.Shade.On` pattern matched against a `light.Shade` reports the
mismatch.

The prelude is a module of its own, so `pub` applies to the standard
library too: `Map.get` is part of the interface, `Map`'s buckets are
not, and reaching for the latter is an error wherever you are.

Its module cannot be named — there is no `use prelude;` and no
`prelude.println(..)`. Prelude names are reached the way they always
were, unqualified; the module exists so that what the prelude does not
export is genuinely unavailable rather than merely undocumented.
`use shapes.Circle;` (importing one specific name unqualified) is
available as an escape hatch for names used constantly in a file.

### Standard-library modules

Most of the standard library is in the prelude, with no `use` needed —
and for the type namespaces that is not a convenience, it is required:
`T.f(x)` *is* the method-call mechanism, so `xs.map(f)` only works
because `Array.map` is always in scope.

A namespace that names no type is a different thing. Those are
modules, and a file that wants one says so:

| module | what is in it |
|---|---|
| `Os` | files, directories, environment, subprocesses, platform, exit |
| `Time` | the clock, and the calendar on top of it |
| `Net` | TCP sockets |
| `Http` | HTTP client and server, built on `Net` |

```
use Os;
use Time;

let stamp (): String = Time.rfc7231(Time.now())
let here (): String = Os.platform()
let conf (): Result[String, String] = Os.read_file("app.conf")
```

A module can depend on another. `Http` is ordinary Plum over `Net`'s
sockets, so `use Http;` brings `Net` in with it — you do not have to
know what a module is built on to use it.

Without the `use`, the error says what to do:

```
unbound variant/function: Time -- `Time` is a standard library module;
add `use Time;` to this file
```

`Time` moved in 0.0.8 and the rest in 0.0.9. `Os` was held back
deliberately: unlike `Time`, it was reachable from every program
already written, so the break got a release of its own rather than
being slipped in beside the feature that revealed it.

What stayed in the prelude, with no `use` needed: `println`/`print`,
the `assert` family, `Json`, and every type namespace.

## Standard library

**[STDLIB.md](STDLIB.md) is the complete list**, generated by the
compiler (`plum stdlib-reference`) and checked against it on every
build, so it cannot drift or go stale. The prose below is the tour;
that file is the reference.

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
  a plain struct — it predates tuple support and has not been changed
  since, to avoid breaking callers), `Array.sum_int`/`Array.sum_float`.
- **`String`** — `String.is_empty`, `String.slice` (codepoint-safe, not
  raw byte indexing — never splits a multi-byte character), `String.
  repeat(s, n)`, `String.trim_start`/`String.trim_end` (ASCII
  whitespace only — narrower than the existing, real-Unicode-aware
  `.trim()`), `String.index_of: Option[Int]`, `String.lines`, `String.
  parse_int: Result[Int, String]`/`String.parse_float: Result[Float,
  String]`.
- **`println(x)`/`print(x)`** — print any value's `.to_string()`
  (`print` with no trailing newline, `println` with one).
- **`use Os;` — whole-file I/O.** **`Os.read_file(path): Result[String, String]`** / **`Os.write_file(path,
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
- **`use Net;` — TCP sockets.** `Net.listen_on(port): Result[Int, String]`, `tcp_
  connect_to(host, port): Result[Int, String]`, `Net.accept(fd): Result[Int, String]`, `Net.write(fd, data): Result[Int, String]`,
  `Net.read(fd, max_len): String`, `Net.close(fd): Unit` —
  blocking, fd-based (an `Int` is the connection). **Unix-only (Linux/
  macOS)**, same documented scope as extern-symbol-resolution has
  elsewhere (see DESIGN.md). `Net.read` is NUL-terminated (not binary-
  safe — fine for line-oriented protocols like HTTP, not arbitrary
  binary payloads) and returns `""` on both a clean peer-close and a
  hard socket error — a real, deliberate v1 scope trade, not a bug (see
  DESIGN.md's "TCP sockets" section for the full reasoning). UDP isn't
  in yet — deferred pending its own design for `recvfrom`'s sender-
  address problem.
- **`use Http;` — HTTP client.** `Http.get(url): Result[Http.Response, String]`,
  `Http.post(url, body): Result[Http.Response, String]`, and the general
  `Http.request(method, url, headers, body): Result[Http.Response,
  String]` (`headers: Array[Http.Header]`), where `Http.Response { status:
  Int, headers: Array[Http.Header], body: String }`. Built entirely on
  top of the TCP module above, no compiler magic. **`http://` only —
  `https://` is rejected with a clear `Err`**, not silently attempted;
  TLS is deliberately deferred as its own future design question. A
  `Transfer-Encoding` (chunked) response is also rejected with a clear
  `Err` rather than mis-parsed — only `Content-Length`-framed or
  read-until-close responses are supported. See DESIGN.md's "HTTP
  client" section for the full scope writeup.
- **`use Http;` — HTTP server.** `Http.serve_once(port, handler): Result[Unit,
  String]` (listens, handles exactly one connection, returns — a real
  one-shot server on its own) and `Http.serve(port, handler): Result
  [Unit, String]` (the real long-running server: accept, handle, close,
  repeat, forever). `handler: (Http.Request) -> Http.Response`, where
  `Http.Request { method: String, path: String, headers: Array
  [Http.Header], body: String }`. **Concurrent — spawn-per-connection**:
  `http_serve`/`http_serve_loop` spawn a real OS-thread task per
  accepted connection, so one slow client can't stall another behind
  it; `handler` must be a plain top-level function or a closure
  capturing nothing (a closure that closes over live local state can't
  cross the `spawn` boundary — a clean runtime abort). Every request
  gets exactly one response, connection always closed afterward (no
  keep-alive). See DESIGN.md's "HTTP server" and "Native-codegen
  zero-capture closure fix" sections for the full writeup, including a
  real request/response body-framing asymmetry bug found via an actual
  deadlock (a bodyless `GET` with no `Content-Length` means different
  things on the two sides of a connection).
- **`use Os;` — directory listing and subprocess exec.** `Os.list_dir(path): Result
  [Array[String], String]` (entry names, `.`/`..` already skipped),
  `Os.is_directory(path): Result[Bool, String]`, and `Os.run_process(program,
  args): Result[ProcessResult, String]` where `ProcessResult { exit_
  code: Int, stdout: String, stderr: String }` — a non-zero exit code
  is an ordinary `Ok`, `Err` only means the process could never even be
  started. Subprocess output is captured via temp files (not pipes),
  deliberately, to avoid the classic pipe-deadlock class of bug. See
  DESIGN.md's "OS module" section for the full writeup, including a
  real, SEPARATE bug found (and filed, not fixed here) while testing
  this: a top-level global `let` used twice with a heap-consuming
  operation like `.as_cstr()` corrupts the value.
- **Methods are namespaced functions.** `x.f(a)` means `T.f(x, a)`,
  where `T` is `x`'s type — so `"  hi  ".trim_end()` and
  `String.trim_end("  hi  ")` are the same call, and declaring
  `let Box.bump (b: Box) (n: Int): Box` makes `myBox.bump(1)` work.
  A namespace names the type of the first parameter, which is why
  `Os` and `Time` are modules rather than prelude namespaces — neither
  has a receiver, so neither has methods. A struct field holding a closure
  wins over a namespaced function of the same name.

- **`use Time;` — the clock, and the calendar on top of it.** The
  first standard-library MODULE rather than a prelude namespace: it
  names no type and nothing dispatches to it, so it is spelled like the
  module it is, and a file that does not `use` it does not get it.

  `Time.now(): Int` is seconds since the Unix epoch and is the only
  part that needs the runtime. Everything else is ordinary Plum over
  it: `Time.utc(epoch): DateTime` (a struct of `year`/`month`/`day`/
  `hour`/`minute`/`second`/`weekday`, with `weekday` 0 for Sunday),
  `Time.iso8601(epoch)` → `2026-08-27T01:40:28Z`, `Time.rfc7231(epoch)`
  → `Thu, 27 Aug 2026 01:40:28 GMT`, and `Time.weekday_name`/
  `Time.month_name`.

  UTC only — a timezone database is a data-shipping problem, and Plum
  ships no data. Dates before 1970 work: the arithmetic uses floor
  division rather than `/`, which truncates toward zero and would
  otherwise put the second before the epoch in 1970.

- **`use Os;` — filesystem and self-location.** A module for the same
  reason `Time` is: it names no type and nothing dispatches to it.
  `Os.temp_dir(): Result
  [String, String]` (a fresh private directory the caller owns and must
  clean up), `Os.make_dir(path)`, `Os.remove_file(path)`,
  `Os.remove_tree(path)`, `Os.copy_tree(src, dst)` (copies the CONTENTS
  of `src` into an existing `dst`), and `Os.self_exe(): Result[String,
  String]` for a program that re-invokes itself. All return
  `Result[Unit, String]` unless shown otherwise.

  **`Os.platform(): String`** is `"linux"`, `"macos"` or `"windows"` —
  compiled in rather than detected, since a binary cannot move between
  platforms. The compiler uses it to choose which libraries to link.

  `Os.remove_tree` removes symlinks rather than following them, so a
  link pointing outside the tree cannot cause deletions there.
  `Os.copy_tree` does not preserve file modes.

  These exist because the compiler used to shell out to `mktemp`, `rm`,
  `cp` and `mkdir` to do them, which is not portable — see
  [PORTING.md](PORTING.md).

All of the above are ordinary Plum source, not compiler magic — you
could write equivalents yourself. They currently live in a shared
prelude rather than real `use`-based modules; that's a known, deliberate
v1 simplification (see DESIGN.md), not a permanent design point.

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

## Status

Plum is a work in progress, and self-hosted: the compiler is written in
Plum, compiles itself to a byte-identical fixed point, and builds
without a Rust toolchain. The core language and LLVM backend are
substantially complete (scalars, control flow, closures, generics,
arrays, strings, concurrency, FFI), and there is one implementation of
all of it — the Rust one was retired on 2026-08-25.

Rebuilding a value — `Entity { x: e.x + 1, ..}` in a loop, or
`Array.map` over an array nothing else is holding — recycles the old
cell instead of allocating a new one, so an update loop that reads as
pure allocates a constant number of times rather than once per
iteration. It applies to structs and enums whatever they
hold — including `String` and `Array` fields and payloads — to arrays
under `.push()` and `Array.map`, and to strings under `.concat()`. It
does not apply to `Array.filter`, or to mapping an array whose elements
are themselves references.

Literals that cannot differ between evaluations are not allocated at
all. A closure that captures nothing is the same three words every
time; so is `[1, 2, 3]`, `Point { x: 1, y: 2 }` or `None`. Each becomes
a module-level constant, so `Array.map(xs, |v| v + 1)` in a
thousand-iteration loop allocates twice in total, and a loop body full
of constant literals allocates none.

Nothing about this is visible in the source: reuse is a runtime check on
whether anything else can see the value, so it is never wrong, only
sometimes unavailable.

What is actually checked, rather than claimed: 74 corpus fixtures under
AddressSanitizer with leak detection, 102 lexer/parser goldens, 11
property tests, recorded allocation counts for ten memory-model
fixtures, every project in `examples/` against its recorded output, and
a real language-server session — on Linux x86_64 and arm64, macOS, and
Windows. Running `./bootstrap/` is the honest answer to "what works";
no list kept by hand is. See DESIGN.md for the full history.
