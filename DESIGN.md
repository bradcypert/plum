# Plum Design Document

This is the living record of decisions made about Plum and the reasoning
behind them. See `VISION.md` for the short pitch. This document exists so
future discussion doesn't have to re-derive reasoning that's already been
worked through.

Status markers: **Decided** (working assumption, revisit only with a
concrete reason), **Leaning** (recommended direction, not yet exercised
against real code), **Open** (genuinely unresolved).

`GRAMMAR.md` is the formal EBNF grammar derived from the syntax
decisions below — the source of truth to implement `plum-syntax`'s
lexer/parser against. This document is where the *reasoning* behind
each grammar choice lives; if the two ever disagree, that's a bug to
fix, not a choice to make.

## Scope and target use cases — Decided

Plum is a general-purpose ML-style language, not a systems/embedded
language first. Primary targets: web APIs, compiling to WASM, and games
(e.g. using Raylib-style bindings). Embedded/bare-metal hardware
(Raspberry Pi, RISC-V microcontrollers) is a welcome side effect of the
memory model, not a headline goal — it's fine if it's never fully
pursued.

This was a real pivot point in the design conversation: the original
framing leaned on "systems language" and embedded reach as the core
differentiator. That framing got dropped in favor of a sharper one: the
memory model is justified by *frame-time predictability for games* (GC
pauses are a well-known pain point in GC'd game engines, e.g. Unity's C#
GC) and *clean C FFI* (see below), independent of whether Plum ever runs
on a microcontroller. This is a stronger, more defensible reason to stay
GC-free than the embedded story alone was.

## Why Plum, not OCaml/F#/Roc/Austral — Decided (see VISION.md)

The differentiator is occupying an empty point in a triangle: GC
languages (ergonomic, unpredictable pauses, FFI friction) vs.
manually-proven languages like Rust (full control, but the programmer is
the proof engine) vs. automatic-but-not-GC (the empty middle). Roc has
the memory model without the target audience (app/scripting, not
hardware/games). Austral has the target audience without the automatic
model (exposes linear types, so it's manual proof with different syntax).
Full reasoning in `VISION.md`.

**This is the thing to protect going forward**: any design choice that
makes tracing GC the *primary* mechanism, or that reintroduces FFI
boundary friction for ordinary values, undermines the actual reason to
build this language instead of just using OCaml.

## Memory model

### Core mechanism — Decided

Reference counting with compile-time reuse analysis, following the
**Perceus** algorithm (Leijen et al.; used in Koka and Lean 4's
compiler) — not a vague "Roc-style" gesture, a specific published
technique:

- **Unboxed primitives.** `Int`, `Bool`, `Float`, `Unit` carry no header
  and no refcount traffic. Only heap-allocated structured values
  (records, variant payloads, arrays, strings, closures) are refcounted.
- **Precise, static refcounting.** The compiler performs a last-use
  analysis over the IR: at a variable's final use, insert a `drop`; at
  any point a value is used more than once or escapes its scope, insert
  a `dup` (increment). There is no runtime traversal — every inc/dec is a
  specific instruction placed at compile time. This is the job of the
  `fbip` pass stubbed in `crates/plum-ir/src/fbip.rs`.
- **FBIP (functional but in place) is the reuse half of the same pass.**
  When a `match` deconstructs a value and the same branch constructs a
  new value of matching size/shape, and the scrutinee's refcount is 1
  (proven statically, or checked with a single runtime `if rc == 1`
  branch when it can't be), the compiler reuses that memory in place
  instead of allocating. Worst case is one branch, not a silent fallback
  to always-allocate.
- **Refcounts are non-atomic by default.** Cheap increment/decrement,
  matches the single-threaded-by-default execution model (see
  Concurrency below). Thread-shared values use an explicit atomic-backed
  type instead of paying atomics everywhere.

### Mutability and cycles — Decided

Immutable by default; FBIP gives in-place performance under the hood for
the common single-owner case (this covers most ordinary code: request
handling, data transforms, tree-shaped data). This is **not** sufficient
for graph-shaped, genuinely shared mutable state (entity/component
references, scene graphs, caches) — FBIP's uniqueness analysis has
nothing to exploit when there's no single owner.

Resolution: an explicit, opt-in mutable-reference/shared type (working
name: `Ref[T]` or `Shared[T]`, TBD when we design the type system)
for the cases that need real sharing — same shape as OCaml's `ref`,
Rust's `Cell`/`RefCell`, Swift's reference types. Reference cycles are
only possible through this explicit type, not through ordinary values.

### Cycle collection — Leaning, deliberately deferred

Two live options, in order of what we're actually doing:

1. **No collector; `Weak` references by convention** (Swift's actual
   choice — ARC + `weak`/`unowned`, no cycle collector at all). Cheapest
   to build, keeps the "no GC" pitch fully intact, keeps FFI totally
   clean. Known cost: retain-cycle leaks are a real, recurring bug class
   in Swift when `weak` isn't used correctly — accepted as a known
   rough edge for now rather than solved preemptively.
2. **A scoped, incremental, budget-bounded cycle collector that only
   walks the subgraph of explicit `Ref`/`Shared` values** (CPython's
   model: refcounting everywhere, a generational cycle collector as
   backup, scoped to container objects that can actually form cycles).
   This is the answer to "can a GC help ergonomics without giving up the
   pitch" — it can, but only if kept scoped to the explicit shared type,
   never the whole heap. Because it only ever walks that subgraph, it can
   have a genuine tunable time budget (e.g. "do at most 0.5ms of
   collection work this frame") — this is what "a GC with the ability to
   influence it" concretely looks like. Would need to be gated as a
   hosted-target feature so bare-metal builds can skip it.

**Decision: build option 1 first.** Don't build the second collection
mechanism speculatively — validate that entity-graph/cycle pain is a
real problem once real Plum code exists (e.g. an actual small game),
then add the scoped collector if it's warranted. Adding it later doesn't
require revisiting anything else in this document.

A full tracing GC as the *primary* mechanism (OCaml/Go-style) was
considered and rejected: it would reintroduce FFI boundary friction for
every value crossing the C ABI (this is exactly why OCaml's FFI needs
`CAMLparam`/`CAMLlocal` root registration), and it would collapse the
core differentiation argument for Plum existing at all.

## Concurrency

### Model — Decided

Go-inspired: lightweight tasks (goroutine-equivalent) + typed channels +
`select`-style multiplexing, as the ergonomic baseline to match or beat.
Explicitly open to innovating beyond Go, not just copying it.

`spawn { block }` starts a task running `block` concurrently; the
expression itself evaluates to a task handle (shape TBD — at minimum
something joinable, `Task[T]` where `T` is the block's result type).
`channel[T]()` (already sketched under Generics syntax) creates a
typed, bounded-or-unbounded-TBD channel and returns `(tx, rx)` — a
`Sender[T]`/`Receiver[T]` pair, matching the `let (tx, rx) =
channel[Int]()` example already on record. Concrete operations, naming
still open (bikeshed, not blocking): something like `tx.send(v)` /
`rx.recv()`, plus a `select { rx1.recv() => ..., rx2.recv() => ... }`
form for multiplexing over several channels at once — exact surface
syntax for `select` is deferred until it's actually being implemented,
not needed to lock the model.

### How this interacts with non-atomic refcounts — Leaning

Go's own proverb — "share memory by communicating, don't communicate by
sharing memory" — is a move discipline in spirit, but Go doesn't enforce
it; you can still pass a raw pointer between goroutines and race on it,
which is why Go ships a separate race detector rather than catching it
at compile time.

Plum's angle: **channel send is a move**, using the same last-use
ownership analysis the FBIP pass already computes. Sending a value on a
channel transfers ownership; the compiler treats that as the value's
last use, so using it again afterward is a compile error, not a runtime
race. Because ownership transfers rather than being shared, there's
never concurrent access to a single non-atomic refcount, so the cheap
non-atomic default holds without extra runtime cost for the common case.

This is a genuine claim of improvement over Go, not just parity: Go's
compiler does not stop you from racing on shared memory (that's what the
race detector is for); Plum's could, structurally, for the default
(non-shared) type.

For cases that want genuine concurrent shared access (a read-heavy
cache, config data touched by many tasks) — an explicit,
atomic-refcounted wrapper type (an `Arc` equivalent) is the escape
hatch, matching Rust's `Rc`/`Arc` split: pay for atomics only where
sharing is real and intentional.

### Scheduler — Open, deliberately sequenced last

A Go-style M:N scheduler (cheap green threads, growable stacks,
preemption) is a large, standalone runtime engineering project on the
order of what Go's own runtime team built — not something to solve
alongside the type system and memory model. Plan: ship the user-facing
model (spawn a task, channels, `select`) on top of plain OS threads
first (heavier per-task, semantically identical to the eventual model),
and treat a real green-thread scheduler as a later performance upgrade,
not a v1 requirement.

### Implementation blocker: heap ownership across tasks — Open

Found while scoping `spawn`'s lowering (2026-07-27): the CURRENT
tree-walking interpreter (`plum-interp`) gives each `Interpreter` its
own single, non-atomic-refcounted `Heap` (a plain `Vec` of cells,
addressed by a bare `usize`). A `Value::HeapRef(addr)` is only
meaningful within the `Heap` that allocated it. If `spawn` ran a block
on a real OS thread with its own `Interpreter` (the natural reading of
"OS threads first" above), sending a struct/enum value across a channel
to a DIFFERENT thread wouldn't resolve — the address wouldn't exist in
the receiving thread's heap at all. This is a genuinely unresolved
question, not just unimplemented plumbing, and it's serious enough that
none of the three options below should be picked reflexively:

- **Deep-copy heap values on channel send.** Simplest to reason about,
  keeps "non-atomic by default" fully intact (each heap genuinely never
  sees concurrent access), but means "move" semantics for channel send
  (see above) would need to become "copy" for anything heap-shaped,
  which undercuts the compile-time race-freedom claim resting on
  ownership TRANSFER rather than duplication — needs real thought
  before committing.
- **A genuinely shared heap for values that cross tasks.** Closer to
  true move semantics (no copy), but reintroduces exactly the
  concurrent-access-to-a-refcount problem the non-atomic-by-default
  design was built to avoid for the COMMON case — would need every
  cross-task-reachable value to opt into atomic refcounting somehow,
  raising the question of how that's tracked/enforced.
- **Restrict channels to non-heap (primitive) values for a first cut**,
  deferring the real answer entirely. Fast to ship, but doesn't
  validate the design's actual hard part, and callers would hit a wall
  the moment they try to send anything struct/enum-shaped.

Not resolving this now. `spawn`'s lowering keeps erroring loudly at
`lower.rs` (see `lower_for`'s sibling `Expr::Spawn` case) — the error
message should be updated to point at THIS blocker specifically, not
just "concurrency model is undecided," since the model itself is now
Decided above; what's actually missing is this.

## Syntax and surface semantics

### Load-bearing ML semantics — Decided

These are the actual value from the ML family, independent of surface
syntax, and are non-negotiable:

- Expression-orientation: blocks, `match`, everything produces a value;
  no statement/expression split.
- Algebraic data types + exhaustive pattern matching as the primary
  control-flow tool.
- Type inference (Hindley-Milner base) doing the annotation work.
- `Result`-based errors as data, not exceptions, as the primary
  mechanism (unwinding machinery is unnecessary complexity against
  refcount cleanup and a poor fit for constrained targets). Panics
  reserved for unrecoverable aborts only.

### Surface syntax — Leaning, revised after first sketch

First pass leaned all the way toward Rust's surface (`fn`, `::`,
mandatory annotations) on top of ML semantics. Reaction after actually
reading the sketch (`examples/overview.plum`, first draft): it read as
"Rust with renamed types," not ML — because the paradigm-level things
carried over from ML (ADTs, exhaustive matching, expression-orientation)
are things Rust *already* has, so they don't register as different on
the page even though they matter. The real, visible tells were
elsewhere, and got corrected:

- **`let` instead of `fn` for function definitions.** Functions are
  values; the binding syntax should say so, the way OCaml/F# do. `fn` is
  one of the most visually "this is Rust" markers there is.
- **`.` instead of `::` for both value field access and type/variant
  access** (`Shape.Circle`, not `Shape::Circle`). Rust's `::`/`.` split
  is inherited from C++ and isn't load-bearing — OCaml itself uses `.`
  for module access (`List.map`). Capitalization already distinguishes
  types/modules from values, so the parser doesn't need two operators to
  disambiguate. This left a wrinkle unresolved at the time: explicit
  generic type arguments at a call site would seem to need something
  like Rust's `::<T>` "turbofish" to avoid `<`/`>` parsing ambiguity with
  comparison operators. Resolved later (see "Generics and type parameter
  syntax" below) by using `[T]` instead of `<T>` for generics entirely,
  which sidesteps the ambiguity rather than routing around it.
- **Type annotations are optional, not mandatory** — see the dedicated
  section below. This was the single biggest lever: `let sum n acc = ...`
  reads completely differently from `fn sum(n: Int, acc: Int) -> Int`.
- A pipe operator (`|>`) is being added as a purely additive ML idiom
  for left-to-right call composition — Rust has nothing equivalent, it
  costs nothing to add, and it's one of the most recognizable ML-family
  idioms.

Kept as-is, because the reaction was specifically to the annotation/
keyword surface, not to these: `enum` for sum types, `struct` for
records (both already mean exactly what they'd mean in an ML language —
no reason to invent new vocabulary), Rust's `..` struct-update spread
syntax, braces for blocks, `match` as an expression, ML-flavored
built-in type names (`Int`/`Float`/`Bool`/`String`/`Unit`, not Rust's
sized-integer zoo), the `Ref[T]` shared-mutable type with
`.get()`/`.set()`/`.update(|old| new)`, `extern "C" { }` + `unsafe { }`
for FFI, and `spawn { }` + `channel[T]()` with move-on-send.

**Deliberately deferred, not adopted**: full ML-style currying by
default (`add 5` partially applying to return a function). Real ML
flavor, but it has implementation weight — it needs a fully-applied
direct-call fast path plus a closure-allocating path for partial
application, touching the calling convention and interacting with FBIP.
Revisit later rather than fold in by default now.

### Local mutability — Decided

Plum has a local `let mut` for genuinely non-escaping mutable variables
(the classic `for`-loop-with-an-accumulator case), alongside `Ref` for
anything that's actually shared or escapes. FBIP's uniqueness analysis
can prove a non-escaping local is exactly as safe as recursion, so
disallowing it would be purity for its own sake, not a real safety win.
The two mechanisms stay non-overlapping by construction: `let mut`
never escapes the function it's declared in; `Ref` is for anything that
does.

### Type annotations — Decided

Full type inference (Hindley-Milner base); explicit annotations are
optional everywhere they'd apply to inferred code, used for
documentation or disambiguation rather than required by the compiler:

```
let move_right p dx = Point { x: p.x + dx, ..p }
let move_right (p: Point) (dx: Float): Point = Point { x: p.x + dx, ..p }
```

both valid, both equivalent. Recommended (soft convention, possibly a
future linter nudge — not a compiler requirement) for top-level/exported
functions to carry explicit signatures anyway: without one, a public
function's inferred type can shift silently when its implementation
changes, quietly breaking callers who depended on the old signature.
Annotating the boundary pins the public API on purpose; internals stay
fully inferred.

Two places annotations stay **mandatory**, not optional: `struct` field
declarations and `extern "C"` FFI signatures — both are interface
declarations with no implementation body for inference to work from.

### Option and Result types — Decided

No null, anywhere, ever. Absence is `Option[T] = Some(T) | None`;
recoverable failure is `Result[T, E]` (already decided under "Load-
bearing ML semantics" above) — same family, same reasoning: represent
absence and failure as ordinary data via pattern matching, not as
sentinel values or control flow.

### Generics and type parameter syntax — Decided

User-defined types and functions are generic, the same as the built-in
`Option[T]`/`Result[T, E]`/`Ref[T]` already are — plain Hindley-Milner
gives unconstrained generics for free (`let identity[T] x = x` needs no
bounds, no typeclass machinery).

Generic parameters use **square brackets**, not angle brackets, in both
declaration and call position — `struct Pair[T] { ... }`,
`let identity[T] (x: T): T = x`, `channel[Int]()` — and this replaces
Rust's `<T>`/`::<T>` split entirely rather than adapting it. The reason:
Rust needs the awkward `::<T>` "turbofish" specifically because `<`/`>`
are already binary comparison operators in expression position, so a
bare `channel<Int>()` is ambiguous with a chain of less-than/greater-
than comparisons. `[`/`]` are never binary operators in any position, so
the ambiguity doesn't exist to route around in the first place — this
removes a whole "which syntax do I use here, declaration or call site"
question rather than answering it with a special case.

This does introduce one thing to disambiguate once arrays/indexing
exist: `x[0]` (indexing) vs. `Thing[T]` (generic instantiation) both use
brackets. Resolved by the same rule already load-bearing for `.`
(`Shape.Circle` vs. `p.x`): capitalization distinguishes type-level
names from value-level ones, so `channel[Int]()` (capitalized `Int`) is
generic instantiation and `arr[i]` (lowercase `i`) is indexing. No new
disambiguation machinery — the existing convention just applies here
too.

Bounds on a type parameter use `[T: Num]`, combined bounds use `+`
(`[T: Num + Eq]`), matching Rust's convention. Because the trait set is
small and closed (`Num`/`Eq`/`Show`, no user-definable traits — see
below), checking a bound is closed-set membership checking, not general
typeclass resolution.

### Ad-hoc polymorphism (v1) — Decided

A small, fixed, compiler-known set of overloadable traits (`Num`, `Eq`,
`Show`). No user-definable typeclasses/traits yet — deferred to keep the
type system's complexity budget on the memory model, not generics
machinery.

### Operator precedence and pipe semantics — Decided

Tightest-binding first:

1. `.` (field/module access), function calls, indexing — postfix,
   left-to-right
2. Unary `-`, `!` — prefix
3. `*`, `/`, `%`
4. `+`, `-`
5. `..` (range) — binds looser than arithmetic, so `0..n+1` parses as
   `0..(n+1)`. **Decided**: `..` is exclusive of its end, matching
   Rust's `Range` (`for i in 0..n` runs `n` times, over `0..=n-1`) —
   there's no `..=` form. Currently the only place a range means
   anything at all is as a `for` loop's iterable (`lower.rs` rejects a
   bare range anywhere else); ranges aren't a first-class value.
6. `==`, `!=`, `<`, `>`, `<=`, `>=` — **non-associative**, no chaining
   without parens (`a < b < c` is a compile error, not "true if both
   hold") — mirrors Rust's rule specifically because silent
   mis-chaining is a documented footgun in languages that allow it
7. `&&`
8. `||`
9. `|>` — lowest, left-associative

Assignment (`total = total + i` inside a `let mut` block) is
deliberately **not** part of this table — it's a statement production,
not a general expression, unlike Rust's assignment-as-expression. Not
needed for anything sketched so far, and leaving it out avoids an entire
category of precedence questions (e.g. chained assignment) for no loss.

**Pipe desugaring**: `x |> rhs` inserts `x` as the *last* positional
argument of the call `rhs` denotes; a bare identifier with no parens is
treated as a zero-argument call before insertion, so `x |> f` and
`x |> f()` both mean `f(x)`, and `x |> f(a, b)` means `f(a, b, x)`. This
is a compile-time syntactic rewrite, not a runtime capability — it does
not require currying (deferred, see Surface syntax above) to work, and
it produces exactly the same result currying would if added later, so
adopting it now costs nothing and won't need revisiting.

### Block statement/expression rule — Decided

A block is a sequence of items followed optionally by a tail expression:
`{ item; item; ...; tail }`. Every item before the tail requires a
trailing `;` (a `let`/`let mut` binding, or an expression whose value is
discarded) — **no exceptions**, including for `if`/`match`/loop-shaped
expressions used as statements. The tail item, if it has no trailing
`;`, is the block's value; with a trailing `;`, or an empty block, the
value is `Unit`.

This deliberately drops Rust's semicolon exemption for block-shaped
statements (Rust doesn't require `;` after `if cond { ... }` used as a
mid-block statement, because the closing brace already reads as a
boundary). That exemption is a well-documented, recurring point of
confusion even for working Rust programmers, and it exists for
convenience, not necessity — dropping it costs a small amount of visual
noise in the comparatively rare case of a purely side-effecting
conditional (`if should_log { log(msg) };`), in exchange for one rule
that applies uniformly to every expression shape with nothing to
memorize about exemptions. Consistent with expression-orientation being
the default anyway (most `if`/`match` usage is in tail position or bound
via `let`, not discarded).

### Pattern grammar — Decided

Building out from the enum-variant patterns already in use
(`Shape.Circle(r)`):

- Literals (`0`, `"hi"`, `true`), variable bindings (`x`, irrefutable),
  and wildcard (`_`).
- Tuple patterns: `(a, b)` — already implied by
  `let (tx, rx) = channel[Int]()`.
- Struct patterns: `Point { x, y }` (shorthand, binds fields to
  same-named variables), `Point { x: px, y: py }` (rename while
  destructuring), `Point { x, .. }` (ignore remaining fields).
- Guards: `pattern if condition => expr`.
- Or-patterns: `Shape.Square(s) | Shape.Rectangle(s, s) => ...` — one
  arm covering multiple shapes.
- Patterns nest arbitrarily (a struct pattern inside a tuple inside an
  enum variant, etc.) — the rule composes, no need to enumerate every
  combination.

Note that `..` now does three distinct jobs depending on grammatical
position — functional struct update (`Point { x: 1.0, ..p }`), ranges
(`0..n`), and "ignore remaining fields" in a pattern (`Point { x, .. }`).
Each is unambiguous by context; this is the same triple-duty Rust
already gives `..`, not a new idea introduced here.

Because `Option[T]`/`Result[T, E]` are ordinary enums under the hood,
`Some(x)`, `None`, `Ok(v)`, `Err(e)` all fall directly out of the
enum-variant pattern rule — nothing type-specific needed for them.

### Tuples and closures — Decided

Tuple types are `(T1, T2, ...)`, values are `(v1, v2, ...)`. `()` is
`Unit`'s only value — which explains, retroactively, a piece of syntax
already used in the examples: `let example () = ...` takes one argument
of type `Unit`, written as the pattern `()`, matching Rust's convention
exactly. Disambiguating a parenthesized expression from a one-element
tuple: `(x)` is just `x`; a genuine one-element tuple needs a trailing
comma, `(x,)` — standard, matches Rust and Python both.

Closures are `|params| expr` or `|params| { block }`, comma-separated
params, optionally annotated (`|p: Point| ...`) the same way top-level
`let` bindings are. One concrete payoff of the memory model shows up
here: **no `move` keyword is needed**, unlike Rust — Rust's `move`
exists to let the programmer choose between borrowing and moving a
capture, a distinction that only exists because of the borrow checker.
Plum doesn't expose borrows at the surface at all, so captures just
work, with nothing to choose.

### Effect/unsafe tracking — Leaning

A lightweight `unsafe`/`extern` marker that propagates from FFI call
sites, not a full Koka-style effect system. Full effect tracking is its
own multi-year research project layered on an already-ambitious
language; scoped down to just marking the FFI trust boundary for v1.

## Module system — Decided

Synthesized from three influences, each solving a different part of the
problem — not a copy of any one of them:

- **From Go**: a directory *is* a module, automatically. Every file in
  a directory shares one namespace with no declaration required — no
  Rust-style `mod foo;` line anywhere announcing a submodule exists.
  Subdirectories become nested child modules, discovered from the file
  tree, not declared in source. File order within a directory never
  matters (no forward-declaration concerns). No `crate` keyword either —
  the project root is just the top-level module implicitly, so paths
  read the same everywhere.
- **From F#**: scope only, not power — no functors, no signature-based
  module algebra (see "Why Plum, not OCaml" — this is squarely in the
  territory we already declined). Also collapses F#'s own
  namespace/module split into one concept (`mod`), since that split is a
  known source of confusion even within F# itself and Plum doesn't need
  its .NET-interop justification.
- **From Rust**: `pub` as an explicit, per-item visibility keyword —
  private by default, `pub` opts a `let` binding, `struct`, `enum`, or
  individual struct field into visibility. `pub use` for re-exports, so
  a module's public shape doesn't have to mirror its file layout.

**`use` is qualify-by-default (Go-style), not bare-import-by-default
(Rust-style)**: `use shapes;` brings the module in, and every reference
keeps the module name attached (`shapes.Circle`, `shapes.area`) — every
call site is self-explanatory without cross-referencing the `use` lines,
which matters for an explicitly "accessible" language read by
newcomers. A bare/aliased import of one specific name (`use shapes.Circle;`)
is available as an escape hatch for names used constantly in a file, but
it's the exception, not the everyday habit. Core types (`Option`,
`Result`, `Some`, `None`, `Ok`, `Err`) live in an always-available
prelude needing no `use` at all, same as Rust's prelude.

No first-class modules-as-values, matching the no-functors decision —
deferred, revisit only if a concrete need surfaces.

Example layout:

```
myapp/
  main.plum
  shapes/
    circle.plum
    rectangle.plum   // both files are just the `shapes` module —
                      // no per-file boundary, same as a Go package
  net/
    http.plum         // nested module: myapp.net.http
```

```
// shapes/circle.plum
pub struct Circle { radius: Float }
pub let area c = 3.14159 * c.radius * c.radius
let internal_helper c = c.radius * 2.0   // private, no `pub`
```

```
// main.plum
use shapes;
let go () = shapes.Circle { radius: 2.0 } |> shapes.area |> print
```

## FFI and C interop

- Calling **into** existing C libraries matters more early than being
  called **from** C/other languages, though both are goals.
- `extern` blocks declare foreign signatures with explicit C-ABI types.
  No implicit string/allocation coercion at the boundary — that would
  hide allocation/lifetime decisions exactly where they need to be
  visible.
- A `#[repr(C)]`-equivalent for structs that cross the boundary, since
  Plum's native structs may carry a refcount header or different field
  ordering than C expects.
- Callbacks: C APIs often want bare function pointers. Plum closures
  that capture an environment can't be handed to C directly without a
  trampoline. Practical answer (same as Rust): only non-capturing
  closures convert directly to C function pointers; capturing ones need
  explicit adapter machinery. Not yet designed in detail.
- Because refcounting (not tracing GC) is the primary mechanism, values
  crossing the FFI boundary don't need root registration the way OCaml's
  GC-tracked values do — this is a concrete, structural advantage over
  OCaml's FFI story, not just a claim.

## Target platforms

- **Hosted (Linux/macOS/Windows), web APIs**: primary target, no special
  constraints.
- **WASM**: a named goal (compile-to-WASM). Not yet designed in detail —
  open question whether Plum ships its own allocator inside WASM linear
  memory (like Go, AssemblyScript) or leans on WASM's GC proposal as it
  matures. Given the memory model is refcounting, not tracing GC, Plum
  likely doesn't need the WASM GC proposal at all — worth confirming
  when WASM codegen is actually built.
- **Raspberry Pi**: effectively free once an LLVM backend exists — it's
  a normal Linux/ARM cross-compile target, not a distinct design
  problem.
- **Embedded/RISC-V microcontrollers (e.g. ESP32-C3/C6/H2)**:
  aspirational, not primary. Original ESP32 (Xtensa) needs Espressif's
  LLVM fork and is deprioritized; RISC-V variants work with mainline
  LLVM. The cycle-collector-for-`Shared` (if ever built) would need to
  be gated off for these targets.

## Implementation plan

- **Bootstrap language**: Rust, edition 2024.
- **Parser strategy**: hand-written recursive descent, not a
  parser-generator crate (`lalrpop`/`pest`). A generated parser collapses
  spec and implementation into one artifact (the grammar file becomes
  the parser, zero drift risk), which is appealing given how much this
  document exists to prevent drift — but it trades away control over
  error messages and recovery, which matters more for a language
  explicitly aiming to be accessible than it would for a research
  language. `GRAMMAR.md`'s "Expressions" section is already shaped for
  this: the precedence hierarchy is written as the exact call chain a
  recursive-descent parser implements, one function per precedence
  level.
- **Backend**: LLVM, targeting the C ABI directly — not compiling
  through C source. C gives no reliable tail-call guarantee (GCC/Clang
  will sometimes eliminate self-tail-calls under optimization, but it's
  not guaranteed, and mutual tail recursion routinely isn't eliminated),
  which matters because ML-style code leans on recursion as the primary
  looping construct. LLVM IR has a first-class `tail call` instruction
  that's a portable guarantee.
- **Sequencing**: validate the memory model (refcount insertion + FBIP
  reuse analysis) on a simplified typed IR using a tree-walking
  interpreter *before* investing in the LLVM backend. The risky,
  unproven part of the design is the memory model, not codegen — de-risk
  it cheaply first. Current repo structure:

  ```
  crates/
    plum-syntax/   lexer, parser, AST (parser is currently a stub)
    plum-ir/       typed IR, AST-to-IR lowering, FBIP pass (stubbed)
    plum-interp/   tree-walking interpreter over the IR
    plumc/         CLI binary wiring the pipeline together
  ```

  Once the memory model's algorithm is validated by the interpreter, the
  backend swaps from "interpret the IR" to "codegen the IR via LLVM" —
  the frontend and refcount-insertion pass should barely change.

## Open questions (not yet decided, flagged so we don't forget them)

- Exact design of the `Ref`/`Shared` mutable type and its interaction
  with pattern matching and FBIP.
- Array/string growth strategy under FBIP (capacity headers, in-place
  realloc when uniquely owned) — conceptually similar to `Vec`/`String`
  in Rust, but inferred rather than explicit `&mut`. Includes literal
  syntax for arrays/lists (not yet decided — deliberately avoided in
  `examples/overview.plum`'s closure example to avoid sneaking in an
  undecided feature) and standard collection operations like `map`.
- Recursive closures that capture themselves (named top-level recursive
  functions should compile to direct calls, sidestepping this; true
  anonymous self-referential closures are a deferred detail).
- Standard library scope.
- Whether/when to build the scoped incremental cycle collector for
  `Shared` values (see Memory model above — deliberately deferred until
  real Plum code shows the pain is real).
- Whether Plum curries function application by default (ML-family
  norm), deferred because of its interaction with the calling
  convention and FBIP (see Surface syntax above; note the `|>` pipe
  desugaring does NOT depend on this being resolved).
