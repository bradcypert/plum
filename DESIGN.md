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
`.ends_with()`/`.replace()` — Decided (2026-07-28).** Rounded out
strings further the same evening, all delegating directly to Rust's
own `str` methods of the same name (Unicode-aware case conversion via
`to_uppercase()`/`to_lowercase()`). `.to_upper()`/`.to_lower()`/
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
  EXHAUSTIVENESS checking. Still open: a catch-all in any NON-last
  position mixed among Ctor-tag arms, or-patterns over LITERAL
  alternatives (`1 | 2 => ..`), and — genuinely still undecided, not
  just deferred — whether a GUARDED arm should keep counting as
  covering its variant for exhaustiveness purposes, or whether that
  should tighten to match Rust's own stricter rule (see "Pattern
  grammar" above for the full tradeoff; explicitly flagged as a
  revisit candidate when the permissive version was chosen, not a
  closed question).
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
