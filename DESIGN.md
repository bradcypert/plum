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

Resolution: an explicit, opt-in mutable-reference/shared type — same
shape as OCaml's `ref`, Rust's `Cell`/`RefCell`, Swift's reference
types. Reference cycles are only possible through this explicit type,
not through ordinary values.

**Naming and surface syntax — Decided (2026-07-28).** `Ref[T]`, not
`Shared[T]` — `Shared` was rejected specifically because the word
already means something else, precisely, in this codebase (a
refcounted heap cell whose count is `> 1`, i.e. NOT safe to reuse-in-
place — see `fbip.rs`/the "Core mechanism" section above), and
overloading it for the type name would be confusing every time both
concepts come up in the same sentence. Construction is `ref(v)` — a
PLAIN lowercase function call, `T` inferred directly from `v` (no
explicit type argument needed the way `channel[T]()` needs one, since
`ref` always starts from a real value). This needed zero new parser
grammar: a bare-Ident call already parses; the ONLY new piece was a
shape-detection check (in both `lower.rs` and `infer.rs`) recognizing
a callee literally named `ref`, checked BEFORE the general variant-
construction fallback — same precedent `channel[T]()`'s own bare-Ident
special-case already established. Reading is `.get()` (returns a
COPY of the current contents); writing is `.set(v)` (UNCONDITIONALLY
overwrites the cell, visible through every other handle to the same
cell, evaluates to `Unit`) — the first genuinely imperative, always-
visible mutation primitive in the language beyond `let mut`'s own
`Assign`, deliberately NOT following the "returns a new value"
functional-update convention every other mutating-looking method
(`.push()`, arrays' own `.set()`, `.concat()`, etc.) uses.

**Representation and FBIP/pattern-matching interaction — Decided
(2026-07-28), resolving DESIGN.md's own longstanding open question.**
`Value::Ref(RefHandle)` where `RefHandle(Rc<RefCell<Value>>)` — Rust's
OWN `Rc`/`RefCell` give real multi-owner shared-mutation semantics
natively, DELIBERATELY kept entirely OUTSIDE the toy refcounted `Heap`
(`heap.rs`): that heap's `CtorReuse`/`dec_and_maybe_reuse`-style "maybe
copy, maybe reuse based on refcount" logic is exactly backwards for a
`Ref` cell, which must ALWAYS mutate in place and ALWAYS stay visible
through every alias, unconditionally — never conditionally reused,
never conditionally copied. This puts `Ref` in the exact same category
as `Task`/`Sender`/`Receiver`: an opaque runtime handle FBIP never
tracks or reuses at all — `fbip.rs` needed ONLY exhaustive-match
passthrough cases for the three new IR nodes (`RefNew`/`RefGet`/
`RefSet`), no `is_syntactically_heap` case (unlike `Ctor`/`Str`), and
`mark_reuse` never touches them. This directly ANSWERS the "interaction
with pattern matching" half of the original open question too: there
isn't one — `Ref` is not directly pattern-matchable, same as `Task`/
`Sender`/`Receiver` aren't today (no `Ctor` shape to match against);
`match r.get() { ... }` is how you'd match on a `Ref`'s contents.
Equality (`==`) is IDENTITY (`Rc::ptr_eq`), matching how `Task`/
`Sender`/`Receiver` already implement `PartialEq` — two DIFFERENT
cells holding equal contents are NOT `==`, which is genuinely what
makes `Ref` useful for aliasing in the first place.

**Concurrency boundary — Decided (2026-07-28).** A plain (non-atomic)
`Rc` isn't `Send`, so `Ref` crossing a `spawn`/`channel` boundary is a
clear, reported error (`to_portable` rejects it, same as it already
rejects closures/bare function values/task handles) — NOT a silent
deep-copy, which would quietly defeat the entire point of `Ref`'s
shared-mutation semantics by splitting one cell into two independent
ones without any warning. Real cross-thread shared mutation would need
atomic refcounts (`Arc`) and real synchronization (`Mutex`), which
DESIGN.md's own concurrency section ("How this interacts with non-
atomic refcounts") already flags as a separate, deliberately deferred
question — this chunk didn't try to solve it, just made sure the
current limitation fails loudly instead of silently.

**A real, pre-existing bug caught and fixed while testing, not Ref-
specific:** `struct Counter { value: Ref[Int] }` (a builtin opaque type
used in a STRUCT FIELD declaration) initially failed with "type
inference not yet implemented for this type annotation" — `ast_type_
to_type` (the OTHER `ast::Type -> Type` converter, used for struct/enum
field declarations and generic type arguments, as opposed to `resolve_
annotation`, used for function param/return annotations) had never
received the same opaque-pseudo-generic-builtin-types fixed-arity-one
check `resolve_annotation` already got when THAT gap was closed
earlier. This affected `Array[T]`/`Task[T]`/`Sender[T]`/`Receiver[T]`
too, not just `Ref[T]` — none of the four could ever have been used as
a struct/enum field's type before this fix, a real, previously-unnoticed
gap now closed for all four at once.

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

### Implementation blocker: heap ownership across tasks — Decided

Found while scoping `spawn`'s lowering (2026-07-27): the CURRENT
tree-walking interpreter (`plum-interp`) gives each `Interpreter` its
own single, non-atomic-refcounted `Heap` (a plain `Vec` of cells,
addressed by a bare `usize`). A `Value::HeapRef(addr)` is only
meaningful within the `Heap` that allocated it. If `spawn` ran a block
on a real OS thread with its own `Interpreter` (the natural reading of
"OS threads first" above), sending a struct/enum value across a channel
to a DIFFERENT thread wouldn't resolve — the address wouldn't exist in
the receiving thread's heap at all. Three options were weighed
(2026-07-28):

- **Deep-copy heap values on channel send.** Simplest to reason about,
  keeps "non-atomic by default" fully intact (each heap genuinely never
  sees concurrent access), but means "move" semantics for channel send
  (see above) would need to become "copy" for anything heap-shaped,
  which undercuts the compile-time race-freedom claim resting on
  ownership TRANSFER rather than duplication.
- **A genuinely shared heap for values that cross tasks.** Closer to
  true move semantics (no copy), but reintroduces exactly the
  concurrent-access-to-a-refcount problem the non-atomic-by-default
  design was built to avoid for the COMMON case — would need every
  cross-task-reachable value to opt into atomic refcounting somehow
  (a Send/Sync-style marker Plum has no precedent for — the only
  existing trait mechanism is the small, closed `Num`/`Eq`/`Show` set),
  raising the question of how that's tracked/enforced. Substantial,
  currently-undesigned work with no scaffolding to build on yet.
- **Restrict channels to non-heap (primitive) values for a first cut**,
  deferring the real answer entirely. Fast to ship, but doesn't
  validate the design's actual hard part, and callers would hit a wall
  the moment they try to send anything struct/enum-shaped.

**Decision: deep-copy on channel send.** Same precedent as the memory-
model GC decision above — ship the simple, correct mechanism first;
don't build the harder shared-heap-with-atomics machinery speculatively
before real Plum code has actually needed it. Concretely: `tx.send(v)`
deep-copies `v` into a fresh cell reachable from the RECEIVING task's
own `Heap`, then the original binding is dropped — from the SOURCE
LANGUAGE's perspective this still looks and type-checks exactly like a
move (reusing `v` after `send` is still a compile error, via the same
last-use analysis), it just isn't zero-copy underneath. Every `Heap`
stays single-owner and non-atomic, exactly as already documented
elsewhere — no concurrent-refcount machinery needed anywhere. Revisit
the shared-heap-with-atomics option later ONLY if real workloads show
the copy cost actually matters (same "validate before building"
discipline as the GC decision) — switching later is an implementation
change, not a language-semantics change, since `send` already reads as
a move at the source level either way.

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

**Literal patterns and whole-arm catch-alls — Decided (2026-07-28).**
`match`'s IR node has always been fundamentally Ctor-TAG-based (it
reads a heap cell's tag and dispatches on it) — great for enums/
structs/tuples/arrays, but `Int`/`Float`/`Bool` aren't heap values at
all, and `Str` (heap-backed, but not `Ctor`-shaped — see heap.rs's
`CellData::Str`) isn't either. A bare `_`/identifier as a WHOLE
top-level arm (as opposed to nested inside a variant's args, which
always worked) had the identical problem: no tag to dispatch on. Both
gaps are closed now, via two ADDITIONAL desugarings in `lower_match`
(checked before the existing Ctor-tag path, which is otherwise
completely unchanged — zero regression risk for any match that already
worked):

- **A single catch-all arm** (`match anything { _ => .. }` or `match
  anything { x => .. }`, and ONLY that one arm) works for ANY
  scrutinee type — no tag inspection needed at all, so it desugars
  directly to `Let{name, value: scrutinee, body}` (or a discarded
  binding for `_`). This is deliberately excluded when the sole arm has
  a GUARD: a guard evaluating false would have nothing left to fall
  back to.
- **A literal match** (`match x { 1 => .., 2 => .., _ => .. }`) — every
  arm is `Int`/`Float`/`Bool`/`Str` EXCEPT a REQUIRED trailing catch-
  all — desugars to an ordinary `If`/`Binary(Eq)` chain (a guard, if
  present, combines via `Binary(And, eq_cond, guard)` — reusing the
  EXISTING short-circuit `&&` node, not a fresh boolean check, so no
  arm's tail gets duplicated in the IR). No new IR node needed for
  either case.

**Why the trailing catch-all is REQUIRED, not optional — a real,
discussed design decision (`AskUserQuestion`), not assumed solo.**
Ctor-tag matching has its OWN separate exhaustiveness gap already
(unmatched tag → a RUNTIME "no match arm for tag" error) — a literal
match could have mirrored that by adding a small new "runtime match
failure" IR node instead. Two real options were on the table; the
chosen one was: literal domains are infinite (or, for `Str`, at least
not enumerable) in a way a finite enum's tag set never is, so a
missing catch-all is REJECTED AT COMPILE TIME (mirroring Rust's own
rule for its own literal match arms) rather than deferred to a runtime
surprise — and it comes for free with the if-chain desugaring, no new
IR node required. The trailing arm additionally can't have its OWN
guard (a guard evaluating false there would reopen exactly the gap
this rule exists to close, with nothing left to fall back to and no
"runtime match failure" node to fall back to using) — checked
identically at BOTH the type-checking (`infer_match`) and lowering
(`lower_match`) gates, so a program that fails one always fails the
other with a consistent message, never a confusing gap between them.
Deliberately uniform across all four literal types — `Bool` having
only two values (and thus being technically enumerable) was
considered and rejected as a special case, in favor of one simple
rule everywhere.

**Mixing a catch-all into a Ctor-tag-shaped match — Decided
(2026-07-28), same evening.** `match x { Circle(r) => .., _ => .. }`
now works too — the one thing explicitly deferred just above turned
out not to need a real IR extension after all. `lower_match` recognizes
a catch-all (`_`/bare-ident) in the LAST position, mixed among
otherwise Variant/Tuple/Struct-shaped arms, and gives it a sentinel
`MatchArm.tag` (`DEFAULT_ARM_TAG = "0Default"`, same leading-digit
unreachable-by-any-real-identifier trick as `RANGE_TAG`/`ARRAY_TAG`)
instead of routing it through `lower_tag_pattern`. `Interpreter::eval`'s
`Match` case recognizes that ONE sentinel specially: it matches
UNCONDITIONALLY regardless of the scrutinee's real tag, and binds the
WHOLE scrutinee value (not positional fields — there's no fixed field
count to bind against) under the arm's one optional name. `ir::Expr::
Match`'s SHAPE is completely unchanged (no new field, no new node) —
every other pass that already walks `MatchArm.tag` as an opaque string
(fbip.rs's traversals, for instance) needed zero changes.

Unlike the pure-literal-match case just above, the trailing default arm
here is allowed to have a guard: if it evaluates false, that falls
through to the exact same "no match arm for tag" RUNTIME error ordinary
guarded Ctor-tag arms already accept as a possibility (a pre-existing,
tested precedent — `no_matching_guarded_arm_is_a_runtime_error`) — no
NEW exhaustiveness gap is being opened here, since this path already
relies on that same runtime fallback either way, unlike literal
matching's deliberately stronger, purely-compile-time-checked
guarantee. A catch-all in any position OTHER than last, mixed among
Ctor-tag arms, remains unimplemented — only the trailing position gets
the sentinel treatment.

**`Pattern::Or` — Decided (2026-07-29), same session, immediate
follow-up.** `A(v) | B(v) => ..` now works too — scoped to TAG-shaped
alternatives (Variant/Tuple/Struct), matching DESIGN.md's own original
`Shape.Square(s) | Shape.Rectangle(s, s) => ...` example above; literal
alternatives (`1 | 2 => ..`) remain a separate, still-unimplemented
extension. `lower_match` expands an or-pattern arm into MULTIPLE
`MatchArm`s (one per alternative tag), all sharing the SAME `guard`/
`body` IR (lowered once, cloned per alternative) — `ir::Expr::Match`
needed no new concept for this at all: ordinary tag matching already
supports several arms sharing one tag (see the pre-existing
`two_arms_may_share_a_tag`-style precedent), and nothing stops the
reverse — one logical arm expanding to several tags.

Real, checked requirements, at BOTH the lowering and type-checking
gates: every alternative must bind the SAME names in the SAME order
(the shared body/guard can only be typed once, against one consistent
binding environment — same rule Rust's own or-patterns enforce), same-
named bindings across alternatives must unify to one consistent TYPE
(`Circle(Float) | Square(Int)` binding the same name in both is a real
type error, not silently resolved one way), and no alternative may
contain a NESTED Variant/Tuple/Struct sub-pattern (lowering's synthetic-
placeholder destructuring mechanism doesn't compose across multiple
arms sharing one body).

**A real "type-checks-but-fails-at-lowering" leak caught and fixed
before landing, not shipped-then-patched**: the first implementation
attempt added `Or`-handling directly inside `bind_pattern` — the shared
helper used BOTH for whole top-level arm patterns AND recursively for
NESTED sub-patterns inside Struct/Tuple/Variant fields. Since lowering
ONLY supports `Or` at the whole-top-level-arm position (its
`classify_subpattern`, used for nested positions, has no `Or` case at
all), this would have let a nested or-pattern like `Point { x: 1 | 2, y
} => y` type-check successfully and then fail at lowering — caught
immediately by the full-workspace test run (a pre-existing test,
`nested_or_pattern_is_still_not_yet_supported`, started failing).
Fixed by moving the `Or` handling OUT of `bind_pattern` entirely into
its own dedicated `infer_or_pattern`, called only from `infer_match`'s
top-level per-arm dispatch — mirroring `lower_match`'s own structure
exactly (a special check before ever reaching the shared per-pattern
function), rather than teaching the shared, recursively-used function
about a shape it should only ever see at the top.

**Enum match exhaustiveness — Decided (2026-07-29), discussed via
`AskUserQuestion` first (a real new static check with rejection risk,
not implemented solo).** `match` over an `Enum`-typed scrutinee must
now cover every declared variant — with a real Ctor-tag arm naming it,
an or-pattern alternative naming it, or a valid trailing catch-all
(`_`/bare-ident, which by construction already accepts anything) — or
it's a COMPILE-TIME error naming exactly which variant(s) are missing,
instead of today's `no match arm for tag` error only firing if that
code path is ever actually reached at runtime. This is the headline
"robust pattern matching" feature this whole family of `match` work
has been building toward. Applies to the prelude's `Option`/`Result`
exactly like any user-declared enum, with zero special-casing needed
(they're ordinary ADT declarations under the hood). Struct/Tuple
scrutinees need no such check — their single Ctor tag is trivially
covered by any arm that type-checks against them at all. Lives entirely
in `infer_match` (type-checking); needed ZERO lowering or interpreter
changes, and — somewhat surprisingly — caused ZERO fallout across the
whole pre-existing test suite when it landed (every existing match
already either covered every variant or already had a catch-all).
`TypeContext` gained one new accessor for this, `enum_variant_tags`
(the REVERSE direction from the existing tag-keyed `variants` map — an
enum name to ALL its declared tags, in declaration order).

A GUARDED arm still counts as covering its variant, EVEN THOUGH the
guard could fail at runtime and fall through to the pre-existing "no
match arm for tag" error — a deliberate choice, matching this project's
established "prefer false negatives over false positives" risk
direction (the same principle behind movecheck.rs's permissiveness),
discussed and picked explicitly over the stricter alternative (Rust's
own rule: a variant reachable ONLY through a guarded arm is still
flagged as non-exhaustive). **Explicitly flagged, at the point this was
decided, as a genuine revisit candidate — not a settled-forever
choice**: the stricter rule remains real, plausible follow-up work if
the permissive version turns out to hide too many real bugs in
practice; it would need tracking guarded vs. unguarded coverage
separately, more implementation complexity than the version that
shipped.

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

**Self-referential closures — Decided (2026-07-29).** `let fib = |n| if
n < 2 { n } else { fib(n-1) + fib(n-2) }` now works, for BOTH a
top-level (`Global`) closure and a local block-level one — closing the
"recursive closures that capture themselves" gap this document
previously listed as an unresolved detail. Deliberately SELF-recursion
only, not mutual recursion between two separately-declared closures
(`let is_even = |n| .. is_odd(n-1) ..` / `let is_odd = |n| .. is_even
..` calling each other) — globals are never pre-declared as a whole
batch the way top-level FUNCTIONS already are (see the ordinary
`Global` ordering rule elsewhere in this document: a global can only
ever see EARLIER globals, never a later or mutually-recursive sibling),
so only a closure's OWN name is ever made visible early.

Two GENUINELY different fixes were needed, one at each layer, because
the top-level and local cases have different root causes:
- **Type-checking** (`plum-types`): BOTH the `Global` case (`infer_
  program`'s Phase 1.5) and the local-block-`let` case (`infer_block`'s
  `Stmt::Let`) needed the SAME fix — when a `let`'s value is a closure
  LITERAL, pre-bind the `let`-bound name to a fresh placeholder type
  BEFORE inferring the closure's body (so a recursive call inside the
  body resolves against something, even though it isn't concretely
  resolved yet), then unify that placeholder against the closure's
  actual inferred type afterward. The exact same "fresh var now, unify
  with the real type after" trick already used for top-level FUNCTION
  self/mutual recursion (Phase 1) — just applied one level differently,
  since a closure-valued `let` doesn't get its own pre-declaration
  phase the way named functions do.
- **The interpreter** (`plum-interp`) needed a fix ONLY for the LOCAL
  case, not the global one — a real asymmetry worth remembering. A
  `Value::Closure`'s captured environment is a one-time SNAPSHOT of
  `self.env` taken at CREATION time; for a local `let`, that snapshot
  happens BEFORE the `Let` node ever pushes the new name onto `self.
  env`, so a naive implementation would leave a recursive self-call
  unbound at runtime even after the type-checker allowed it. Fixed with
  the classic `letrec` trick: `Expr::Let`'s eval special-cases a
  closure-literal `value`, reserving the closure's id up front and
  seeding `(name, Value::Closure(id))` into its OWN captured snapshot
  (in addition to the ordinary outer-scope push). The GLOBAL case needs
  NONE of this at the interpreter level: `Interpreter::load_program`
  finishes evaluating and inserting every global into `self.globals`
  BEFORE any function or closure is ever actually CALLED, and free-
  variable lookup inside a closure body already falls through to `self.
  globals` at CALL time (not creation time) regardless of what got
  captured — so a global closure calling itself by name already just
  worked, for free, once the type-checker stopped rejecting it.

Needed ZERO lowering or FBIP changes — lowering is purely structural
(it doesn't reason about binding/scoping at all), and closures were
already outside FBIP's tracking before this change and remain so.

### Arrays — Decided (v1 scope)

`Array[T]` is the first collection type — no array/list/collection
existed anywhere in the language before this (`for` could only ever
iterate a literal `start..end` range). Decided 2026-07-29, after
weighing `Array[T]`/`List[T]`/`Vec[T]` as the name: **`Array[T]`** —
the growth semantics decided below are conceptually Rust's `Vec`
(capacity-header realloc, in-place growth when uniquely owned), and
`List[T]` risked the wrong mental model for anyone coming from OCaml/
F#/Haskell, where `list` means an immutable singly-linked cons-list
with O(n) indexing — the opposite of what's being built here (O(1)
index, amortized-O(1) push).

**Mutability model: a purely functional API, with FBIP making it fast
in place** — the SAME choice already made for every other heap value
in the language, not a new mutation model invented for arrays
specifically. `arr.push(v)` is an ordinary VALUE-returning operation
(`let b = a.push(v)`); nothing about the surface syntax exposes
mutation. The `let mut`-based genuine-in-place-mutation alternative was
explicitly considered and rejected for v1: it would need real aliasing
rules (can two `let`-bound names alias the same array? what happens on
push if they do?) that don't exist anywhere in the language yet, for a
problem FBIP's existing reuse-in-place philosophy already has a proven
answer to.

Literal syntax: `[e1, e2, ...]` — a genuinely new primary-expression
grammar production (unlike `arr[i]` indexing, which the parser already
had, written ahead of a real collection type to use it with).

Operations: construction (`[1, 2, 3]`), `arr[i]` (index read, runtime-
bounds-checked, not a compile-time check), `.len()`, `.push(v)`,
`.pop()`, `.set(i, v)`, `.remove(i)`, `.map(f)`, `.filter(f)`,
`.fold(init, f)`. `for x in arr` iteration IS now supported (see
below) — `for` accepts either a `Range` or an `Array[T]`.

`.pop()`/`.set(i, v)`/`.remove(i)` (2026-07-28) follow `.push(v)`'s
exact precedent: new primitive IR nodes (`ArrayPop`/`ArraySet`/
`ArrayRemove`), runtime-checked bounds (`.set`/`.remove`) or
runtime-checked non-emptiness (`.pop()`) rather than a compile-time
check. All four now reuse-in-place when uniquely owned — see below.

`.map(f)`/`.filter(f)`/`.fold(init, f)` (2026-07-28) take a DIFFERENT
approach: no new IR nodes at all. Each desugars directly into a small
tree of EXISTING IR nodes (`Let`, `For`, `ArrayLen`, `Index`,
`ArrayPush`, `Assign`, `If`, `Call`) — the same "reuse what exists"
philosophy `for x in arr` and the array literal already established.
`.map(f)` builds a fresh output array and pushes `f(elem)` for each
element; `.filter(f)` does the same but only pushes when `f(elem)` is
`true`; `.fold(init, f)` accumulates a scalar via `acc = f(acc, elem)`
instead of building a new array. Because these are ordinary desugarings
into existing nodes, FBIP/movecheck needed ZERO changes to support
them — only `lower.rs` (three new shape-detected `Call` cases dispatch
to three new `lower_array_*` desugaring functions) and `infer.rs`
(three new `Call`-shape cases unifying against `Type::Function`) needed
touching.

**`for x in arr` iteration — Decided (2026-07-28).** `for` now accepts
`Array[T]` as well as `Range`. Desugars into an index-based loop reusing
only EXISTING IR nodes (`Let`, `For`, `ArrayLen`, `Index`) — no new IR
node needed, following the file's established "reuse what exists"
lowering philosophy:
```
let __for_arr = <iter> in
  for __for_i in 0 .. __for_arr.len() {
    let x = __for_arr[__for_i] in <body>
  }
```
The genuinely new piece is a second span-keyed inference→lowering
side-channel, `Infer::array_for_loops: HashSet<Span>` (mirroring the
existing `field_owners` pattern), because lowering has no type
information of its own and a bare `for x in y { ... }` doesn't
syntactically say whether `y` is a `Range` or an `Array[T]`. Populated
in `infer_for`'s general (non-literal-range) branch by matching the
iterand's ALREADY-RESOLVED type shape directly against `Struct("Array",
[T])` — NOT by trial-unifying against `Array[fresh]` and checking
success, since an unresolved type variable (e.g. a still-generic
function parameter's type) would trivially unify against either Array
or Range, wrongly committing a genuinely Range-typed polymorphic loop
to the array desugaring. This was a real bug caught by the existing
`a_range_stored_and_passed_around` test before landing. A consequence
inherited from Hindley-Milner having no ad-hoc "iterable" trait: a
single function body with an unannotated `for i in x { ... }` still
commits `x`'s type to exactly ONE of Range or Array for that whole
function (whichever shape inference resolves first) — annotate the
parameter explicitly (`arr: Array[Int]`) to force the array reading.

**Builtin-type parameter/return annotations — Decided (2026-07-28).**
`resolve_annotation` now accepts `Array[T]`/`Task[T]`/`Sender[T]`/
`Receiver[T]` in function parameter and return-type annotations, not
just real user-declared structs/enums. Needed its own fixed-arity-one
check ahead of the ordinary `ctx.generic_params`-lookup path, since
these four names are DELIBERATELY never registered in `TypeContext`
(they exist purely for their structural unify behavior — see the
opaque-pseudo-generic-builtin-types precedent above). Closes the gap
flagged when `for x in arr` landed.

**`.push()`/`.pop()`/`.set()`/`.remove()` reuse-in-place — Decided
(2026-07-28).** All four now recycle the SAME heap cell instead of
always allocating fresh, whenever the array is uniquely owned at that
point — the functional API surface (`let b = a.push(v)`) is completely
unchanged; this is purely a memory-savings optimization underneath it,
never a semantic one. Unlike `CtorReuse` (which only ever recognizes
SAME-arity struct/tuple/enum reconstruction inside a `Match` arm),
these operations aren't Match-shaped and can genuinely grow or shrink a
cell's field count — so this needed its own mechanism, though one that
directly mirrors `CtorReuse`'s existing safety argument rather than
inventing a new one:
  - Four new `*Reuse` IR nodes (`ArrayPushReuse`/`ArrayPopReuse`/
    `ArraySetReuse`/`ArrayRemoveReuse`), each carrying a `reuse_of:
    String` naming the array's plain-variable binding — exactly like
    `CtorReuse`'s own `reuse_of` field, and for the same reason: only a
    bare variable names a specific cell reuse can target.
  - `mark_reuse` (fbip.rs, the existing reuse-in-place pass, which
    always runs AFTER refcount-insertion) gained a structural rewrite:
    `ArrayPush{array: Var(x), ..}` (and the other three) unconditionally
    becomes the `*Reuse` form whenever `array` is a plain `Var` — no
    arity/shape check needed here at all, unlike `CtorReuse`'s
    same-arity requirement, since array ops have no fixed shape to
    match against.
  - The actual SAFETY decision is made entirely at RUNTIME, in
    `Interpreter::eval`, via the EXACT SAME `Heap::dec_and_maybe_reuse`
    primitive `CtorReuse` already uses: decrement `reuse_of`'s
    refcount first, and only reuse the SAME address if that reaches
    zero (nobody else holds a reference); otherwise allocate fresh,
    exactly as before. The structural rewrite in `mark_reuse` is safe
    regardless of whether `array` turns out to still be shared — if `a`
    is used again later in the same scope (e.g. `let b = a.push(v);
    a.len()`), `insert_refcount_ops` (which always runs BEFORE
    `mark_reuse`) already inserted an `Inc` for that later use, keeping
    the runtime refcount above zero at the `*Reuse` site and correctly
    forcing the fresh-allocation path — identical to how `CtorReuse`
    already handles the same "used again later" case.
  - Tests added at every layer: plum-ir (`mark_reuse`'s structural
    rewrite, including "non-variable base is not a candidate"), plum-
    interp (hand-built-IR reuse-fires-when-unique/allocates-fresh-when-
    shared pairs mirroring `CtorReuse`'s own existing tests exactly,
    plus real-source `alloc_count`-proof end-to-end tests), plumc
    (full-pipeline correctness — reused values still see original
    contents, chained push/set/remove/pop still produce correct
    results). Workspace is now 838 tests, clean build, zero warnings.

### Strings — Decided (2026-07-28)

Made heap-backed and refcounted, closing a real divergence between this
document's own stated intent (the "Core mechanism" section above has
always listed strings among the refcounted heap types) and the actual
implementation, which had `Value::Str(String)` as a plain unboxed
scalar — same treatment as `Int`/`Bool`, zero FBIP involvement, and
(as a direct consequence) no operations at all beyond construction and
equality.

**Representation.** There is no `Value::Str` variant — a string value
IS a `Value::HeapRef`, exactly like an array value, into a dedicated
string heap cell. Strings deliberately do NOT reuse the existing
`Ctor`-shaped cell (`tag: String, fields: Vec<Value>`) the way arrays
do: there's no `Value::Char` in this language, and representing a
string as one `Ctor` field per codepoint would be wildly wasteful
beyond toy inputs. `heap.rs` gained a `CellData` enum (`Ctor{tag,
fields}` / `Str(String)`) so a single heap can hold both shapes; `read`
stays `Ctor`-only (erroring on a string cell), a new `read_str`
mirrors it for strings, and a new `read_any` (returning a `CellView`
enum) is for the few callers — `to_portable`, `.len()`'s eval,
heap-value equality — that need to dispatch on whichever shape a given
`HeapRef` actually turned out to be, rather than asserting one ahead of
time.

**v1 operations:** construction (string literals, unchanged syntax),
`.len()` (byte length, matching Rust's own `String::len()` — NOT char
count), `.concat(other)`, `s[i]` (byte indexing — see below), `.runes()`
(character decoding — see below), and `==`/`!=` (heap-aware now — reads
both cells and compares content when both are string cells; two Ctor
cells, or a Ctor and a string, still hit the same pre-existing "cannot
compare" error general heap-value structural equality already had
before this chunk, since that's a separate gap this didn't attempt to
close). Deliberately NOT yet decided/implemented: `split`/`trim`/
`to_upper` and other standard string library operations, and string
PATTERN matching in `match` (`Pattern::Str` exists at the AST level but
was never wired into `lower_tag_pattern` — a PRE-EXISTING gap,
confirmed unaffected by this change either way).

**Indexing — Decided (2026-07-28): byte-indexed, returns `Int`.**
`s[i]` reads the raw BYTE value (0-255) at byte offset `i`, exactly
matching `.len()`'s own byte semantics — `s[s.len() - 1]` always works,
regardless of what's in the string. Explicitly NOT Unicode-character-
aware: indexing into the middle of a multi-byte UTF-8 character (e.g.
`"café"[3]`, landing on the first of `é`'s two encoded bytes) is
allowed and just returns that raw byte — not an error, not a decoded
character. This was a real, discussed three-way fork (byte-indexed
returning `Int`; byte-indexed returning a 1-byte `Str`; codepoint-
indexed returning a 1-character `Str`) — the codepoint-indexed option
was rejected specifically because it would be O(n) per index (UTF-8 is
variable-width) AND inconsistent with `.len()` already being byte
length (`s[s.len() - 1]` would be wrong/panic for any non-ASCII
string). Reuses the EXISTING `Index` node (no new IR node) — the same
"lowering can't tell array from string apart, so share the node and
dispatch at runtime" trick `.len()`/`ArrayLen` already established.

**`.runes()` — Decided (2026-07-28): the character-aware complement to
byte indexing.** Since byte-indexing can't give you actual Unicode
characters, `.runes()` decodes a string's UTF-8 bytes ONCE, O(n), into
an ordinary `Array[Int]` — one element per Unicode codepoint (Rust's
own `char`, via `str::chars()`), not per byte. `"café".len()` is 5
(bytes) but `"café".runes().len()` is 4 (characters) — the accented
character decodes to exactly one array element. Every access into the
resulting array is then an ordinary O(1) `Index`, and — since it's a
completely normal array — `for x in "s".runes() { ... }`, `.map()`,
`.filter()`, `.fold()` etc. all already work on it for free, no new
machinery needed. This IS a genuinely new primitive node (`StrRunes`,
no reuse-in-place counterpart — it always builds a brand-new,
differently-shaped array, never recycling the string cell it read
from): unlike `.len()`/`s[i]`, UTF-8 decoding needs bit manipulation
the IR has no operators for, so it can't be expressed as a desugaring
into existing nodes the way `.map()`/`.filter()`/`.fold()` could be.

**`.trim()`/`.split(sep)` — Decided (2026-07-28).** Rounded out the same
evening as indexing/`.runes()`. `.trim()` follows `.concat()`'s exact
precedent: delegates directly to Rust's own `str::trim()` (Unicode
whitespace both ends), gets the full `StrTrim`/`StrTrimReuse` reuse-
in-place treatment (same `Heap::dec_and_maybe_reuse_str` mechanism).
`.split(sep)` evaluates to `Array[Str]`, delegating directly to Rust's
own `str::split()` INCLUDING its edge-case behavior, deliberately not
special-cased: consecutive separators yield empty-string elements, no
match yields one whole-string element, an empty `sep` yields one
element per character plus empty leading/trailing entries. Like
`StrRunes`, `StrSplit` is a genuinely new primitive node with NO
reuse-in-place counterpart — it builds a whole new array of freshly
allocated string cells (a differently-shaped heap value), never
transforming one existing cell in place. Because the result is an
ordinary `Array[Str]`, `for`/`.map()`/`.filter()`/`.fold()`/indexing
all already work on split output for free — confirmed with a dedicated
test summing `.len()` across `for part in "...".split(",")`.

**`.to_upper()`/`.to_lower()`/`.contains()`/`.starts_with()`/
`.ends_with()`/`.replace()` — Decided (2026-07-28, case mapping revised
2026-08-03).** Rounded out strings further the same evening, all
delegating directly to Rust's own `str` methods of the same name
(Unicode-aware case conversion via `to_uppercase()`/`to_lowercase()`).
**Codegen-specific caveat**: the compiled backend implements
`.to_upper()`/`.to_lower()` via libc's `towupper`/`towlower`
(locale-aware, one-codepoint-in-one-codepoint-out — real Unicode SIMPLE
case mapping across the vast majority of scripts, not an ASCII-only cut
as originally shipped). The one remaining, narrowly-scoped divergence
from this section's full-Unicode intent: multi-codepoint expansions
(German `ß`→`"SS"`) can't happen through `towupper`/`towlower`'s
1-in-1-out C signature, so `ß` stays `ß`. The interpreter's own
behavior is unaffected — see "Unicode-aware string operations" in the
LLVM backend section below for the full mechanism and the tests
proving both real-Unicode mapping and the narrower gap. `.to_upper()`/
`.to_lower()`/
`.replace()` all evaluate to `Str` and follow `.trim()`'s exact
precedent — `StrToUpper`/`StrToUpperReuse`, `StrToLower`/
`StrToLowerReuse`, `StrReplace`/`StrReplaceReuse`, each with the full
reuse-in-place treatment. `.contains()`/`.starts_with()`/`.ends_with()`
are the FIRST string operations that evaluate to `Bool` rather than a
new `Str` — `StrContains`/`StrStartsWith`/`StrEndsWith` have no
reuse-in-place question to even ask, since nothing is heap-allocated
for the result at all (a plain `Bool`, same as `Int`/`Float`). 9 new IR
nodes total (6 with a `*Reuse` counterpart, 3 without), all following
directly-established precedent — no new design decisions needed beyond
confirming the "just call the underlying Rust method" and "Bool-
returning ops need no reuse variant" patterns already implicit in
`.len()` and `==`.

**`.to_string()` — Decided (2026-07-28), scoped to `Int`/`Float`/`Bool`/
`Str`.** Closes the real, glaring gap of having no way to convert a
number into a string at all (so no way to build a display string
dynamically). Genuinely different from every other `.method()` above:
`base` can be ANY of four unrelated concrete types, not one specific
type to unify against, so there's a single shared `ToString` node
(lowering still has no type info to pick a per-type node anyway) whose
`Interpreter::eval` dispatches on the ACTUAL `Value` variant `base`
evaluates to at runtime — `Int`/`Float`/`Bool` render via their own
`to_string()`, a `Str` is a no-op content-wise. Deliberately NOT yet
extended to structs/enums/arrays/tuples — the IR carries no field
NAMES at all (see ir.rs's own top-of-file doc comment), so even a
minimal positional rendering needs real design, not just wiring; a
real, separate gap, not silently pretended away.

`infer.rs`'s check is deliberately PERMISSIVE when `base`'s type is
STILL an unresolved type variable at the call site — a real regression
was caught and fixed here (not shipped-then-patched): the first
implementation attempt eagerly errored whenever `base`'s resolved type
wasn't already one of the four, which wrongly rejected the natural,
expected `[1,2,3].map(|x| x.to_string())` — inside the closure body,
`x`'s type isn't unified against the array's element type until AFTER
the closure's own inference finishes, so it's still a bare `Var` at
the point `.to_string()` is checked. Fixed to only reject a type that's
ALREADY concretely known and wrong; an unresolved var passes through,
with the interpreter's own runtime check (already written, since it
has to handle any `Value` regardless) as the fallback if that var
later turns out to be something unsupported. Same "compile-time check
when possible, runtime check as the honest fallback otherwise" split
`Index`'s out-of-bounds checking already established — confirmed with
a dedicated pair of tests: a generic function using `.to_string()`
type-checks and runs correctly when called with a supported type, and
produces a clear RUNTIME (not compile-time) error when called with an
unsupported one only discoverable at that call site.

**`.len()`'s shared-node trick.** `.len()` is shape-detected IDENTICALLY
for arrays and strings at lowering time — lowering has no type
information to tell `arr.len()` from `s.len()` apart, so both still
lower to the exact same `ArrayLen` node (no new IR node at all).
Dispatch happens at RUNTIME instead, in `Interpreter::eval`, via
`read_any`: a `Ctor` cell's field count for arrays, a string cell's
byte length for strings. `infer.rs`'s `.len()` case checks the base's
ALREADY-RESOLVED type against `Type::Str` directly (not a blind
trial-unify) before falling to the existing Array-unify path — the
same "check resolved shape, don't trial-unify" precedent `for x in
arr` and `.len()`'s OWN Array case already established, for the same
reason: trial-unifying against `Array[fresh]` first would trivially
succeed for a still-unresolved type variable, wrongly ruling out Str.

**`.concat(other)` and reuse-in-place.** Unlike `.len()`, `.concat()`
has no ambiguity with any array operation, so it gets its own dedicated
node pair — `StrConcat`/`StrConcatReuse` — built as a DIRECT structural
mirror of `ArrayPush`/`ArrayPushReuse` (including the reuse-in-place
optimization from the section above): `mark_reuse` rewrites
`StrConcat{base: Var(x), ..}` into `StrConcatReuse{reuse_of: x, ..}`
unconditionally when `base` is a plain variable, and the actual safety
decision happens at runtime via a NEW `Heap::dec_and_maybe_reuse_str`
— the exact string-cell counterpart to `dec_and_maybe_reuse`, same
refcount-gated recycle-or-allocate-fresh logic.

**Blast radius was small.** Before this chunk, strings had exactly
ONE consumer outside literal construction and `==` (`Value::Str`
appeared at only 5 call sites total, all in `plum-interp/src/lib.rs`)
— precisely BECAUSE no other operations existed yet. `PortableValue::
Str` (the cross-thread/channel-safe representation) was UNCHANGED: it
already stored a plain owned `String` with no heap reference, exactly
right for something that has to survive being read on a different
interpreter's heap entirely — `to_portable` now resolves a string
`HeapRef` into it via `read_any` instead of cloning an inline field,
and `from_portable` now allocates a fresh heap cell instead of building
an inline `Value::Str`, but the wire format itself didn't need to
change at all.

Tests added at every layer: plum-interp/heap.rs (the new `CellData`/
`read_any`/`dec_and_maybe_reuse_str` machinery directly), plum-ir
(lowering's shared-node behavior, `mark_reuse`'s `StrConcat` structural
rewrite, `is_syntactically_heap` now recognizing `Expr::Str`), plum-
types (`.len()`/`.concat()` success and error cases, the still-
unresolved-var-defaults-to-Array regression), plumc (concat/len/
equality/struct-field/reuse-still-correct end-to-end). Workspace is now
865 tests, clean build, zero warnings.

### Effect/unsafe tracking — Decided

A lightweight `unsafe`/`extern` marker that propagates from FFI call
sites, not a full Koka-style effect system. Full effect tracking is its
own multi-year research project layered on an already-ambitious
language; scoped down to just marking the FFI trust boundary for v1.

Implemented exactly that narrow scope: `unsafe { .. }` was already valid
syntax (parsed since early on) but a pure no-op everywhere downstream —
it lowered, type-checked, and move-checked transparently, gating
nothing. It now gates exactly one thing: calling a declared `extern`
function. `plum-types::Infer` tracks an `in_unsafe: bool`, set true for
the duration of inferring an `unsafe` block's body (saved/restored, not
reset to `false`, so nesting behaves sensibly) and checked the instant a
`Call`'s callee resolves to a name declared in some `extern "C" { .. }`
block; calling one outside `unsafe` is a type error, reported before
lowering or interpretation ever run. This is enforced entirely at
type-checking time — by the time `plum-ir`'s lowering produces an
`ExternCall` node, the call has already been proven safe to make, so
neither lowering nor the interpreter re-check anything.

No other operation is `unsafe`-gated (no raw pointers, no unchecked
arithmetic) — extending this to cover something else, should the
language ever grow such a thing, is unscoped future work, not implied by
today's implementation.

## Module system — Decided, v1 implemented

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
prelude needing no `use` at all, same as Rust's prelude — as of the
first standard-library chunk, `println` lives there too (see "Standard
library" below for why, and why that's a deliberate, revisitable v1
trade rather than the long-term home for stdlib functions in general).

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

### What's actually implemented (v1)

`crates/plumc/src/modules.rs`'s `typecheck_and_run_modules(modules: &[(module_path,
source)], fn_name, args)` — an IN-MEMORY multi-module entry point.
`crates/plumc/src/project.rs`'s `typecheck_and_run_project(root: &Path,
fn_name, args)` is the real filesystem layer on top: walks a project
directory tree, reads every `.plum` file, and turns it into that same
`&[(&str, &str)]` shape (module path = containing directory, relative
to the project root, dot-joined) before handing off — there's still no
`mod` declaration anywhere in Plum source itself, a file's module path
is purely a function of its location in the tree. `plumc <project-dir>`
(the CLI, `main.rs`) runs a project's `main` this way, calling a
unit-param entry point (`let main () = { ... }`).

**Architecture**: a PRE-PASS over the parsed AST, not a change to
`plum-types`/`plum-ir`/`plum-interp` — none of them learned a module
system exists. Every declaration gets renamed to its fully-qualified
form (`"shapes.Circle"` as one literal string) and every reference gets
rewritten to match, via a real (if approximate) scope-tracking AST walk
that never touches a genuine local variable/parameter binding. The
merged, fully-qualified `ast::Program` then runs through the EXACT SAME
`run_resolved_program` function `typecheck_and_run` (the pre-existing
single-file entry point) already used — `TypeContext`/`Interpreter`'s
existing flat `HashMap<String, ..>` tables just see one more ordinary
string key, since Plum's lexer can never produce an identifier
containing `.`, so a qualified name can never collide with a real one.
The ROOT module (path `""`) is never qualified, keeping
`typecheck_and_run`'s single-file behavior byte-for-byte unchanged.

**Also wired to the LLVM backend**: `typecheck_and_run_modules`/
`typecheck_and_run_project` are each split into a `resolve_*` half
(`resolve_modules`/`resolve_project`) that stops right after producing
the merged `ast::Program` — no interpreter run — plus the original
`Value`-producing function as a thin wrapper calling `resolve_*` then
`run_resolved_program`. This is what lets `plumc build` (below) reuse
the EXACT SAME module-resolution pre-pass the interpreter CLI does,
with zero changes to the resolver itself: module resolution never had
any notion of which backend eventually consumes its output.

**`use` semantics**: `use shapes;` brings the WHOLE module into scope
for qualified access (`shapes.Circle`); required before `shapes.X`
resolves at all (referencing an un-`use`d module's qualified name
falls through unresolved, same as any genuinely undeclared name).
`use shapes.Circle;` is the bare-import escape hatch, checked (both
existence and `pub`) right at the `use` site rather than deferred to
wherever `Circle` might (or might never) actually be referenced.
Root-module declarations (and the prelude, injected into the root) are
visible from EVERY module with no `use` needed — a deliberate rule, not
just backward-compatibility residue, matching the prelude's own
already-established "always available" spirit.

**`pub` is enforced**: a private item accessed from a different module
than the one that declared it is a compile error, checked at the same
point qualified-name resolution happens. Enforced for structs, enums,
top-level functions/globals, and extern functions — NOT for individual
struct fields (`is_pub` exists per-field in the AST, but nothing reads
it yet, a separate smaller gap) or for enum variant TAGS (see below).

**Known v1 scope boundary, not an accidental gap**: enum variant tags
(`Circle`, `Some`, `None`, ...) are NOT module-qualified or `pub`-
checked at all — they stay in the SAME flat, global, look-up-by-bare-
tag-name-alone namespace that predates the module system entirely (a
variant tag has never been validated against its owning enum, an
established precedent this pass doesn't touch). Two modules each
declaring an enum with a variant of the same tag name collide — a real,
narrow limitation, worth fixing if it becomes a practical problem,
folded into full variant-tag qualification the same way struct/enum
NAMES already are, but out of scope for this v1 pass.

## FFI and C interop — Decided (v1 scope)

- Calling **into** existing C libraries matters more early than being
  called **from** C/other languages, though both are goals.
- `extern` blocks declare foreign signatures with explicit C-ABI types.
  No implicit string/allocation coercion at the boundary — that would
  hide allocation/lifetime decisions exactly where they need to be
  visible.
- A `#[repr(C)]`-equivalent for structs that cross the boundary, since
  Plum's native structs may carry a refcount header or different field
  ordering than C expects. **Implemented** — see below. No dedicated
  annotation syntax exists (Plum has no attribute/annotation grammar at
  all yet); a struct is automatically FFI-safe if every one of its
  fields (recursively) is Int/Float/Bool or another FFI-safe struct.
- Callbacks: C APIs often want bare function pointers. Plum closures
  that capture an environment can't be handed to C directly without a
  trampoline. Practical answer (same as Rust): only non-capturing
  closures convert directly to C function pointers; capturing ones need
  explicit adapter machinery. **Not yet implemented** — v1 has no way to
  pass a Plum function as a C callback at all.
- Because refcounting (not tracing GC) is the primary mechanism, values
  crossing the FFI boundary don't need root registration the way OCaml's
  GC-tracked values do — this is a concrete, structural advantage over
  OCaml's FFI story, not just a claim.

### What's actually implemented (v1)

`extern "C" { fn name(param: Type, ...) -> RetType; }` blocks were
already valid grammar (parser support predates this feature); they now
have real lowering, type-checking, and runtime behavior:

- **Type scope**: `Int`/`Float`/`Bool`/`CStr`/a qualifying struct
  (mapping to C `long long` / `double` / `int` / `char*` / a real C
  struct — see below) — no raw pointers yet. An extern signature naming
  any other type is rejected with a clear error at `TypeContext::
  from_items` time, before inference even runs.
- **Strings**: explicit conversion, not implicit coercion — a Plum
  `Str` must be converted via `.as_cstr()` before it can cross an
  extern boundary; an ordinary `Str` value is a type error against a
  `CStr`-typed parameter. `.as_cstr()` produces `Type::CStr`, a type
  distinct from `Str` with no operations of its own besides being
  produced this way and consumed by a `CStr` extern parameter/return.
  At runtime, `.as_cstr()` (lowered to its own `ir::Expr::AsCStr` node,
  not folded into `ExternCall`) eagerly validates the string has no
  embedded null byte — a C string ends at the first null, so an
  embedded one would otherwise silently truncate at the actual call
  site instead of failing loudly where the conversion happens. A
  `CStr`-typed return value is copied into a fresh Plum heap string;
  the original C pointer is never freed (unknown provenance — might be
  static, might be malloc'd) — an honest, documented v1 leak-avoidance
  tradeoff. A null return pointer is a runtime error, not silently
  treated as an empty string.
- **Structs**: automatic/structural eligibility, not an explicit
  marker — a struct qualifies as an extern parameter/return type
  whenever every field (checked recursively) is `Int`/`Float`/`Bool` or
  another qualifying struct. `CStr` is deliberately excluded from
  struct fields (only valid as a function's own top-level param/return)
  — a nested `CStr` field would need a nested owning `CString` buffer
  kept alive through marshaling, out of v1's scope. A self-referential
  struct (`struct Node { next: Node }` — legal ordinary Plum, since
  every field is a heap-boxed `Value`, never inline) is rejected: a C
  by-value layout has no notion of a field pointing back to its own
  type, only Plum's heap-indirect model does. A generic struct is
  rejected too (FFI boundary is monomorphic-only for now). The real
  ABI layout (field offsets, padding, alignment, total size) is
  computed by libffi ITSELF via `ffi_get_struct_offsets` — never
  hand-rolled — since trusting the C library that already knows the
  target's real ABI beats reimplementing per-platform struct-layout
  math. A struct argument is "flattened" from Plum's heap-indirect
  `Ctor` representation into the C ABI's inline byte layout (and the
  reverse on return) via a small recursive marshal/unmarshal pass.
  Verified against Rust's own (C-ABI-authoritative) `#[repr(C)]` struct
  layout, including a deliberately padding-inducing field order, and
  against a real libc struct-returning function (`div`/`div_t`) through
  the full real-symbol-resolution pipeline.
- **Calling convention**: `sqrt(2.0)` where `sqrt` names a declared
  extern function is a genuinely separate IR node (`ir::Expr::
  ExternCall`), not a special shape of the ordinary `Call` node — an
  extern function isn't backed by a `Function`/`ClosureValue` at all.
  Deliberately **not first-class**: `let f = sqrt` doesn't work, only
  the direct `sqrt(...)` call shape is recognized (mirrors the existing
  `channel[T]()`/`ref(v)`/`.push()` shape-detection precedent used
  throughout `lower.rs`).
- **`unsafe`-gating**: see "Effect/unsafe tracking" above — calling an
  extern function outside `unsafe { .. }` is a type error.
- **Symbol resolution**: against the CURRENT PROCESS's own dynamic
  symbol table (covers already-linked libc/libm functions like
  `sqrt`/`abs`), via the `libloading` crate's Unix-only `Library::this()`
  API. `ExternBlock` has no library-path field, so there's no way to
  `dlopen` an arbitrary `.so` yet — only symbols already loaded into the
  running process are reachable. **Unix-only for v1** (Linux/macOS);
  Windows has no clean equivalent to "resolve against the running
  process" and is an honest, documented gap, not a silent wrong answer.
  `plum-interp`/`plumc`'s own `build.rs` link `libm` explicitly so
  standard math functions are actually present in the process to
  resolve against — a plain Rust binary that never calls a libm function
  itself doesn't pull it in by default.
- **Calling mechanism**: the `libffi` crate (Rust bindings to the real C
  `libffi`), used because the C function's signature is only known at
  *runtime* (parsed from Plum source), not at Rust-compile-time — there's
  no way to write a Rust `extern "C" fn` type for it ahead of time. Each
  declared extern function resolves ONCE, in `Interpreter::load_program`,
  into an `ExternFnHandle` (a `libffi::middle::Cif` + `CodePtr`); calling
  it marshals `Value::{Int,Float,Bool}` arguments into the shapes
  `libffi`'s call API expects and marshals the C return value back.
- **Dependency-choice rationale** (recorded because it set a standing
  policy, not just a one-off choice — see the "Self-hosting-viability"
  note below): `libffi`/`libloading` are used ONLY inside `plum-interp`,
  confined behind this project's OWN `ir::ExternType`/`ir::ExternFn`
  types — `plum-ir`, `plum-types`, and the AST never see libffi's own
  type representation. This is deliberate: an LLVM/native backend won't
  need a *dynamic-signature* calling mechanism at all (a compiled Plum
  program calling C becomes an ordinary call instruction via codegen),
  so the dependency naturally stops mattering rather than needing a hard
  "unwind." A hypothetical future self-hosted Plum interpreter facing
  the same runtime-unknown-signature problem would conventionally link
  against the *real* C `libffi` library directly (the same technique
  PyPy/LuaJIT use), not port the Rust crate — so today's crate choice
  doesn't block that path either.

- **Callbacks**: the `#[repr(C)]`-equivalent-for-structs bullet above
  already covers structs; this is the OTHER half — passing a Plum
  function TO C as a real function pointer. **Implemented**, with hard
  v1 restrictions:
  - **New grammar**: `(A, B) -> R` as a TYPE annotation (`ast::Type::
    Function`) — documented in GRAMMAR.md since early on but never
    actually implemented until this chunk (`ast::Type` only had `Path`/
    `Generic`). Resolves generally (any function-typed annotation, not
    just extern callbacks — a struct field or ordinary function param
    typed `(Int) -> Int` now type-checks too, a side benefit of closing
    this gap properly rather than special-casing it for extern alone).
  - **Only a bare top-level function name**, never a closure literal or
    a local variable, may be passed where a callback is expected —
    checked at type-checking time by name-set membership (`Infer::
    top_level_fns`), not real free-variable/capture analysis. A
    top-level function is non-capturing BY CONSTRUCTION (`Interpreter::
    call` always builds a completely fresh environment from just its
    own params), so no analysis is needed for THAT case — proving a
    closure literal doesn't capture anything would need real analysis
    this v1 doesn't do, so closures are rejected outright. Known,
    narrow gap: shadowing a top-level function's name with an unrelated
    local binding of the SAME name isn't seen through by this check.
  - **Callback params/return scoped to Int/Float/Bool** — no CStr,
    struct, or nested callback. A `void` callback return is spelled
    `-> Unit`. A callback TYPE is rejected as an extern function's own
    RETURN type (calling a C-supplied function pointer FROM Plum isn't
    implemented — only the reverse direction).
  - **Sound only for a C function that invokes the callback
    SYNCHRONOUSLY, during the call** — never one that stores the
    pointer for later (a signal handler, an event-loop registration).
    The generated trampoline's backing state (a `libffi::middle::
    Closure` + a `Box`-heap-allocated userdata struct holding a raw
    `*mut Interpreter`) only lives as long as the ONE `ExternCall` that
    created it; nothing enforces this from Rust's side, it's a hard
    invariant callers must uphold.
  - **Mechanism**: `libffi::middle::Closure` generates the real C
    function pointer; its callback fires a trampoline that reborrows
    the raw `*mut Interpreter` as `&mut Interpreter` and calls
    `Interpreter::call(fn_name, args)` — sound only because the outer
    `ExternCall` is BLOCKED inside the C call for the whole duration
    (never touching `self` concurrently), the one unsafe assumption
    this whole feature rests on. A Plum-side error OR a caught Rust
    panic (via `catch_unwind` — a panic must never unwind through the C
    call frame, that's undefined behavior) is recorded in the userdata
    rather than returned directly (a C function pointer has no error
    channel) and checked immediately after the outer call returns,
    taking precedence over whatever value that call produced.
  - Verified against a real reentrant round-trip (a Rust `extern "C"
    fn` invoking a Plum `add` function as a callback and getting the
    correct summed result back), a Bool-typed callback return, and
    callback-side error propagation surfacing as the outer call's own
    error.

**Self-hosting-viability is now a standing dependency-choice policy**:
before adding any new external Rust crate, ask whether it would paint a
future self-hosted Plum compiler/tooling into a corner. If a crate is
confined to solving a problem specific to the CURRENT Rust
implementation (like dynamic-signature FFI calls in a tree-walking
interpreter), it's fine even if a future self-hosted version would solve
the same problem differently — the crate just stops being relevant. A
crate whose specific *API shape* leaks into `plum-ir`/`plum-types`/the
AST is the actual risk to avoid, not "used an external crate at all."

Tests added at every layer across the whole FFI/unsafe effort (base
extern/unsafe support, then strings, structs, and callbacks as three
follow-on chunks): plum-syntax (the new `(A, B) -> R` type grammar,
including the plain-grouping and rejected-multi-type-without-arrow
cases), plum-ir (lowering + FBIP passthrough for every new node,
recursive struct/callback type resolution including the self-
referential-struct and nested-callback rejection cases), plum-types
(unsafe-gating accept/reject, every unsupported-type rejection,
extern/global name collision, void-return-is-Unit, the callback-
argument bare-top-level-function-only restriction), plum-interp (real
`sqrt`/`abs`/`strlen` calls through actual libffi, a real reentrant
callback round-trip verified against a Rust `extern "C" fn` standing in
for a C caller, real ABI struct layout verified against Rust's own
`#[repr(C)]` — including a deliberately padding-inducing field order —
and against real libc `div`/`div_t`), plumc (full gated pipeline, both
accepting and rejecting cases for every sub-feature). Workspace is now
1139 tests, clean build, zero warnings.

## Standard library — v1 started (basic output)

With `plum-codegen`'s LLVM backend covering essentially the entire core
language, concurrency, and FFI (see "Implementation plan" below), work
moved to the previously-open "Standard library scope" question. Started
by surveying the current codebase (not assuming): there is no `impl`/
method-block syntax anywhere (`ast::ItemKind` has exactly `Let`/
`Struct`/`Enum`/`Extern`/`Use` — no fifth variant), and every existing
dot-method (`.map()`, `.split()`, `.to_string()`, ...) is a hardcoded
AST-shape match in `lower.rs`, not user-extensible dispatch. So a Plum
standard library is necessarily a library of **plain, importable
functions** — `println(x)`, not `x.println()` — unless/until the
language grows real method syntax, which is its own, separate, larger
design question, not assumed as part of this work.

The survey also found a genuinely foundational gap: **there was no way
for a running Plum program to produce output at all.** The only output
mechanism was the host process printing the entry function's final
return value after the WHOLE program had already finished
(`printf`-with-format-string in `emit_main`; `println!("{value:?}")` in
the interpreter CLI). This blocked writing or testing almost anything
else standard-library-shaped, so it became the first piece.

**`println` needs no new compiler/backend builtin at all** — it's
ordinary Plum source:
```
extern "C" {
    fn puts(s: CStr);
}

let println[T] (x: T): Unit = unsafe { puts(x.to_string().as_cstr()) }
```
This works because two combinations, unverified by any existing test
before this chunk, both turned out to hold — confirmed empirically
(compiled and run for real) before committing to the design, not
assumed:
- `.to_string()` on a still-unresolved generic type parameter `T`
  works in BOTH backends. The interpreter already had a test proving
  this; the native/LLVM backend didn't, but the architecture predicted
  it should (monomorphization fully specializes a generic function's
  body — including any `.to_string()` call inside it — into a concrete
  instantiation before codegen ever runs), and a dedicated test now
  proves it directly, at two different concrete types in one compiled
  program.
- `unsafe { extern-call }` inside a still-generic function's body works
  with zero special-casing — the `in_unsafe` gate and monomorphization's
  own per-instantiation rewrite are both orthogonal to genericity.

`puts` is declared with **no return type** — infers as `Unit`
(`extern_function_with_no_return_type_is_unit` is a real, pre-existing
test for this), matching `plum_codegen::emit_program`'s own doc comment,
which already used `puts` BY NAME as its literal void-extern precedent
example. This makes `println` itself genuinely `Unit`-returning with no
"discard a non-Unit extern return value" question to answer. Scoped to
`println` alone for v1 (auto-appends a newline, exactly matching libc
`puts`) — a no-newline `print` needs `fputs`/a `stdout` `FILE*` handle
or C-variadic support, neither of which this codebase's extern
mechanism has any precedent for yet; left as a small, separate,
well-scoped follow-up rather than guessed at now.

**How it's exposed — a real, deliberate scope trade for v1**: merged
into `with_prelude` (`plumc/src/lib.rs`) as a new `STDLIB_IO_SRC`
constant, alongside the existing `PRELUDE_SRC` (`Option`/`Result`) —
`println` needs no `use` at all, exactly like `Option`/`Result` today.
This was a genuine fork, resolved with Brad rather than assumed: the
project's real module system (`use io;`, directory-as-module,
multi-file — see "Module system" above) already exists and works, and
would be the more properly "library-shaped" home for `println`. But the
EXISTING `compile_and_run` test harness — used by nearly this entire
workspace's codegen test suite — goes through `with_prelude` alone and
never touches the module system at all; routing `println` through a
real `use io;` module would have meant extending that harness to drive
a full temp project through `resolve_project` first, a bigger, separate
piece of work. Decided to unblock output NOW via the prelude mechanism,
and revisit "real, `use`-based stdlib modules" as its own later chunk
once there's more than one stdlib piece to justify the investment. Kept
as its own constant (not folded directly into `PRELUDE_SRC`'s string)
specifically so it can be deleted/moved wholesale into a real `io`
module later without having tangled its source into the `Option`/
`Result` sugar-type story.

**A real, separate bug found and fixed along the way, unrelated to
`println`'s own correctness**: the interpreter CLI (`plumc <project-dir>`,
no `build`) showed Plum-level `println` output AFTER the CLI's own
final `println!("{value:?}")` of the entry function's return value —
backwards from true program order. Root cause: libc's `puts` (called
via `libffi` from inside the interpreter) writes through C's OWN stdio
buffering, which is fully block-buffered whenever stdout isn't a TTY
(piped output, a test harness, etc.) — its writes only reach the OS at
actual process exit unless flushed explicitly. Rust's own `println!`
flushes on every newline via its own, separate buffering, so it reached
the terminal/pipe FIRST even though it executes SECOND in true program
order. Fixed with a single `fflush(NULL)` call (flushes every open C
stream) right before the CLI's own final `println!`, declared as a raw
`unsafe extern "C"` FFI import directly in `main.rs` — no new Rust
crate dependency, and the identical shape a future self-hosted Plum
compiler's own CLI driver would need regardless of implementation
language (matching this whole project's standing self-hosting-
dependency-avoidance policy). The native/`build` path never had this
problem: `emit_main`'s hand-written `main()` and every `puts()` call it
makes both run inside the SAME single process, and real process exit
already flushes every open C stream in true program order — there's no
separate Rust host process printing something else afterward.

Tests: 2 new native-codegen compile-and-run tests (`println` for every
`.to_string()`-supported type, asserting captured stdout is in the
correct order relative to the entry function's own final printed
return value; `.to_string()` on a still-generic type parameter in
isolation, at two concrete types in one compiled program) and 1 new
interpreter-path test (mirroring the existing "prelude type needs no
declaration" test style). Workspace now 1369 tests, clean build, zero
warnings.

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
  that's a portable guarantee. **v1 implemented** — see below.

### LLVM backend — v1-v14 implemented (scalars, control flow, tail calls, heap values, generics/monomorphization, arrays, core strings, closures (including inside generic functions), general array iteration, spawn/join, channels/select, full FFI including struct-by-value, Unicode-aware string ops, non-constant globals including ones calling a still-generic function)

`crates/plum-codegen` emits LLVM IR as TEXT (the `.ll` format) — no
`inkwell`/`llvm-sys` Rust binding at all. This machine has no
`llvm-config`/dev headers installed (only runtime `.so`s), and both
crates need a system-level LLVM install matching one specific major
version; text emission needs nothing beyond `std::fmt` and shells out
to `clang` (already present) to compile and link. This also fits the
self-hosting-viability policy (see the FFI section's writeup of that
policy): a future self-hosted Plum compiler would do the exact same
thing — generate text, shell to `clang` — so there's no Rust-crate-
specific API shape to ever "unwind."

**Scope**: scalars (`Int`/`Float`/`Bool`/`Unit`), `Var`, `Unary`,
`Binary` (including short-circuit `&&`/`||`, compiled as real
branching + `phi`, not a plain `and`/`or` instruction — must match the
interpreter's exact short-circuit semantics: the untaken side's code
must never execute), `Let`, `If`, plain named `Call`, struct/enum heap
values (`Ctor`/`CtorReuse`/`RcAnnotated`/`Match`, refcounted via a
small emitted runtime — see below), and — as of a follow-on chunk —
generic structs/enums/functions, monomorphized into concrete, mangled
instantiations before lowering (see "Generics/monomorphization"
below), and — as of a further follow-on chunk — arrays (literal,
index, len, push/pop/set/remove + FBIP's reuse-in-place variants) and
core, byte-level string operations (literal, len, byte indexing,
concat + reuse, `.contains()`/`.starts_with()`/`.ends_with()`,
`ToString` for `Int`/`Float`/`Bool`/`Str` — see "Arrays and core
strings" below), and — as of a further follow-on chunk — closures in
non-generic functions, including self-referential local closures,
higher-order calls through a closure-typed value, and a bare
top-level function name used as a value (see "Closures" below), and —
as of a further follow-on chunk — `For` (range- and array-based) and
`Assign`, and therefore `.map()`/`.filter()`/`.fold()` (which desugar
purely into `For`+`Assign`+ordinary `Call` — see "General array
iteration" below), `spawn { block }`/`.join()` (real OS-thread
concurrency), and — as of a further follow-on chunk — `channel[T]()`/
`.send()`/`.recv()`/`select` (see "Concurrency: channels/select"
below; disconnect detection is explicitly NOT part of this, and at
most one distinct `channel[T]()` element type `T` is supported per
program — see that section), and — as of a further follow-on chunk —
`extern "C"` calls, `CStr`, and C callbacks (see "FFI: scalar extern
calls, CStr, callbacks" below), and — as of a further follow-on chunk
— struct-by-value FFI marshaling (see "FFI: struct-by-value marshaling"
below), closing out FFI entirely, and — as of a further follow-on
chunk — `.runes()`, `.trim()`, `.replace()`, `.split()` (full Unicode
correctness) and `.to_upper()`/`.to_lower()` (full Unicode SIMPLE case
mapping via libc `towupper`/`towlower` — see "Unicode-aware string
operations" below for the mechanism and its one remaining, narrowly-
scoped gap), non-constant top-level `Global` initializers, including
one that calls a still-generic function (see "Non-constant Global
initializers" below for both), and — as of a further follow-on chunk —
closure literals inside a still-generic function's own body, working
correctly and independently per concrete instantiation (see "Closures
inside generic functions" below). Still out of scope, but NOT a
codegen error — a silent, documented pass-through instead (see
"Unicode-aware string operations" below): multi-codepoint Unicode case
expansions (e.g. German `ß`→`"SS"`) leave the input unchanged, since
`towupper`/`towlower`'s 1-in-1-out C signature cannot produce them.
Still out of scope and producing a clear codegen error, never a panic:
an `Assign` inside a closure body writing back into an enclosing
loop's carried variable (structurally out of reach of this backend's
closure design, not merely unimplemented), an `Assign` reachable only
through a `Let`/`If`/`Match`/`RcAnnotated` used in an ordinary value
position (e.g. `f({ sum = sum + 1; sum })` as a `Call` argument),
disconnect detection on channels, more than one distinct `channel[T]()`
element type per program — and a generic instantiated at any of these
still-unsupported types (e.g. `Box[Array[Str]]` once `.split()` is
needed).

**Guaranteed tail calls**: any call in tail position (the function's
own body; both branches of a tail-positioned `If`/arms of a tail-
positioned `Match`; a `Let`'s `body`/an `RcAnnotated`'s `rest`) to a
directly-named function — self- OR mutual-recursive — compiles to
`%r = musttail call <ty> @callee(...)` immediately followed by
`ret <ty> %r`, the exact shape LLVM's `musttail` requires, PROVIDED
the caller's own function prototype matches the callee's exactly — a
real LLVM constraint discovered via an actual `clang` compile failure
("cannot guarantee tail call due to mismatched parameter counts"), not
documented up front: `musttail` is only valid between functions with
identical signatures, not "any tail call to a directly-named
function" as originally assumed. A tail call to a different-shaped
function (e.g. a zero-arg entry point tail-calling a two-arg
accumulator) falls back to an ordinary `call` + `ret` — still correct,
just not `musttail`-guaranteed to reuse the stack frame at `-O0` (LLVM
may still eliminate it as a best-effort sibling call under `-O2`).
Verified as a REAL, unconditional guarantee where it DOES apply (not
just an optimizer hint under `-O2`): a compiled 1,000,000-deep self-
recursive accumulator and a 1,000,001-deep mutual-recursion pair (both
same-signature) run correctly at `-O0`, which would stack-overflow
without genuine tail-call elimination.

**Type handling**: `ir::Function`/`ir::Global` carry no type
information at all (`ir::Type` is vestigial/unused). `plum-codegen`
defines its own minimal `CgType { Int, Float, Bool, Unit, Heap }` (not
reusing `plum_types::Type`, keeping the crate self-contained and
testable in isolation with hand-built `ir::Program` values, matching
every other crate's own precedent) and expects a `HashMap<String,
FnSig>` (function signatures) plus a `TagFields` map (every non-
generic struct name/enum-variant name -> its fields' `CgType`s) —
supplied by the caller. `plumc::compile_and_run` derives `FnSig` from
`Infer::infer_program`'s own results (the only place real type
information exists in the pipeline) and `TagFields` from
`TypeContext::struct_fields`/`variant`, restricted to non-generic
types only (see "Heap values" below). Within a function body, codegen
does its own simple bottom-up type-propagation walk as it emits
instructions — not full Hindley-Milner (the program already passed
real type-checking before reaching codegen), just "what LLVM type does
this node need."

**Heap values (non-generic structs/enums, refcounting, `Match`)**:
one uniform cell layout for every struct/enum-variant, `{ i64
refcount, i64 tag, i64 fields[N] }` — every field slot is a raw 64-bit
word regardless of its own type (`Int`/`Bool` direct, `Float` via a
bit-preserving `bitcast` not a numeric conversion, a nested heap
pointer via `ptrtoint`/`inttoptr`), so codegen never needs a distinct
LLVM struct type per Plum type. Every distinct tag gets a compile-
time-interned small integer; `Match` dispatch and the runtime's own
recursive-field-release logic are both a plain sequential `icmp`+`br`
chain, never an LLVM `switch` (`Match`'s real semantics — arms tried
in order, the SAME tag may appear in more than one arm with different
guards — don't map onto `switch`'s one-label-per-case-value shape
anyway). Four small runtime functions are emitted as TEXT into every
program's own `.ll` output (no separate hand-written runtime file at
all, consistent with this whole backend's style and the self-hosting-
viability policy): `@plum_alloc`, `@plum_rc_inc`, `@plum_rc_dec`
(frees + recursively releases heap-shaped fields at refcount zero),
`@plum_release_fields` (the shared recursive-release logic, also used
by `CtorReuse`'s in-place-overwrite path without a `free` call).
`CtorReuse` (FBIP's ALREADY-COMPUTED reuse-in-place decision) branches
on the old cell's refcount at runtime: if it hit zero, releases the
old fields and overwrites in place; otherwise falls back to an
ordinary fresh allocation — FBIP already guarantees matching field
counts whenever it emits this node, so the in-place path never needs
a realloc. A `Match` arm's bound field gets `@plum_rc_inc`'d if
heap-shaped — a deliberate FIX over the interpreter's own established
(if accepted) gap, where a bound field is implicitly borrowed from its
scrutinee with no refcount bump.

**Generics/monomorphization**: `plum-ir`'s lowering fully ERASES type
parameters (no type checker existed originally, and `ir::Function`/
`ir::Ctor` still carry zero type information by design — the
interpreter never needed this fixed, since it's uniformly
dynamically-tagged at runtime and `List[Int]`/`List[Bool]` produce
byte-identical heap cells). LLVM codegen can't get away with that:
every SSA register has one static LLVM type, and — the crux fact that
makes this a real monomorphization pass rather than just "allow
generics through" — codegen's uniform heap-cell scheme picks a
different STORE/LOAD bit-conversion per field depending on that
field's concrete `CgType` (direct for `Int`, zext/trunc for `Bool`,
bitcast for `Float`, ptrtoint/inttoptr for a nested heap pointer), so
two different instantiations of the same generic declaration whose
fields need different conversions (`List[Int]`'s `Cons` vs
`List[Bool]`'s `Cons`) cannot share one tag — they need distinct,
mangled tags/function names.

A new `plum-ir/src/monomorphize.rs` pass runs before lowering, in the
codegen-only pipeline (the interpreter's pipeline never calls it, so
it's completely untouched). It consumes a new capability added to
`plum_types::Infer`: every generic construction/pattern/call site
encountered during inference is recorded (as a `RawSite`) alongside
which top-level function's body it's nested in, if any; once whole-
program inference finishes, `Infer::resolve_generic_sites` resolves
each one against the final accumulated substitution into either a
fully concrete argument list, or — for a site nested inside another
still-generic function's own body, never pinned down by that
function's own (generic, inferred-exactly-once) body inference — a
`Type::Param` TEMPLATE referring to that enclosing function's own
declared generic parameter. (A type parameter that's never pinned to
a concrete type ANYWHERE it's used is a genuine new static check: a
clear type error, not silently accepted.) `monomorphize::plan` then
runs a fixpoint worklist over every reachable `(declaration, concrete
args)` pair — seeded from every already-concrete site plus every
ordinary non-generic function — discovering deeper instantiations as
it rewrites each reachable generic function's body (substituting any
template `Param` through that specific instantiation's concrete
binding via a clone-and-rewrite pass over the AST, then handing the
rewritten clone to `lower.rs`'s ordinary, otherwise-unmodified lowering
functions). Termination is guaranteed: `unify.rs`'s occurs check
already rejects any self-recursive function/type whose own use would
require a type strictly containing itself, so the set of reachable
instantiations is finite by construction, independent of
monomorphization itself.

Mangling: `$` is not a legal Plum identifier character, matching the
existing precedent of synthetic tags using identifier-illegal
characters (`tuple_tag`, `RANGE_TAG`, `DEFAULT_ARM_TAG`) — a mangled
name (`Cons$Int`, `identity$Bool`, `Pair$Int$Bool`) can never collide
with a real user identifier, and a non-generic name mangles to exactly
itself, so this whole pass produces a ZERO output diff for any program
that never uses generics at all. `plumc::codegen_cli` merges the
resulting mangled `tag_fields`/`signatures` into its existing non-
generic derivation (disjoint by construction) and fully REPLACES
`lower_program`'s own function list with the monomorphization pass's
output — an *ordinary* (never-generic) function whose body merely
*uses* a generic type still needs its own body re-lowered with mangled
tags, since its plain-tagged body would otherwise reference tags
`tag_fields` has no entry for. An entry point naming a generic function
with more than one reachable instantiation is rejected with a clear
"ambiguous entry point" error (compile a concrete, non-generic wrapper
function instead) rather than silently picking one.

**Arrays and core strings**: neither fits the `{ i64 refcount, i64
tag, i64 fields[N] }` Ctor scheme above — `N` there is required to be
a compile-time-fixed property of the TAG, but a string's raw byte
buffer isn't `Vec<CgType>`-shaped at all, and an array's length is
runtime-variable. Two new, genuinely separate heap-cell layouts:
strings are `{ i64 refcount, i64 len, i8 bytes[len] }` (one extra
trailing `\0` always kept in sync, purely so `plumc`'s own `emit_main`
can `printf` a `Str`-returning test result via `%s` — Plum's own
`.len()` never counts it; not language-visible); arrays are `{ i64
refcount, i64 len, <elemTy word> elements[len] }`, structurally
identical in shape to a `Ctor` cell (refcount + one more `i64` + N
words), so the existing `field_byte_offset`/`store_field_word`/
`load_field_word` helpers are reused unchanged, just called with a
runtime-variable index via new sibling `store_array_elem`/
`load_array_elem` functions instead of a compile-time-bounded one.
Two new top-level `CgType` variants, `Str` and `Array(Box<CgType>)`
(parallel to `Heap`, not a sub-case of it — unlike a struct/enum,
which needs `tag_ids`/`tag_fields` side tables because one `Heap`
pointer type covers many different runtime tags, an array/string's
`CgType` is precise and complete on its own and needs zero runtime tag
dispatch).

An array's element `CgType` is deliberately NOT resolved via the
monomorphization worklist above — `Array[T]` is representationally a
pseudo-generic `Type::Struct("Array", [T])` to `plum_types`, but
`Infer::record_site` (what `resolve_generic_sites` depends on) is
never invoked for it, since the builtin pseudo-generics (`Array`/
`Task`/`Sender`/`Receiver`/`Ref`) are deliberately excluded from
`TypeContext` in the first place — and arrays have no TAG to mangle
anyway (`ARRAY_TAG` is one constant for every instantiation), so
there's nothing for `TagFields`-style per-mangled-tag output to
produce. Instead, element `CgType` is recovered through three narrow,
independent paths that together cover every site codegen ever touches
an array at: a function signature's `Array[T]` position (`plum_type_to_
cg_type` now recurses into `Struct("Array", [elem])`, fixing a latent
bug where it previously silently mapped to plain `CgType::Heap`); a
non-empty literal or any op with a value operand (derived structurally
from that operand's own already-computed `CgType` — no lookup needed);
and reading an existing array (derived from its own already-known
`CgType::Array(elemTy)`, threaded through `Env` like any other value).
The one gap — an empty literal `[]` has no operand to derive `elemTy`
from — is closed by a new IR node, `ir::Expr::EmptyArray(PrimTy)`
(`PrimTy` a small new plum-ir-local enum, independent of both
`plum_types::Type` and `plum_codegen::CgType`), produced by lowering
only for an empty `ArrayLiteral`, using one new span-keyed `Infer` side
channel (`empty_array_elem_types`) mirroring the existing `field_
owners`/`array_for_loops` precedent exactly. (Known narrow gap: this
side channel isn't threaded through `monomorphize.rs`, so an empty
array literal inside a *generically monomorphized* function body still
lowers to the old untyped shape and produces a clear codegen error —
not yet fixed, consistent with this whole area's incremental scoping.)

Array reuse-in-place (`ArrayPush/Pop/Set/RemoveReuse`, FBIP's already-
computed decisions) leans on a declared libc `@realloc` to encapsulate
the actual grow-or-shrink-in-place-or-move decision at the `malloc`/
`free` layer, rather than hand-rolling it the way `CtorReuse` does —
refcount==1 is still checked first (a shared array must never be
`realloc`'d, since other holders would dangle), and `realloc` doesn't
preserve refcount across the call, so it's explicitly restored to 1
after. A real correctness gap neither the plan nor any upstream pass
anticipated, found and fixed during implementation: every FRESH-
allocation array op that `memcpy`s a whole buffer of elements into a
new cell creates a SECOND, independently-owned reference to every
heap-shaped element it copied — no upstream FBIP pass tracks this (it
only reasons about whole heap-cell *variables* via last-use analysis,
never individual array *slots* surviving a bulk copy) — so codegen
increments every copied heap-shaped element itself
(`inc_copied_array_elements`), or the shared element would eventually
be decremented twice once both arrays are released, a real double-free
rather than an accepted leak. The one place this needs a decrement
instead — `ArraySetReuse`'s overwritten slot, `ArrayPopReuse`'s
dropped tail, `ArrayRemoveReuse`'s dropped middle element on a
PROVABLY-uniquely-owned (refcount was 1) cell — is handled by a
parallel `dec_array_element_at`. Final release (the array analogue of
`@plum_release_fields`) needs no runtime tag dispatch at all, unlike
structs/enums: since an array's element `CgType` is always statically
known at every codegen site touching it, one dedicated
`@plum_rc_dec_array_<mangled_elemty>` is emitted per **distinct**
element `CgType` that actually appears anywhere in the compiled
program — a genuine simplification over the struct/enum scheme's
runtime `icmp`-chain dispatch.

`Index`/`.len()` are genuinely shape-shared IR nodes (byte-indexing a
`Str` vs. element-indexing an `Array`, no static hint at the node
itself) — dispatch is a simple match on the already-known `CgType` of
the value being indexed, once `Str`/`Array` exist and are threaded
through `codegen_value`'s return type; no new type information is
needed beyond what codegen already tracks. `ToString` dispatches the
same way: `Int`/`Float` via a declared libc `@snprintf` into a stack
buffer copied into a fresh string cell, `Bool` via two static string
constants, `Str` via a genuine fresh copy (never reuse-in-place, per
`ir::Expr::ToString`'s own "always allocates" contract), anything else
a clear `Err` — enforced STATICALLY here (stronger than the
interpreter's necessarily-dynamic runtime-tag check), since a value's
`CgType` is always known structurally at codegen time.

Bounds/emptiness checks are new ground for codegen — no runtime-
checked-failure mechanism existed before this (`Match`'s `unreachable`
is a compile-time exhaustiveness fact, not a runtime check). One
shared `@plum_abort(ptr %msg)` helper (`printf`s the message, `exit(1)`
via a declared libc `@exit`) backs every `a[10]`-on-a-length-3-array-
style check, compiling to an ordinary `icmp`+`br` to either a `fail`
block or a `continue` block — the honest, minimal compiled-binary
equivalent of the interpreter's own `Result<_, String>` hard-error-out
behavior. `CStr` stays out of scope (it has no operations besides FFI
boundary crossing, and FFI as a whole isn't in codegen's scope yet —
adding it now would be dead machinery with zero reachable consumers).

**Closures (non-generic functions only)**: `ir::Expr::Closure` carries
no captured/free-variable list — lowering is purely structural, and
neither it nor FBIP compute one (FBIP's own `Closure` handling only
ever forces the OUTER binding conservatively live, via
`mark_last_uses`, whenever a closure body might still reference it —
it does not compute or track a capture set for the closure itself).
Free-variable analysis is therefore entirely new, codegen-side work: a
full structural walk of a closure's body collecting every
`Expr::Var` present in the enclosing `Env` (excluding the closure's
own params and bare top-level function names, neither of which is in
`Env`), deduplicated and sorted by name for reproducible `.ll` output
— this order fixes the capture cell's own field-index assignment.

A closure value is a heap cell with a **3-word header** — `{ i64
refcount, i64 code_ptr, i64 release_fn_ptr, i64 captured[N] }` —
deliberately one word WIDER than the `Ctor`/array cells' 2-word
header (`field_byte_offset` needs a genuinely separate closure-
specific variant, `24 + index*8`, not `16 + index*8`; an initially
simpler-looking "just reuse Ctor's offsets" idea turned out wrong once
worked through). The reason: unlike an array (whose release logic is
fully determined by its element `CgType` alone), two different `if`
branches can produce two DIFFERENT closure literals — different
capture layouts — both flowing into the same `CgType::Closure(params,
ret)`-typed value at a control-flow join, so release must be resolved
via a function pointer stored IN the cell, not derived from the
static type. `CgType::Closure(Vec<CgType>, Box<CgType>)` therefore
carries only enough to know how to CALL through a value (the indirect
call's signature annotation), never its capture layout. One dedicated
allocator `@plum_alloc_closure`, and one genuinely uniform
`@plum_rc_dec_closure(ptr)` runtime function — dec, and at zero, load
and indirectly call whichever release function this particular cell
stored, then `free` — the same "one shared function, runtime-dispatch
via a pointer stored in the cell" shape `Heap`'s own `@plum_rc_dec`
already uses (a closer analogy than array release turned out to be).

Each closure LITERAL SITE gets its own generated LLVM function pair:
`@closure$<fn>$<K>` (the body — loads captures back out of `%env` by
index into a fresh, NOT-inherited `Env`, real lexical scoping) and
`@closure_release$<fn>$<K>` (dec's every heap-shaped capture, then
`free`s). **Captured heap-shaped values are properly refcounted** —
inc'd at capture time, dec'd by the per-site release function — a
deliberate fix over the interpreter's own accepted leak (it never
refcounts captures at all), matching the v2 precedent of making the
compiled backend correctness-strict where the interpreter accepts a
gap. Proven by two tests: a heap-captured struct's field is still
correctly readable from inside a closure body reached only through an
INDIRECT call from a genuinely separate function (rules out
use-after-free), and a closure created inside one function, returned,
and called only after that function's own stack frame is gone still
works (rules out stack-frame dependence) — this exact escape pattern
was untested anywhere in the project, interpreter included, before
this chunk.

**Calling through a closure value**: a bare `Expr::Var(name)` routes
through the ordinary direct-call path only when `name` names a known
top-level function AND isn't shadowed by a local in `Env` (`Env`, not
the signature table, is authoritative for what a bare identifier
resolves to — a local variable sharing a top-level function's name
must go through the indirect path). Otherwise, `codegen_value(callee)`
must yield a `CgType::Closure`; the code pointer is loaded and called
indirectly, with the environment pointer as an implicit first
argument. **`musttail` is deliberately never attempted for indirect
closure calls** — the existing mechanism compares two known, named
`FnSig`s; an indirect target's signature is only known via
`CgType::Closure`'s call-shape, a different and weaker check, and
indirect `musttail` has its own unexercised LLVM legality constraints
— always falls back to an ordinary `call`, even in tail position. A
self-referential closure's own recursive call is therefore NOT
guaranteed-tail-call-eliminated yet — a real, documented limitation.
Direct calls to plain named functions are completely unaffected.

**Self-referential local closures** (DESIGN.md's own decided surface
semantics above: self-reference only, no mutual recursion, no `move`
keyword): the cell is allocated and bound into `Env` under its own
name BEFORE its other captures are stored — sound because the cell's
address is an ordinary known SSA value the moment it's allocated, well
before anything needs to write it as a captured word. The
self-capture slot's `plum_rc_inc` is **deliberately skipped** (and,
found and fixed during implementation, its paired dec in the
per-site release function must be skipped too, or the true refcount
is under-counted) — incrementing it would create a reference cycle
the cell could never reach zero to escape: a genuine, deliberate,
documented leak, matching the same "leak over unsoundness" precedent
already used for `For`/`Closure`/`Spawn` captures elsewhere in
`fbip.rs`. A bare top-level function name used as a VALUE (not
called) — not eta-expanded upstream, confirmed by reading `lower.rs`
directly (only non-zero-arity enum-variant constructors get that
treatment) — is wrapped in a generated trampoline closure with zero
captures, memoized per distinct function name so repeated references
don't duplicate work.

A closure literal inside a still-generic function's own body now works
correctly, per-instantiation (see "Closures inside generic functions"
below) — the pre-check that originally rejected this outright has
been removed.

Getting a closure literal to codegen at all required a real upstream
addition beyond what was originally scoped: `ir::Expr::Closure`
carried no param/return type information whatsoever (unlike every
other IR node, whose types are structurally derivable) — a new
`Infer::resolve_closure_types` span-keyed side channel (mirroring the
`empty_array_elem_types` precedent exactly) and two new optional
fields on `ir::Expr::Closure` were needed just to make a closure
literal's own signature knowable at codegen time.

**Fixed gap**: `fbip.rs`'s `Closure` handling originally predated real
capture refcounting (written when the interpreter — the only backend
that existed at the time — never refcounted captures at all) and
inserted an `Inc` at every mention of a captured heap-shaped name
INSIDE the closure body itself, not just once at capture time, with no
matching `Dec` ever emitted for it. Combined with codegen's own
correct capture-time inc, a heap-captured value used inside a
REPEATEDLY-called closure would accumulate one unmatched extra
increment per call — an unbounded leak. Fixed by no longer recursing
`mark_last_uses` into a closure's body for a captured name at all
(only detecting whether it's mentioned, to keep forcing the OUTER
binding's `live_after` correctly) — a captured name's lifetime inside
the body is owned by the closure cell now, not this outer walk.
Verified safe for BOTH backends before implementing, not assumed:
traced that `fbip.rs` only ever emits a `Dec` from one place (`Let`'s
"name never referenced at all" case, gated on `used == false`), which
a closure capturing a name always makes unreachable for that name —
so no path, before or after the fix, ever `Dec`'d a captured name's
original reference while an escaping closure held it; the fix only
removes redundant, unmatched `Inc`s, in the interpreter as well as
codegen (both share this one FBIP pass via one unparameterized call
site).

**General array iteration (`For`, `Assign`, `.map()`/`.filter()`/
`.fold()`)**: neither `Expr::For` nor `Expr::Assign` had ANY codegen
implementation before this chunk (not just range-only `For` as
earlier chunks' scope notes implied — both fell to the catch-all
error). `for x in arr { ... }` with no mutation only needed `For`
(lowering already desugars it into an ordinary index-based range
loop, `Let{arr, ArrayLen/Index-based body}` — no new IR primitive);
the accumulator idiom (`sum = sum + x`) and therefore `.map()`/
`.filter()`/`.fold()` (which desugar purely into `For`+`Assign`+
ordinary `Call`/`ArrayPush`, confirmed NOT built via any closure-
specific machinery — `f` is just an ordinary `Call` callee, the same
mechanism closures already compile through) also needed `Assign`.

Implemented via SSA phi-threading, deliberately NOT stack allocation
— keeps this backend's "everything is SSA registers + heap cells,
never mutable stack memory" character intact, and generalizes a
pattern already in use rather than introducing a second one.
`codegen_expr`'s return type gained a resulting-`Env` component
(`Result<(Option<(String,CgType)>, Env), String>`) — every existing
arm just returns the SAME env it was given (provably behavior-
preserving); only `Assign` (one entry overridden) and `For`
(loop-carried names' post-loop registers) produce a different one. A
new `merge_envs` helper generalizes `If`/`Match`'s EXISTING
branch-value phi-merge to also phi-merge any `Env` entry whose
register diverged between branches — this alone is what makes
`.filter()`'s `Assign`-inside-`If` shape work correctly, with no
separate detection walk needed for `If`/`Match` themselves (divergence
is found by diffing envs directly). A new `assigned_vars`/
`assigned_vars_scoped` structural walk (the write-target sibling of
the closures chunk's `free_vars`/`free_vars_scoped`) finds which
outer-scope names a `for` body reassigns; `Closure` is a deliberate,
structurally-necessary hard stop (a closure compiles to a wholly
separate top-level `define` with its own fresh, byval-captured `Env`
— there is no point in program order where "write back into the
loop's phi" could have a coherent meaning, since the closure can
escape and be called after the loop, even after the enclosing
function returns); nested `for` loops are NOT a stop point, so nested
accumulators work for free (each loop level independently phi-merges
a shared accumulator). Two new small `Emitter` primitives,
`reserve_line`/`patch_line`, exist because a loop header's phi needs
an operand (the body's final register) not known until AFTER code
that must textually precede it (the header's own `icmp`/`br`, which
is what jumps into the body) has already been emitted — the one
genuinely new control-flow shape this introduces; `If`/`Match` never
needed this since their merge block is entered exactly once, after
all operands are already known. `codegen_for`'s block structure
(preheader/header/body/body_end/after) is built so `for_after` is
reachable ONLY via the header's own conditional branch — no other
edge into it — making the header provably dominate the exit path, so
every phi register (carried vars + the induction variable) is valid
past the loop. `Assign` codegen mirrors `Let`'s own arm structurally
and deliberately emits NO `Dec` for the value being overwritten,
matching `fbip.rs`'s own already-accepted-leak stance for
reassignment (not a gap left unfixed — inventing a `Dec` here would
be inconsistent with what FBIP already decided).

A real, separate blocking bug was found and fixed as a prerequisite:
`lower_array_map`/`lower_array_filter` built their empty output
accumulator as `Ctor{ARRAY_TAG, []}` rather than `EmptyArray(PrimTy)`
— hitting `codegen_array_literal`'s existing "empty fields" error.
Fixed in `lower.rs` to emit `EmptyArray(elem_ty)`, sourcing the
element type from the mapped/filtered closure's own resolved type via
the `closure_types` side channel (with a `PrimTy::Heap` fallback for
lowering-only test paths that never run `Infer` at all, preserving
every pre-existing interpreter-only `.map()`/`.filter()` test).

A real deviation from the original design, found via an actual
failing test (`for x in arr` produced `0` instead of the correct
sum) rather than assumed: the initial `Let`-value handling only
special-cased a value that was DIRECTLY `For`/`Assign`, but
`lower_for`'s own desugaring wraps the whole loop in `Let{arr_name,
.., body: For{..}}` — a `Let` whose value contains a `For`, not a
bare `For` itself — which the narrow guard missed, silently dropping
the loop's env changes through `codegen_value`'s existing env-
discarding fallback. Fixed by generalizing: every `Let` value (except
the pre-existing `Closure` self-reference special case) now routes
through `codegen_expr` rather than `codegen_value`, so whichever arm
`value` itself resolves to threads its own env correctly with no
per-shape special-casing — simpler than the original design and
verified against the real failure, not assumed correct.

**Known, deliberately out-of-scope gap**: an `Assign` reachable ONLY
through a `Let`/`If`/`Match`/`RcAnnotated` used in an ordinary VALUE
position (a `Binary` operand, `Call` argument, `Ctor` field — e.g.
`f({ sum = sum + 1; sum })`) still has its env effects discarded by
`codegen_value`'s own signature, which deliberately stayed unchanged.
No construct required by this chunk's own scope reaches this path.

**`plumc build` CLI (real, persisted native binaries)**: codegen was
reachable only from `codegen_cli.rs`'s own tests until this chunk —
`main.rs` had zero subcommand parsing (`plumc <dir>` unconditionally
interpreted; no flags at all). New: `plumc build <dir> [-o <output>]`
compiles+links a real native executable via the same module-resolution
pre-pass the interpreter CLI uses (`resolve_project`, above) — pure
plumbing, not new compiler capability, since codegen's own pipeline
never assumed single-file input. `compile_to_ir` split into a
parse+prelude shim plus a new `pub fn compile_program_to_ir(program:
&ast::Program, entry_fn)`, so a module-resolved `Program` never gets
double-prelude-injected. The compiled binary always prints `main`'s
return value and exits 0 — deliberately mirroring the interpreter
CLI's own `println!("{value:?}")`, not real Unix exit-code semantics
(a `main` returning `Heap`/`Array`/`Closure` is a clear build-time
error via a shared `reject_unprintable_return` check, not a panic —
printing those shapes needs a compiled `ToString`-style dispatcher
that doesn't exist yet, real follow-up work not this chunk's job). A
real, previously-undiscovered symbol collision was found and fixed:
`plumc build`'s fixed entry-point name IS `"main"`, and codegen emits
every function under its literal unmangled Plum name with no
namespacing — so a Plum-level `main` compiles to LLVM symbol `@main`,
colliding with the hand-written native C `@main` wrapper (`clang`:
"invalid redefinition of function 'main'"). Fixed by textually
renaming `@main(` → `@__plum_entry_main(` in the generated IR body
when the resolved entry is literally `"main"` — safe since Plum's
lexer never produces `__`-prefixed identifiers (the same precedent the
module system's own qualified-name scheme already relies on) and
`@name(` only ever appears at that function's own define/call sites in
codegen's generated text. Default output path (no `-o`): the project
directory's own basename, written to the current working directory
(the `go build`/`cargo build --bin` convention), falling back to
`"a.out"` if the directory has no filename component.

**Concurrency: `spawn`/`.join()`** (channels/`select` deferred to a
separate chunk). Real OS threads (`pthread_create`/`pthread_join` via
libc declarations), matching the interpreter's own `std::thread`-based
implementation and DESIGN.md's own "Scheduler — Open, deliberately
sequenced last" decision (ship on plain OS threads first; a real
green-thread scheduler is later performance work, not a v1
requirement). Unlike most gaps documented elsewhere in this backend,
a bug here would be a genuine data race/UB on a non-atomic refcount
word, not merely an accepted leak — DESIGN.md's own "Implementation
blocker: heap ownership across tasks" section decided a spawned task's
captures cross via DEEP COPY specifically so no two threads ever touch
the same non-atomic `i64 refcount` field; codegen preserves this
exactly. A genuine simplification over the interpreter, confirmed
directly rather than assumed: plum-interp needs a `PortableValue`
serialization format because its own hand-rolled `Heap` (a
`Vec<Option<HeapCell>>`) isn't thread-safe and each task needs a wholly
separate one; plum-codegen has no such structure to begin with — every
heap cell is an ordinary `malloc`'d pointer, and glibc's `malloc`/
`free` are already thread-safe for concurrent allocation, so "deep
copy" is simply: recursively allocate fresh cells via the same
`@plum_alloc`/`@plum_alloc_str`/`@plum_alloc_array` functions already
used everywhere else and copy data into them — no marshaling format
needed at all.

`CgType::Task(Box<CgType>)` is deliberately NEVER refcounted — FBIP's
`is_syntactically_heap` never treats a `Task`-bound name as heap-
tracked, so codegen matching that (`dec_fn_for` returns `None` for it)
is required, not optional, or the two passes' assumptions would
diverge into a real bug. The task cell is a plain, unrefcounted
16-byte block (`{ i64 joined, i64 pthread_id }`), allocated via bare
`@malloc`, not `@plum_alloc` — `pthread_t` is confirmed a plain 8-byte
scalar on this platform (`unsigned long`), storing directly as an
`i64` with no opaque-struct handling needed (this backend already ties
itself to the local platform/ABI by shelling out to `clang`).

Deep-copy runtime functions mirror `@plum_release_fields`'s existing
runtime-tag-dispatch shape exactly (`@plum_deepcopy_heap`, emitted
once per program, only if `spawn` is used anywhere) rather than
inventing new dispatch — `CgType::Heap` is already opaque everywhere
else in this backend, so deep-copying it already needs the same
runtime tag chain. `@plum_deepcopy_str` allocates fresh + `memcpy`s
(no recursion, strings have no nested heap fields); one
`@plum_deepcopy_array_<mangled>` per distinct array element `CgType`
actually captured, extending the existing `needed_arrays` discovery
machinery already used for array release functions.

A captured free variable whose `CgType` is `Closure(..)`/`Task(..)` —
including nested inside an `Array` — is a clear compile-time `Err`,
matching the interpreter's own restriction (`to_portable` rejects
closures, bare function values, and other task handles as non-
portable). `Ref[T]` needs no check: confirmed it has zero codegen
representation today, so a `Ref`-typed value can never reach a live
`Env` entry to begin with. A SEPARATE, deliberately conservative
whole-program check rejects any program using `spawn` at all if ANY
declared struct/enum anywhere has a closure/task-shaped field, even if
that exact tag is never the one actually captured — necessary because
`CgType::Heap` is opaque at the capture site (nothing there can see
"which" struct a captured value actually is), so the only sound place
to catch a deeply-nested closure/task field is a structural scan over
every declared tag's fields, once, whenever `spawn` is used.

One generated thread-entry function per `spawn` literal site
(`@spawn$<fn>$<K>`, mirroring the closures chunk's per-literal-site
precedent), with the exact `pthread_create` C ABI. `.join()` needs NO
second deep-copy on the way out — a real simplification over the
interpreter, justified by an explicit happens-before argument: the
child's boxed result is exclusively owned by the child at return time
(ordinary FBIP-correct return-value semantics), the child terminates
immediately after, and `pthread_join` is a POSIX-guaranteed
synchronization point — there is no window where two threads could
concurrently touch the result box. The joiner simply adopts the
returned pointer directly. A second `.join()` on the same handle is
caught via the cell's own `joined` flag + a runtime abort (confirmed
NOT statically enforced by `movecheck.rs` — codegen needs its own
check).

Verified via an actual `-fsanitize=address` run on a 1000-task scale
test (not just inspected by eye) — this genuinely caught a real bug
during implementation (a leaked spawn-args block, ~1000 leaked
allocations, one per task) before it was fixed; after the fix, the
same ASan run passes cleanly with leak detection specifically disabled
to isolate corruption/UAF/double-free detection from plain leaks.

**Known, deliberate, accepted leak** (not a soundness gap): a spawn
capture's deep copy is never explicitly released once the entry
function is done with it — `fbip.rs`'s `mark_last_uses` forces
`live_after=true` for the whole walk into a `Spawn`'s block (mirroring
`Closure`'s identical treatment), so no `Dec` is ever emitted for a
captured name's uses inside it, and unlike a closure cell (whose
release function eventually decrements its captures), a spawn's deep
copies live only in the entry function's own registers with nothing to
trigger their release. Fixing this needs real last-use analysis inside
a spawned block (to distinguish "used, then releasable" from "returned
directly as the block's own result" — e.g. `spawn { p }`, where an
unconditional release would free the very cell about to be returned, a
real use-after-free) — matches this codebase's repeatedly-established
"accepted leak over unsoundness" precedent, deferred rather than
attempted under this chunk's stated correctness priority.

**Concurrency: channels/`select`** (disconnect detection explicitly
NOT part of this — `send()` always succeeds, `.recv()`/`select` block,
potentially forever, if nothing is ever sent, rather than replicating
the interpreter's Arc/Drop-based disconnect errors; the underlying
queue/mutex/condvar is a permanent, accepted leak, same precedent as
`spawn`'s own captures). Builds directly on `spawn`/`.join()`'s
toolkit: `CgType::Sender(Box<CgType>)`/`CgType::Receiver(Box<CgType>)`,
neither refcounted (`dec_fn_for → None` — there is no refcount word
anywhere in the shared queue struct's layout; treating one as
refcounted would corrupt the mutex sitting at offset 0) nor deep-copied
when crossing a thread boundary (`deepcopy_fn_for → None` for a THIRD,
different reason than `Closure`/`Task`: a `Sender`/`Receiver`
legitimately crosses — unlike those two — but as a VERBATIM POINTER
COPY, since both ends must keep pointing at the SAME shared queue or
the channel silently splits into two mutually-invisible halves).
`crosses_spawn_boundary` was renamed `crosses_thread_boundary` and
reused (not duplicated) at the new channel-send call site.

One `malloc`'d, permanently-leaked 104-byte queue struct per
`channel[T]()`: `{ [40 x i8] mutex, [48 x i8] cond, ptr head, ptr
tail }` (`pthread_mutex_t`/`pthread_cond_t` confirmed fixed-size opaque
buffers on this platform, same precedent as `pthread_t`'s plain-`i64`
treatment). Both the `Sender` and `Receiver` values `channel[T]()`
produces are literally the SAME pointer to this one struct — no
`Arc`-style indirection needed, unlike the interpreter (codegen's
shared `malloc` arena has no analogous need for an owned, `Clone`-able
handle). Queue node: `{ i64 value_word, ptr next }` (16 bytes,
`malloc`'d per `send`, `free`'d by whichever `recv`/`select` poll pops
it), using the same uniform single-word representation every other box
in this backend already uses.

**The central correctness property — a genuinely NEW hazard class
beyond `spawn`/`.join()`**: unlike spawn/join (zero shared mutable
state by design), a channel is a real, concurrently-accessed structure
— multiple senders are a supported, intentional case (matching the
interpreter's own `mpsc::Sender` being `Clone`). EVERY read or write of
`head`/`tail`/a node's `next` pointer happens strictly between the
SAME struct-embedded mutex's lock/unlock, in every one of
`@plum_channel_send`/`recv`/`try_recv` — so any two concurrent callers
strictly serialize: whichever thread's lock returns first performs its
entire append/pop (including `tail`'s read-modify-write) before any
other caller's lock can return. A lost update to `tail` (the classic
multi-producer race this queue shape is vulnerable to if
unsynchronized) is therefore structurally impossible. `send`'s own
node `malloc`+word-store happens BEFORE the lock — safe, since each
caller mallocs its own independent node; nothing shared is touched
until the lock is held. `pthread_cond_wait` is only ever called with
the mutex already held (POSIX guarantees atomic unlock-and-wait, then
re-lock before returning), so it never races a concurrent `send`'s own
critical section either. `send` deep-copies the value being sent
(reusing `deep_copy_capture` verbatim, never `plum_rc_inc` — the same
argument as spawn captures, if anything more load-bearing here since
multiple concurrent senders could otherwise race on the SAME source
cell's non-atomic refcount word with no synchronization at all).
`.recv()` needs NO second deep-copy on the way out — once a node is
off the queue, only the popping call ever touches its payload word
again, the same clean ownership-transfer argument `.join()` already
established, now verified to hold per-node even with multiple
concurrent senders. Verified via actual `-fsanitize=address` AND
`-fsanitize=thread` runs (not just `address` alone, unlike the
spawn/join chunk) on a many-producer test — both passed cleanly.

`.recv()` uses a REAL blocking `pthread_cond_wait` loop, not a
busy-poll — a single channel has a genuine primitive for this, unlike
`select`. `select` uses a busy-poll matching the interpreter's own
algorithm exactly: every arm's receiver expr evaluated once up front,
then a fixed-index-order, non-blocking poll of each arm's queue every
sweep (lock, check `head`, pop-and-done if non-empty else unlock and
continue), `usleep(1000)` (matching the interpreter's 1ms sleep) and
retry from arm 0 if nothing was ready — deliberately no `Disconnected`
case, since these queues never signal disconnection at all (the same
documented gap applies: `select` genuinely spins forever if every arm's
channel is dead). Reuses `Match`'s exact arm-binding pattern
(`"__select_recv"` bound into a fresh per-arm `Env`) and, since
`plum-types` already guarantees every arm's body shares one result
type, `Match`'s exact `phi`+`merge_envs` result-merging scheme
verbatim — no new merging machinery needed.

`check_no_closure_or_task_fields`'s gating was extended from "only if
`spawn` is used" to "if `spawn` OR channels are used" — a channel send
can smuggle a closure/task three levels deep into an opaque `Heap`
pointer exactly as easily as a spawn capture can. Confirmed this does
NOT need extending to also cover nested `Sender`/`Receiver` fields:
those already get `deepcopy_fn_for → None`, and `@plum_deepcopy_heap`'s
existing "`None` means copy the raw word as-is" fallback is EXACTLY
correct for a nested channel handle (copy the shared queue pointer
verbatim) — unlike `Closure`/`Task`, where that same fallback would be
silently wrong, which is precisely why only those two need the
separate whole-program check.

**A real, narrower-than-planned scope limitation, found and handled
safely rather than silently**: `ir::Expr::Channel` carries no type
information at all (`T` is fully erased at the IR layer, by design),
and — confirmed by reading `lower.rs`'s tuple-lowering directly — every
2-tuple in the whole language, generic or not, shares one flat
`"2Tuple"` `tag_fields` entry; there is no existing per-content-type
tuple-tagging mechanism to hook into (unlike generic structs/enums,
which DO get `monomorphize`-based distinct mangled tags). Building real
T-specific tagging for channels would need a new span-keyed `Infer`
side channel plus a matching `lower.rs` special case (mirroring
`EmptyArray`/`empty_array_elem_types`'s own precedent) — real,
cross-crate follow-up work. Rather than risk silently mis-tagging a
second element type (a genuine memory-safety bug, since `.recv()`'s
word-to-value conversion depends entirely on the `Receiver`'s declared
inner `CgType` being correct), **at most one distinct `channel[T]()`
element type is supported per program** — a second, differently-typed
`channel[..]` call anywhere in the same program is a loud, clear
compile-time `Err`, never a silent miscompile.

**FFI: scalar extern calls, CStr, callbacks** (struct-by-value
marshaling deferred). Confirmed directly (not assumed) that FFI is far
simpler in codegen than in the interpreter: `libffi`/`libloading` exist
there solely because a call frame's signature is only known at
Plum-RUNTIME and the target symbol must be resolved dynamically —
neither problem exists in codegen, where an `ExternFn`'s signature is
fully known at codegen time and `clang`/the linker already resolve
real C symbols natively, exactly the way this backend already declares
and calls `malloc`/`pthread_create`/`memcpy`. DESIGN.md's own FFI
section anticipated this ("an LLVM/native backend won't need a
dynamic-signature calling mechanism at all"). `unsafe`-gating and the
"Callback argument must be a bare function name, never a closure
literal" restriction are enforced by the single `plum_types::Infer::
infer_program` call shared by both pipelines — codegen needs zero
duplicate enforcement of either.

`ExternType` → LLVM mapping: `Int`→`i64`, `Float`→`double`, `Bool`→
`i32` (C's `int`, NOT `i1` — this backend's own `CgType::Bool` is
`i1`, a real width mismatch), `Str`/`Callback`→`ptr`, `None` return→
`void`. `declare` emission needs no reactive "only if used" gating the
way spawn/channel runtime helpers do — `program.externs` is already
the complete, explicit list, so one `declare` per entry is emitted
directly, with a defensive reserved-name collision check (a user
declaring `extern "C" { fn malloc(...) }` gets a clear error, not a
broken duplicate `declare`) — with one added wrinkle found during
implementation: LLVM rejects even a BYTE-IDENTICAL duplicate `declare`
line, so a user's own extern block re-declaring a name this backend
already declares for its own runtime (e.g. `strlen`) needs to be
recognized and skipped rather than re-emitted, not just rejected as a
collision.

**`Bool` marshaling, both directions, is the one place a subtly wrong
answer would silently corrupt an ABI-level detail rather than
obviously fail**: an argument going IN gets `zext i1 to i32`; a return
coming OUT uses `icmp ne i32 .., 0` — deliberately NOT `trunc i32 to
i1`, since C's "any nonzero value is true" convention differs from a
naive "read the low bit" truncation, which would silently misread e.g.
a `2` as `false`. Verified both directions not just via inspected IR
text but through a real compile-and-run test executing actual native
code (a tiny custom-compiled C helper, since no real libc function has
a narrow enough Int/Float/Bool-only signature to exercise either
direction).

`CgType::CStr` is a genuinely NEW, distinct representation from
`CgType::Str` — a bare, non-refcounted, NUL-terminated `char*` with no
header at all (not Plum's own length-prefixed, refcounted string
cell). Never refcounted, never deep-copied, and REJECTED from crossing
a spawn/channel boundary entirely (extending the existing `Closure`/
`Task` rejection) — a raw unowned C pointer aliased across two threads
with zero synchronization is strictly worse than either of those,
which at least have defined single-owner or Plum-managed semantics.

`.as_cstr()` codegen validates no embedded NUL (via a declared libc
`@memchr`, matching the `@strlen` precedent of reaching for a real
libc primitive over a hand-rolled loop) then **must** `malloc` a fresh
buffer rather than aliasing a pointer into the existing Str cell's own
NUL-padded byte region (which `@plum_alloc_str` already reserves) —
this is a real, non-obvious SOUNDNESS requirement, not a missed
optimization, discovered by reading `fbip.rs` directly: `.as_cstr()`'s
inner expression is treated as an ORDINARY heap-consuming occurrence,
meaning `.as_cstr()`'s own codegen is the ONLY place that ever
discharges the incoming `Str`'s refcount ownership (no separate `Dec`
is emitted anywhere else for a `Str` wrapped in `.as_cstr()`). Since
this mandatory `@plum_rc_dec_str` call is therefore required, an
aliased pointer into that same cell would dangle the instant the dec
drops the refcount to zero and frees it — the common case whenever the
`Str` was at its last use. A fresh, independently-owned allocation is
the only sound design given `fbip`'s existing ownership contract.

C callbacks reuse `CgType::Closure` — no new `CgType` variant — since
a bare top-level function name passed where a `Callback`-typed extern
parameter is expected already codegens through the EXISTING closures-
chunk `codegen_bare_fn_value` machinery built for ordinary Plum-level
higher-order use. What's genuinely new is a SECOND, env-free trampoline
generator invoked specifically at the extern-call argument-marshaling
site: a real C API has no way to supply the `ptr %env` argument an
ordinary closure trampoline's signature requires, so the callback
trampoline is a structurally simpler, separate function shape with no
env parameter at all, memoized in its own table (kept separate from
the ordinary closure-trampoline table specifically because conflating
the two risks returning the wrong calling-convention shape under what
could otherwise collide as the same lookup key). The extern-call arg
loop special-cases a `Callback`-typed parameter slot, matching the raw
argument expression for a bare function name BEFORE ever calling
`codegen_value` on it (never materializing an unnecessary closure
cell), and references the generated trampoline's function symbol
directly as the call argument — no `ptrtoint`/`inttoptr` round-trip
needed at all, simpler than the closure-cell case, since there's no
intermediate storage step here. Verified via a real compile-and-run
test invoking a genuine C-to-Plum callback round trip through a tiny
custom C helper — proving something the interpreter's own test suite
explicitly could not (no real libc function has a narrow enough
signature to exercise the successful-invocation path, only the
argument-rejection path).

**FFI: struct-by-value marshaling** — closes out FFI entirely. LLVM's
own backend performs real System V AMD64 ABI classification (register
vs. stack, `byval`/`sret`, eightbyte merging/padding) automatically
once codegen emits a correctly-shaped, genuinely-named LLVM aggregate
struct type and uses it as a real by-value parameter/return type —
exactly what `clang` itself does for real C struct-by-value code.
plum-codegen therefore does NOT hand-implement ABI classification the
way the interpreter's `libffi`-based approach needs to (libffi exists
there only because call frames are built at Plum-runtime with no
compiler involved — codegen has a real compiler, `clang`, in the
loop). Verified empirically during planning, not just reasoned about:
a real `clang` compile of a deliberately-padding-inducing struct
(`{int flag; long big;}`) using a genuine named LLVM aggregate type as
a real by-value parameter/return, `extractvalue`/`insertvalue` to
flatten/rebuild it, round-tripped correctly through real native code.
Also verified: named LLVM struct types resolve by name at module
scope — textual definition order doesn't matter, so a nested struct
type can be referenced before its own `type` line appears in the
output.

`plum_types::context::check_ffi_safe` (shared, already running) already
guarantees every FFI-safe struct is non-generic, non-self-referential,
and made entirely of Int/Float/Bool/other-FFI-safe-struct fields —
codegen needed zero additional eligibility re-checking; every
corresponding codegen-side rejection case (CStr/Callback nested in a
struct field, self-reference, generics) is dead-code-by-construction
in a well-typed program, matching this backend's existing precedent
for similar shared-check-already-ran cases.

One non-reactive pass over `program.externs` collects every distinct
`ExternType::Struct` shape (recursing into nested fields, deduped by
name) and emits `%struct.<name> = type { <field llvm types> }` per
entry, reusing the scalar C-ABI width mapping already established for
scalar FFI. Argument marshaling (`build_c_struct_value`) reads each
field out of the Plum Ctor cell via the EXISTING `load_field_word`,
narrows to real C width via the EXACT SAME conversion scalar-FFI
arguments already use (`Bool`: `zext i1 to i32` — genuine reuse, not
reinvention), and `insertvalue`s into a growing aggregate. A nested
struct field's Ctor-cell slot holds a pointer to another heap cell
(Plum structs are always heap-boxed) — recursed into its own aggregate
value, never passed as a pointer at the C boundary. No refcount/
ownership discharge happens here (confirmed against the interpreter's
own struct-argument behavior, which never touches the heap's refcount
either) — the cell's lifecycle stays governed by ordinary FBIP
last-use analysis, same as any other `Heap`-typed extern-call argument.

Return marshaling (`build_ctor_from_c_struct`) is the mirror:
`extractvalue` each field, widen back to Plum's uniform word
representation, and `store_field_word` into a fresh cell allocated via
`@plum_alloc` (reusing the struct's already-interned tag id, the same
lookup ordinary `Ctor` construction already uses). **The one genuinely
nontrivial design call, worked through explicitly rather than
assumed**: a returned `Bool`-mapped struct field gets the SAME
`icmp ne i32 .., 0` "any nonzero is true" normalization as an ordinary
scalar `Bool` return — NOT a bare `zext i32 to i64` — because
`store_field_word`'s `Bool` arm demands a genuinely-normalized `i1`
operand (there is no alternate wider Plum-side `Bool` representation
anywhere in this backend a struct field could legitimately target
instead), and a raw truncation would silently misread a real
nonzero-but-not-1 C `int` (e.g. `2`) as `false` — exactly the bug the
scalar case's own `icmp ne` already exists to avoid, for the identical
underlying reason. Proven with a real, not just theoretical, exposure
test: a C helper deliberately returns `Mixed{flag: 2, big: 777}` (a
genuine padding-inducing shape, `int` then `long long`), and the Plum
program only returns `big` if `flag` reads as true — the test asserts
`"777"`, which is reachable ONLY if both the Bool normalization
correctly reads `2` as true AND `big` was read from the correct,
padding-adjusted byte offset LLVM's own ABI classification computed —
a single test proving both correctness properties LLVM handles for
free, matching the plan's own explicit design intent that IR-text
inspection alone cannot verify padding/alignment (a named LLVM
aggregate type carries no explicit offset text at all — that's
precisely why LLVM's own backend is trusted for it, not this crate).
Also verified against the real system libc `div`/`div_t`, asserting
actual computed quotient/remainder values, not just call success —
going further than the existing interpreter-path test.

**Unicode-aware string operations** (`.runes()`, `.trim()`,
`.replace()`, `.split()` full Unicode correctness; `.to_upper()`/
`.to_lower()` full Unicode SIMPLE case mapping via libc, as of a
2026-08-03 revision — see below). The largest remaining language-
feature gap closed. Confirmed during scoping that `.runes()` needs a
bounded UTF-8 DECODER (byte-pattern classification logic, no table),
`.trim()` needs the Unicode `White_Space` property, a small, fixed
25-codepoint list, and `.split()`/`.replace()` are purely byte-level
substring operations needing no Unicode awareness at all. Case mapping
was initially scoped ASCII-only in the belief that full Unicode case
mapping needed large hand-rolled data tables; a later chunk (2026-08-03)
revisited this by reaching for libc's own `towupper`/`towlower`
instead (see below), the same "declare a real libc function, don't
hand-roll" philosophy already used for `malloc`/`strlen`/`memchr`/
`pthread_*` elsewhere in this backend.

A shared `@plum_utf8_decode`/`@plum_utf8_len_at` runtime primitive
(sequential `icmp`+`br` leading-byte classification, no LLVM `switch`,
matching this backend's established style) decodes/measures one
codepoint at a byte position — assume-valid-by-construction, no
defensive malformed-UTF-8 handling, matching this backend's existing
trust of `@memcpy`/`@strlen` (Plum strings can only ever originate
from valid UTF-8 source text or byte-preserving transforms). `.runes()`
is two-pass (count codepoints, allocate once, decode-and-fill) — the
same two-pass shape used wherever a result's final length isn't
knowable without a scan (`.split()`, and `.replace()`'s pass computing
a grow/shrink-aware final length). `.trim()` uses the standard UTF-8
"scan backwards past continuation bytes to find a character start"
trick for its trailing boundary, checking Unicode whitespace
membership via a shared `@plum_is_unicode_whitespace` helper (25
`icmp` range/equality checks, no loop, no table — cheap and exact,
matching Rust's own `char::is_whitespace` bit-for-bit, the ground
truth the interpreter's own `.trim()` already delegates to).

`.to_upper()`/`.to_lower()` (revised 2026-08-03) call libc's
`towupper`/`towlower` — locale-aware, one-codepoint-in-one-codepoint-
out functions confirmed via real scratch-C-program testing against
this platform's glibc to give genuine Unicode-aware SIMPLE case
mapping across the vast majority of scripts (e.g. `é`(U+00E9) ->
`É`(U+00C9) under `C.utf8`). A one-time `@plum_locale_init()` (called
unconditionally from `plumc`'s generated `@main`, before
`@plum_init_globals()`) sets the process locale to `C.utf8` — glibc
otherwise defaults to the ASCII-only "C" locale, in which
`towupper`/`towlower` silently reproduce the exact ASCII-only behavior
this revision replaces. `C.utf8` (built into glibc since 2.35) is used
over e.g. `en_US.UTF-8` for portability, since it needs no locale to be
installed on the target system. Two new small runtime primitives
support this: `@plum_utf8_encoded_len` (classify an already-known
codepoint's UTF-8 byte length) and `@plum_utf8_encode` (the inverse of
`@plum_utf8_decode` — write a codepoint back out as UTF-8 bytes).
`@plum_str_to_upper`/`@plum_str_to_lower` are two-pass, mirroring
`@plum_str_runes`'s "count then fill, re-decoding/re-mapping in both
passes rather than caching" shape: pass 1 maps each codepoint via
`towupper`/`towlower` and sums the MAPPED codepoints' encoded lengths
(case mapping can change a character's UTF-8 byte length); pass 2
re-maps identically and encodes into the freshly allocated destination.
**The one remaining, precisely-scoped divergence from this section's
"Unicode-aware case conversion" language**: multi-codepoint expansions
(German `ß`→`"SS"`) structurally cannot happen through `towupper`/
`towlower`'s 1-in-1-out C signature, so `ß` stays `ß` — proven by a
dedicated compile-and-run test alongside tests proving real non-ASCII
mapping in both directions (`é`↔`É`) and that plain ASCII still
converts correctly. Because case mapping can now change total byte
length, the `_inplace` reuse variants that existed under the old
fixed-length ASCII scheme are gone — `StrToUpperReuse`/
`StrToLowerReuse` instead follow `StrReplaceReuse`'s own precedent
(see the memory-corruption hazard paragraph below, on `.replace()`'s
own reuse branch): once uniquely owned, call the same fresh-allocating
function and `@free` the old cell directly, rather than mutate in
place.

`.replace()`/`.split()` share a `@plum_str_count_matches` helper
(extending the existing `@plum_str_contains` double-loop precedent to
count ALL non-overlapping matches, not just detect one) for their
respective two-pass length/piece-count computations. Both correctly
handle the empty-separator/empty-`from` edge case via the same
UTF-8-char-boundary logic `.runes()` already builds — confirmed
EMPIRICALLY during design (a real `rustc` scratch-program run, not
assumed) that `str::replace("", to)` uses the exact same char-boundary
insertion semantics as `str::split("")` (inserting/splitting at every
character boundary, N+1 times for an N-character string) — an initial
design-draft guess that this might be byte-boundary instead was wrong
and caught before implementation began.

**A real memory-corruption hazard found and correctly avoided during
implementation**: `StrReplaceReuse`'s reuse branch was originally
planned to `@realloc` and rewrite in place. Hand-tracing a concrete
case (`"aa".replace("a","bbb")`, growing) revealed a naive forward
in-place copy can overwrite source bytes the read cursor hasn't
reached yet — a real correctness bug, not a hypothetical one. A fully
correct in-place version exists (right-to-left copy, per-gap
`@memmove`, since `@memmove` already handles arbitrary overlap
correctly) but was judged real, new, unverified algorithm surface not
worth shipping under this chunk's own scope — instead, the reuse
branch calls the same fresh-allocating path (always safe, including
growth) and frees the old cell directly, correct in every case, just
without a genuine buffer-reuse win for this one specific op. A
dedicated test locks in and documents this deliberate simplification.

**Non-constant Global initializers**. Previously a hard, unconditional
rejection — even the most trivial constant (`let x = 1`) was rejected,
since `ir::Global`'s initializer is an arbitrary `Expr` with no
"constant" tag anywhere upstream and this was entirely a codegen-
imposed restriction. LLVM's own `@g = global <ty> <initializer>` needs
a compile-time constant in the `.ll` text itself, so a placeholder
LLVM global slot (`@global.<name> = global <llvmtype>
zeroinitializer`) is paired with a new `@plum_init_globals()` function
that codegens each initializer — using the exact same `codegen_expr`
machinery any function body already uses — in declaration order and
stores each result into its own slot, called from the hand-written
`main()`'s entry block BEFORE the resolved entry function runs,
preserving the interpreter's own "every global fully materialized
before any user code executes" invariant. This backend doesn't need
`@llvm.global_ctors` (built for independently-ordered constructors
across many separately-compiled translation units) — it already writes
its own single `main()` and has only one Plum program's globals to
initialize in one deterministic order.

`Var(name)` resolution gained one new, purely additive third fallback
tier (`env` → top-level function name → NEW: a known global → error),
serving both an ordinary function referencing an earlier global and
`@plum_init_globals()` itself referencing an earlier global from a
later one's own initializer — both resolve through the identical code
path, making "a later global's reference to an earlier one is always a
load of the already-initialized slot, never a re-evaluation" correct
by construction. Verified directly, not assumed, that free-variable
analysis needs zero changes: a name only becomes a closure-capture
candidate if it's present in the enclosing scope's `env`, and globals
(like bare top-level function names before them) are never inserted
into any `env` — a global referenced inside a closure body is
automatically excluded from the capture set, correctly, since a global
has a fixed whole-program-lifetime address needing no capture/snapshot
at all.

**A real gap found by testing, not merely assumed away**: the
`Var`-resolution tier alone wasn't sufficient for a self-referential
global closure calling itself by bare name (`let fib = |n| .. fib(n-1)
..`) — that call goes through a SEPARATE direct-call fast path
(`codegen_call`), which only checked local scope and known top-level
functions and errored out before ever reaching the tier that consults
known globals. Fixed by having that direct path fall through to the
indirect (closure) path when a bare name isn't a known function but
IS a known global, rather than special-casing anything about
self-reference specifically — `fib`'s own generated body is a separate
top-level `define`, only ever called after `@plum_init_globals()` has
already fully run and stored the closure cell into its slot, so its
own internal self-reference resolves through the ordinary `Var`
tier and finds a fully-materialized value every time. Proven with a
direct global-scope port of the existing self-referential-closure
test (`fib(10) == 55`).

Mutual recursion between globals is structurally impossible to
construct in a well-typed program (globals aren't pre-declared as a
batch the way functions are); forward references are already rejected
upstream by `plum-types`; neither needs a codegen-side check.

**`Global` initializers calling a still-generic function.** Closes the
gap noted above — required a genuine, two-layer fix, not just a
codegen change, because the real root cause turned out to be a
type-soundness bug, not a monomorphization/rewrite gap.

The shallow half was as expected: `monomorphize::plan`'s worklist
already discovered and monomorphized any generic function a global's
initializer called (its seeding loop walks every `resolved_sites`
entry unconditionally, regardless of which item — function or global —
a site belongs to), but `MonoPlan` never rewrote a GLOBAL's own body to
reference the mangled instantiation, and `plumc::codegen_cli` pulled
`ir_program.globals` straight from an unrewritten `lower_program` pass.
Fixed by giving `MonoPlan` a `globals: Vec<ir::Global>` field, folding
globals into the SAME worklist mechanism functions/structs/enums
already use (a new `Task::Global(String)` variant — needed because a
call site inside a global's own rewrite pass re-requests an
instantiation via `resolve_site`'s `new_tasks` EVERY time it matches,
regardless of whether that instantiation was already fully processed
elsewhere; the existing `done_fns` dedup at the top of each task's own
processing makes re-requesting it a harmless no-op, but only if it goes
through the real worklist, not a hand-rolled "no new tasks expected"
assumption — which does NOT hold and was caught by a failing test, not
guessed), then reassembling `globals` in original source declaration
order (not worklist/discovery order) in one final pass once the
worklist drains, since `@plum_init_globals` depends on that order.

The deep half, found while grounding the shallow fix against the
actual type-checker (not assumed): `plum_types::Infer::infer_program`
inferred every global's REAL initializer in a "Phase 1.5" that ran
BEFORE any function body was checked. A generic function's parameter
and return types are only linked together — and its signature only
generalized into a real, callable-at-many-types `Scheme` — once ITS
OWN body has been checked ("Phase 2"). So a global calling a generic
function was structurally stuck unifying against that function's raw,
un-linked Phase-1 placeholder, which could PERMANENTLY pin the
function's type variable to whatever the global happened to use it at
— breaking every OTHER call to the same generic function elsewhere in
the program at a different type. Verified empirically before touching
any code: `identity[T](x:T):T=x; let g=identity(5); let h()=identity(true)`
failed with `type mismatch: expected Int, found Bool` on this exact
codebase before this fix — a genuine, pre-existing soundness gap, not
a narrow codegen limitation, and unrelated to the shallow half above.
Surfaced to the user via `AskUserQuestion` (fix the real ordering bug /
narrow-scope-and-reject the unsound case / park it) rather than
shipping a fix that only works when the generic function has exactly
one caller in the whole program — the user chose to fix the real bug.

Fixed by splitting global inference into three phases: (1) a NEW early
pass, positioned where "Phase 1.5" used to run (before functions),
infers every global's REAL initializer using a throwaway, DISCARDED
substitution — giving Phase 2 (function bodies) each global's real,
concrete early-computed type for ordinary visibility (including struct
FIELD ACCESS, which needs a resolved `Type::Struct` immediately and
can't defer the way plain unification can — confirmed by trying a
placeholder-based design FIRST, which broke `struct Box{val:Int}\nlet
g=Box{val:1}\nlet go()=g.val`, a case with no generics involved at
all), without letting any premature generic-call unification leak into
the REAL substitution Phase 2 depends on for generalization (since it
never composes into it); (2) Phase 2 (functions) runs exactly as
before, now merging each global's early type into `body_env` FRESH on
every iteration (not a one-time snapshot — an early design mistake
that silently broke ordinary function-to-function calls between two
Phase-2 siblings with NO globals involved at all, caught by a failing
test) and re-applying the LIVE substitution on top of each stored early
type (not just cloning it — a second, related mistake: a global that
merely ALIASES a function's still-unresolved signature, `let f =
square`, needs that alias to track `square`'s type as Phase 2 actually
resolves it, exactly the same staleness bug an existing regression
test, `a_global_aliasing_a_function_declared_earlier_resolves_calls_
through_it_fully`, was already written to guard against); (3) the REAL
"Phase 3" (moved to run AFTER Phase 2, the exact position "Phase 1.5"
used to occupy) re-infers every global's body a SECOND time, now
against the real, fully-generalized environment, becoming the
authoritative source for `global_types`/`resolved_sites`/
`final_subst`. The early pass's own failures are caught and treated as
non-fatal per global (not propagated with `?`) — a global whose shape
depends, even transitively, on a generic function's not-yet-linked
Phase-1 placeholder can genuinely fail to resolve early even in a
perfectly well-typed program (a generic function's param/return aren't
linked until Phase 2 checks its own body), so an early failure just
means that global (and anything referencing it BY NAME during the same
early pass) gets no early visibility — Phase 3 remains authoritative
regardless.

This reorder has one genuine, narrow, remaining structural limit,
found and documented rather than hidden: a FUNCTION cannot do field
access (or any other structural, immediately-concrete-requiring
operation) on a global whose value came from calling a still-generic
function, because that global's real shape isn't knowable until Phase
2 (or Phase 3) has actually run — the early pass can at best learn a
generic call's ARGUMENT type, never its un-linked RETURN type. A LATER
GLOBAL doing the same field access works fine (Phase 3 threads
`global_env` sequentially in file order with fully-resolved types), but
a function referencing even that later global then hits the same wall
transitively. Locked in by dedicated tests on both sides of the
boundary, in both `plum-types` and a real compiled-and-run `plumc`
test proving the actual soundness fix end-to-end (the same generic
function instantiated from a global at one type AND from a function at
a different type in the same compiled program).

**Closures inside generic functions**. Lifts a pre-check that
previously rejected any closure literal inside a still-generic
function's own body outright. Precisely traced (not guessed) what was
actually missing before touching anything: `plum-codegen`'s existing
closure machinery already handled per-instantiation differences
correctly with zero changes needed — a capture's `CgType` is
determined purely from the enclosing function's own already-concrete
`env` by the time codegen ever sees an `ir::Function` (codegen only
runs on monomorphization's output), and the per-closure-literal-site
naming counter is a single, never-reset, whole-compile `RefCell`, so
two instantiations' closures always land under provably distinct
generated names for free. Confirmed by `git diff`: this chunk touched
zero lines in `plum-codegen` itself.

The genuine gap was entirely upstream, in two precisely-traced places:
`plum_types::Infer::resolve_closure_types` had no template-resolution
fallback the way `resolve_generic_sites` already did — a closure
inside a generic function's body has its type pinned only to the
enclosing function's own still-unresolved type variable, the exact
same "tier 2 template" situation `resolve_generic_sites` already
solves for ordinary generic construction/call sites, just never
extended to closure literals; and `monomorphize.rs`'s `rewrite_expr`
recursed into a closure's body but did nothing to resolve/specialize
the closure's OWN recorded type per instantiation, needing the same
per-instantiation substitution treatment `field_owner_overrides`/
`extra_variants`/`extra_struct_fields` already get. Both fixed by
mirroring the existing pattern exactly rather than inventing a new
mechanism.

One additional, unanticipated fix was needed once the pre-check was
removed: `plumc`'s pipeline eagerly lowers the ORIGINAL, un-instantiated
AST once (solely to pick up globals/externs) before `monomorphize::plan`
ever runs — with the pre-check gone, this eager pass now hits a
closure literal whose recorded type is still a `Type::Param` template,
which `lower.rs`'s existing type-conversion helper correctly rejects
as unrepresentable. Fixed by having that one lowering arm treat a
template-containing type the same as "no info available," which is
exactly correct here since this particular lowering pass's own
function output is always discarded and fully replaced by
`monomorphize::plan`'s per-instantiation output — a real, necessary
gap-fill exposed by removing the pre-check, not a sign the original
design was wrong.

**Deliverable**: `plumc::compile_and_run(src, entry_fn, args) ->
Result<String, String>` runs parse → prelude → type-check → movecheck
→ lower → optimize (the same pipeline `run_resolved_program` uses,
diverging exactly where DESIGN.md's own sequencing note said it
would), emits IR via `plum_codegen::emit_program`, appends a hand-
written LLVM `main` that calls the entry function and `printf`s its
result, writes the `.ll` to a temp file, shells to `clang` to compile
and link, runs the resulting native binary, and returns its captured
stdout — proven via tests that actually compile and execute real
binaries (including a real recursive enum linked list, tail-recursively
summed), not just inspect emitted IR text.
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

- `Ref[T]`'s naming, construction (`ref(v)`), `.get()`/`.set()`,
  representation (`Rc<RefCell<Value>>`, outside the toy heap/FBIP
  entirely), pattern-matching interaction (none — not directly
  matchable), and concurrency-boundary behavior (a reported error, not
  a silent deep-copy) are all now Decided (see "Mutability and cycles"
  above). Still open: cycle collection strategy (deliberately deferred,
  see "Cycle collection" above — a `Ref` that reference-cycles with
  itself/another `Ref` currently just leaks, by design, until that's
  revisited), and whether `Ref` should ever get real cross-thread
  sharing (would need `Arc`/`Mutex`, a genuinely different
  representation — not a small extension of the current one).
- `Array[T]`'s v1 scope, `for x in arr` iteration, the `.pop()`/
  `.set()`/`.remove()`/`.map()`/`.filter()`/`.fold()` operations,
  builtin-type parameter/return annotations, and `.push()`/`.pop()`/
  `.set()`/`.remove()`'s reuse-in-place optimization are all now
  Decided (see "Arrays" above). Strings becoming heap-backed/refcounted,
  `.len()`, `.concat()`, heap-aware `==`, byte-indexing `s[i]`,
  `.runes()`, `.trim()`, `.split(sep)`, `.to_upper()`, `.to_lower()`,
  `.contains()`, `.starts_with()`, `.ends_with()`, `.replace()`, and
  `.to_string()` (scoped to Int/Float/Bool/Str) are likewise now
  Decided (see "Strings" above). Still open, for strings specifically:
  `.to_string()` for structs/enums/arrays/tuples (needs real design —
  the IR carries no field names to render with), other standard string
  operations (e.g. `repeat`), and grapheme-cluster-aware operations (a
  "rune" is a Unicode SCALAR VALUE / codepoint, not a user-perceived
  character — a grapheme cluster like an emoji with modifiers can span
  multiple runes; `.runes()` doesn't attempt that level). String (and
  Int/Float/Bool) literal PATTERN matching in `match` is now Decided
  too (see "Pattern grammar" above) — closing what used to be listed
  here as a pre-existing gap.
- A trailing catch-all mixed into an otherwise Ctor-tag-shaped `match`
  is now Decided too (see "Pattern grammar" above — a sentinel
  `MatchArm.tag`, no IR shape change needed), and so is `Pattern::Or`
  for tag-shaped alternatives (`A(v) | B(v) => ..`) and enum match
  EXHAUSTIVENESS checking. Still open/unimplemented in `match`,
  tracked here so none of it gets lost:
  - A catch-all in any NON-last position mixed among Ctor-tag arms
    (only the trailing position gets the `DEFAULT_ARM_TAG` sentinel
    treatment today).
  - Or-patterns over LITERAL alternatives (`1 | 2 => ..`) — today's
    `Pattern::Or` support only covers tag-shaped (Variant/Tuple/Struct)
    alternatives.
  - Or-patterns whose alternatives contain a NESTED Variant/Tuple/
    Struct sub-pattern (`A((x, y)) | B((x, y)) => ..`) — rejected at
    both `infer_or_pattern` and `lower_match` today; lowering's
    synthetic-placeholder destructuring mechanism doesn't currently
    compose across multiple arms sharing one body.
  - Genuinely still UNDECIDED, not just deferred: whether a GUARDED arm
    should keep counting as covering its variant for exhaustiveness
    purposes, or whether that should tighten to match Rust's own
    stricter rule (see "Pattern grammar" above for the full tradeoff;
    explicitly flagged as a revisit candidate when the permissive
    version was chosen — Brad said he isn't sure he prefers it — not a
    closed question).
- Self-referential closures are now Decided (see "Tuples and closures"
  above — SELF-recursion only). Mutual recursion between two separately
  -declared closure-valued globals (as opposed to two named top-level
  FUNCTIONS, which already support mutual recursion) remains genuinely
  unsupported — a real, separate gap, not just an oversight.
- Standard library scope — started (see "Standard library" above:
  basic output/`println` is the first piece). Still wide open: what
  comes next (collections beyond the built-ins, file I/O, JSON, HTTP,
  ...), and whether/when `println` migrates from the prelude into a
  real `use`-based `io` module once there's enough stdlib surface to
  justify extending the `compile_and_run` test harness to drive a real
  temp project through `resolve_project`.
- Whether/when to build the scoped incremental cycle collector for
  `Shared` values (see Memory model above — deliberately deferred until
  real Plum code shows the pain is real).
- Whether Plum curries function application by default (ML-family
  norm), deferred because of its interaction with the calling
  convention and FBIP (see Surface syntax above; note the `|>` pipe
  desugaring does NOT depend on this being resolved).
