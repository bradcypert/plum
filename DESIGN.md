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

### `spawn` + `extern` calls — a real bug, found and fixed (2026-08-12)

A task thread's own `Interpreter` (`Expr::Spawn`'s handling) started
with a completely empty `extern_fns` table — only `Interpreter::load_
program`, run once on the MAIN thread, ever populated it. Any `extern`-
backed call inside a `spawn { .. }` block — every `tcp_*`/`dir_*`/
`process_*` stdlib function, all of it — failed with "unknown extern
function" even though the identical call worked fine outside a
`spawn`. Found incidentally while doing unrelated work, filed, and
fixed here: `Interpreter` now also keeps the raw `ExternFn`
declarations it resolved (`extern_defs`, plain `String`/`Vec`/enum
data, genuinely `Send` — unlike `ExternFnHandle` itself, which wraps a
foreign `CodePtr` `libffi` gives no `Send` impl for), and `Expr::Spawn`
hands a COPY of those declarations across the thread boundary so the
new task's own `Interpreter` can re-resolve them itself (a second
`dlsym`+`Cif::new` per extern, paid once at spawn time). Native codegen
never had this bug — an `ExternCall` there compiles to a plain,
statically-linked LLVM `call`, valid identically in every thread, no
per-thread resolution table involved at all.

While fixing this, a SECOND real bug surfaced: a spawned task's own
thread used Rust's small (~2 MiB) `thread::spawn` default stack, not
the much larger stack size (256 MiB) this interpreter's OTHER call
sites already have to hand-configure around plain (non-tail) recursion
in stdlib code (deep `String` scanning, HTTP parsing, ...) — see
`Interpreter::eval`'s own doc comment on why it has no TCO. A spawned
task's body is exactly as likely to hit that recursion as top-level
code is; confirmed by a real stack overflow (not hypothetically) the
first time a real recursive HTTP parse ran inside a spawned task.
Fixed by giving every spawned task's thread the same 256 MiB stack.

### HTTP server concurrency — attempted, reverted (2026-08-12)

With the `spawn`+`extern` bug above fixed, `http_serve_loop` was
rewritten to `spawn` a task per accepted connection instead of running
`http_handle_connection` inline before accepting the next one — the
natural next step once `spawn` could actually make the `tcp_*` calls
`http_handle_connection` needs. Verified working, for real, in the
INTERPRETER: a dedicated test opened two connections, left the first
one's request deliberately incomplete (so the server's read for it
stays genuinely blocked), and confirmed the second connection still
gets accepted and served promptly rather than waiting behind the first.

It broke NATIVE codegen, though — not just for programs that actually
use `http_serve_loop`, but for every native-codegen program that pulls
in the HTTP module AT ALL (confirmed directly: even the plain HTTP
*client* test, which never calls `http_serve_loop`, started failing
to compile), because this backend codegens every concrete top-level
function in the loaded module unconditionally, not just ones reachable
from the entry point. Root cause: `plum-codegen`'s `spawn` capture
check (`crosses_thread_boundary`) rejects EVERY closure-typed free
variable by TYPE alone, with no per-VALUE distinction between a real
closure (genuine captured heap state, can't safely cross a thread) and
a bare reference to a top-level function (`codegen_bare_fn_value`
already generates this as a genuinely zero-capture closure cell —
`plum_alloc_closure(i64 0)`, a no-op release function — which is just
as safe to send as the interpreter's own `Value::Function` already is,
see the section above). The interpreter has exactly this per-value
distinction already (`Value::Function` vs `Value::Closure(id)`); native
codegen's closure representation currently erases it once a value is
stored in `env` — by the time `spawn`'s capture check runs, there's no
way to tell "this Closure-typed register holds a bare top-level
function reference" from "this Closure-typed register holds a real
closure with live captures" without a runtime tag on the closure cell
itself (its release-function pointer, stored at a known offset, could
serve as exactly that tag — equal to `@plum_closure_release_noop`
`iff` zero-capture — but wiring that into `spawn`'s codegen changes a
compile-time rejection into a value that can only be checked at
runtime, a bigger, cross-cutting change not attempted here).

**Decision: reverted, for the moment.** `http_serve_loop` went back to
sequential — real, useful, precedented (Apache prefork/early Java/Ruby
WEBrick all shipped this shape for years) — while the real fix (below)
was scoped and built.

### Native-codegen zero-capture closure fix — done (2026-08-12), same session

Scoped and built the fix flagged above, then re-shipped `http_serve_
loop`'s spawn-per-connection concurrency on top of it. Three changes,
all in `plum-codegen`, nothing elsewhere:

1. **`codegen_spawn_literal`**: a closure-typed captured free variable
   is no longer a blanket compile-time `Err`. Instead, it emits a
   RUNTIME check — load the cell's release-function pointer (a fixed
   offset, already used identically by `codegen_bare_fn_value`/`codegen_
   closure_literal`'s own zero-capture case) and compare it against
   `@plum_closure_release_noop`, using the exact same `emit_runtime_
   check`/`@plum_abort` idiom this backend already uses for e.g. array-
   bounds checks. A real closure with live captures still aborts
   cleanly; a zero-capture closure or bare top-level function reference
   now passes through.
2. **`deep_copy_capture`**'s `Closure` arm went from a "guaranteed
   unreachable" stub to a real implementation: build a FRESH zero-
   capture cell (`plum_alloc_closure(i64 0)`, code pointer copied from
   the original cell at its own known offset, release fn set to the
   noop) rather than passing the original pointer through unchanged —
   even a zero-capture cell has its own refcount header, and sharing
   that raw pointer across threads would let both threads race on it,
   exactly the class of bug deep-copying every other heap type already
   exists to prevent.
3. `crosses_thread_boundary` itself, and `codegen_channel_send`'s use
   of it, were left UNTOUCHED — this only changes `spawn`'s own capture
   loop. Matches an asymmetry the interpreter already has (it lets a
   bare top-level function cross `spawn` for free via `functions.
   clone()`, but `channel.send()` still rejects one via `to_portable`)
   rather than inventing new cross-backend behavior. Array-typed
   captures containing closures also stay hard-rejected, unchanged —
   narrowly scoped to the exact case that was blocking, not a general
   capability expansion.

Verified with three new native-codegen tests: a zero-capture closure
literal crossing `spawn` successfully, a closure that genuinely closed
over a local variable aborting cleanly at runtime (not a compile-time
reject — confirmed by updating the OLD test that asserted the old
compile-time-reject behavior, since a zero-capture closure it had been
using as its example — `|x: Int| x + 1`, no free variables — is now
legitimately ALLOWED, not a bug), and a bare top-level function passed
by name crossing `spawn` (the exact `handler`-parameter shape `http_
serve_loop` needs). Then re-applied the `http_serve_loop` spawn-per-
connection rewrite, this time verified working in BOTH backends — a new
native-codegen concurrency test mirrors the interpreter one: client A
connects and sends a deliberately incomplete request (leaving its
task genuinely blocked), client B connects with a complete, ordinary
request and gets served promptly regardless.

**A real, separate bug found while writing that native test**: since
`http_serve`/`http_serve_loop` never returns, a `compile_and_run`-based
test's underlying compiled BINARY runs forever — and a `Command`-
spawned child process that outlives its Rust parent does NOT get killed
when the parent exits (it's reparented, not reaped), unlike a merely-
leaked Rust THREAD (which dies with its process). An early version of
this test leaked a real orphaned server process holding its port across
LATER, unrelated test runs, causing spurious failures against a STALE
process's stale binary. Fixed by spawning the compiled binary directly
(bypassing `compile_and_run`) and wrapping the `Child` in a `Drop`-based
kill guard, so it's reaped even if an assertion panics partway through.

**`http_serve_loop` is spawn-per-connection concurrent in BOTH backends
now** — the interpreter version from the attempt above, unchanged; the
native-codegen path newly unblocked by this fix.

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

**Originally deliberately deferred, since revisited and built**: full
ML-style currying (`add(5)` partially applying to return a function).
This paragraph originally read "deliberately deferred, not adopted,"
citing real implementation weight — a fully-applied direct-call fast
path plus a closure-allocating path for partial application, touching
the calling convention and interacting with `fbip`. That concern turned
out to be smaller than expected once actually scoped: see this file's
own "Currying" section (way below, dated 2026-08-12) for what shipped
and why the feared cost mostly evaporated (the closure-allocating path
already existed, for an unrelated reason).

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

**Amendment — `_` placeholder for non-last insertion position**: almost
every stdlib associated function (`Array.map(arr, f)`, `Array.take(arr,
n)`, `Array.sort_by(arr, cmp)`, ...) takes its "subject" value FIRST,
not last — the opposite position "insert as last argument" targets. So
`x |> Array.map(f)` (meaning `Array.map(f, x)`) doesn't work at all; it's
a type error, not just unidiomatic. Discovered while trying to pipe two
chained `Array.map` calls in `examples/adts_and_matching/main.plum`.
Fixed by letting a literal `_` argument in `rhs`'s call mark WHERE `x`
goes instead of always appending: `x |> f(a, _, b)` means `f(a, x, b)`;
no `_` anywhere falls back to the original append-last rule, unchanged.
More than one `_` is a compile error (ambiguous — which one?), not a
silent pick.

No grammar change needed for this — `_` as a bare call argument (no
postfix chain following it) was ALREADY parsed as sugar for the trivial
identity closure `|_| _` (see GRAMMAR.md's `PlaceholderChain`), for an
unrelated reason (`xs.map(_)` meaning `xs.map(|x| x)` back when `.map`
still had dot-call sugar). Pipe desugaring just gives that same,
already-produced AST shape a SECOND meaning, specifically and only
inside a pipe's own RHS argument list: nobody writes a literal identity
closure as an explicit call argument on purpose, so repurposing it there
is unambiguous in practice, and it costs zero parser/grammar changes —
purely a `plum-types::infer::infer_pipe` / `plum-ir::lower::lower_pipe`
desugaring-time change (see `splice_pipe_args`/`is_pipe_placeholder` in
both).

**A related bug this surfaced and fixed**: `infer_pipe`/`lower_pipe`
used to build the desugared call DIRECTLY (calling `infer_call`/
emitting `ir::Expr::Call` by hand) instead of going back through the
ordinary `ast::Expr::Call` dispatch path — meaning piping into `Array.
map`/`Array.filter`/`Array.fold` specifically (recognized purely by AST
SHAPE inside that ordinary dispatch, not through the general name/
scheme-lookup machinery every other call uses) produced a spurious
"unbound variable: Array", independent of and in addition to the
argument-order problem above. Fixed the same way: both functions now
rebuild a genuine `ast::Expr::Call` and recurse through `infer_expr`/
`lower_expr`, so pipe desugaring is indistinguishable from writing the
same call out by hand.

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

## Standard library — v1 started (basic output, `Map`/`Set` collections)

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

**`println`/`print` need no new compiler/backend builtin at all** —
they're ordinary Plum source (final form — see "Chunk 3" below for how
this evolved from an initial `puts`-based `println` once `print` was
added and a real cross-function bug surfaced):
```
extern "C" {
    fn write(fd: Int, buf: CStr, count: Int) -> Int;
}

let print[T] (x: T): Unit = unsafe {
    let s = x.to_string();
    let n = s.len();
    write(1, s.as_cstr(), n);
    ()
}

let println[T] (x: T): Unit = unsafe {
    let s = x.to_string().concat("\n");
    let n = s.len();
    write(1, s.as_cstr(), n);
    ()
}
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

Discarding `write`'s non-`Unit` (`Int`) return value inside a block
ending in `()` was ALSO verified empirically rather than assumed —
confirmed working in both backends before committing to the design.

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

### Chunk 2: `Map[K,V]`/`Set[T]` collections

`Array[T]` already covers growable-list semantics as a core-language
builtin, so the obviously-missing next piece was a key-based `Map`/
`Set`. A research pass found a real, blocking prerequisite FIRST:
structural equality (`==`/`!=`) was broken for non-scalar types in
both backends, and `Str` equality was missing ENTIRELY from the native/
LLVM backend — `codegen_binop` (`plum-codegen/src/codegen.rs`) only had
arms for `Int`/`Bool`/`Float`; `"a" == "b"` didn't even compile
natively. Presented the scope tradeoff to Brad (fix Str equality + keys
scoped to `Int`/`Float`/`Bool`/`Str` / also build full structural
equality now / narrower Int+Str-only slice) rather than picking
silently — he chose the first: fix Str equality as a real prerequisite,
scope `Map`/`Set` keys to the four types with working equality, leave
struct/array/tuple keys as an explicit, documented, separate future
gap (the `Eq` bound in `plum-types` doesn't actually enforce this at
the type-checker level today — a pre-existing checker/backend mismatch,
not tightened as part of this chunk).

**Str equality fix**: a new `@plum_str_eq(ptr, ptr) -> i1` runtime
primitive (fast length-reject via the existing string-cell-layout
offsets `@plum_str_contains` already establishes, then `@memcmp` the
byte range — `declare i32 @memcmp(ptr, ptr, i64)` added alongside the
existing libc declares) plus a `Str`-specific branch in `codegen_binop`
for `Eq`/`Ne`, handled before the generic instruction-shaped match
since a runtime call doesn't fit that match's single-binary-op tuple
shape. The interpreter needed no change — `values_equal` already
compared `Str` content correctly.

**`Map[K,V]`/`Set[T]` as recursive generic enums (association
lists)**, NOT `Array[Tuple[K,V]]`: confirmed `plum_type_to_cg_type` has
no `Type::Tuple` arm at all — every tuple lowers to one flat, non-
type-specialized synthetic tag, unsafe/unrepresentable once reached
through a type signature (e.g. an array element) rather than always
fully destructured locally. The safe, already-proven foundation is the
`List[T] { Cons(T, List[T]), Nil }` shape already exercised by real
compiled-and-run tests at two concrete types — `Map`/`Set` are the same
pattern with more payload fields per node:
```
enum Map[K, V] { MapNode(K, V, Map[K, V]), MapEnd }
let map_new[K, V] (): Map[K, V] = MapEnd
let map_insert[K: Eq, V] (m: Map[K, V]) (k: K) (v: V): Map[K, V] = MapNode(k, v, m)
let map_get[K: Eq, V] (m: Map[K, V]) (k: K): Option[V] = match m {
    MapNode(k2, v, rest) => if k == k2 { Some(v) } else { map_get(rest, k) },
    MapEnd => None,
}
-- map_contains/map_remove/map_len follow the same shape; Set[T] mirrors
-- it with SetNode(T, Set[T])/SetEnd, deduping on insert via set_contains.
```
Deliberately simple v1 semantics, documented rather than accidental:
`map_insert` always PREPENDS (O(1), no scan-to-replace) — `map_get`/
`map_contains` scan from the head, so the most recently inserted entry
for a key naturally wins; `map_remove` removes only the first (most
recent) matching node, uncovering an older value if a key was inserted
twice; `map_len` counts nodes, not unique keys. `set_insert`, unlike
`map_insert`, DOES dedupe (calls `set_contains` first) since a set has
no duplicates by definition. Linear (`O(n)`) everything — no hashing;
a hash-table-backed version is a natural, separate future chunk if/when
real Plum code shows the pain (mirroring this project's own stated
policy for the GC/cycle-collector question). No `println`/`.to_string()`
support for `Map`/`Set` values themselves (still `Int`/`Float`/`Bool`/
`Str` only) — printing one directly is out of scope; a program prints
values it EXTRACTS from a `Map`/`Set` instead (proven this way in
manual end-to-end testing).

Merged into `with_prelude` as a new `STDLIB_COLLECTIONS_SRC` constant,
alongside `PRELUDE_SRC`/`STDLIB_IO_SRC` — same "no `use` needed yet, a
real `use`-based module is a later chunk" reasoning as `println`.

**Two real, unplanned monomorphization bugs found and fixed while
implementing this** (not silently absorbed — both are narrow, targeted
fixes restoring clearly-intended-but-gapped behavior, not new
semantics):
1. `resolve_site`'s `SiteKind::Enum` branch (`plum-ir/src/
   monomorphize.rs`) never pushed a follow-up `Task::Enum` — only
   `SiteKind::Function`/`Struct` did. This is invisible for an ordinary
   generic function, since some OTHER site (a struct field, a sibling
   call) usually also discovers the same enum — but `let map_new[K, V]
   (): Map[K, V] = MapEnd` has NO other struct/enum-typed site anywhere
   in its body; its entire reachable enum usage is one bare `Ctor`
   call. Without the fix, that instantiation's tag never got registered
   and codegen failed with "unknown tag" for a tag that was real but
   simply never enqueued. Fixed by tracking variant→owning-enum name
   and pushing `Task::Enum` from this branch too, mirroring what the
   top-level seeding loop already does for a non-generic caller.
2. `validate_field_type` (generic struct/enum field-type validation)
   had no arm for `Type::Str`, even though `Str` is a fully ordinary,
   always-supported `CgType` everywhere else — a stale mismatch against
   its own non-generic counterpart, `plum_type_to_cg_type`, which has
   always mapped `Str` fine. Presumably no earlier generic-struct/enum
   test happened to use a `Str` field. Blocked any `Str`-keyed/valued
   `Map`/`Set` until fixed (needed one pre-existing test —
   `instantiating_a_generic_type_at_an_unsupported_concrete_type_is_a_
   clear_error` — to swap its example from `Str` to a genuinely still-
   unsupported closure field, since `Str` is no longer an example of
   the thing that test is proving).

Tests: 3 `plum-codegen` IR-shape unit tests (`@plum_str_eq`/`@memcmp`
present; `==` on `Str` actually calls `@plum_str_eq`); 10 native
compile-and-run tests (`Str` equality both directions; `Map` most-
recent-wins and remove-uncovers-older semantics precisely, not just
"doesn't crash"; `Set` dedupe; `Map[Int,Str]` and `Map[Str,Int]` both
instantiated in the same compiled program); 3 interpreter-path tests.
Workspace now 1385 tests (up from 1369 — net +16), clean build, zero
warnings. Verified independently — forced a rebuild before trusting
diagnostics (per this project's established stale-diagnostics
pattern), re-ran the full suite, read both monomorphization fixes and
the `Str`-equality codegen diff directly rather than trusting the
implementing agent's summary alone, and built+ran a real throwaway
Plum project through BOTH the native `build` and interpreter CLI paths
by hand — output identical and correct through both (`map_get`'s
most-recent-wins behavior, `map_len`'s node-count semantics, and
`set_len`'s dedupe were all visually confirmed, not just asserted in a
test).

### Chunk 3: `print` (no trailing newline) — and a redesign of `println` along with it

Closes the one deliberately-deferred piece from chunk 1. `println`
used libc's `puts`, which conveniently always appends a newline on its
own — `print` needs the same underlying write WITHOUT that newline.
`fputs(s, stdout)` and variadic `printf("%s", s)` were both considered
and rejected as genuine dead ends for this codebase specifically
(confirmed by reading the actual extern-type machinery, not assumed):
this codebase's extern type system is a CLOSED list (`Int`/`Float`/
`Bool`/`CStr`/callback/struct-of-those) with no raw-pointer/opaque type
and no extern-GLOBAL-variable grammar at all, so `FILE* stdout` isn't
expressible or even referenceable; C-variadic call support doesn't
exist for USER-declared externs in either backend (the LLVM backend
emits a couple of HARDCODED internal variadic calls of its own, e.g.
for `Float.to_string()`, but nothing threads that through to Plum-
source-declared externs, and `plum-interp`'s `libffi`-based extern-call
path has no variadic-CIF support at all) — both would need genuine new
type-system/backend work.

The mechanism that IS expressible today with zero new type-system
work: the raw POSIX `write(2)` syscall — `write(fd: Int, buf: CStr,
count: Int) -> Int`, every type already in the existing closed list.
The byte count comes from `.len()` on the `Str` before converting to
`CStr` (a core-language builtin) rather than a separate `strlen`
extern call — deliberately: an earlier draft used `strlen`, but more
than one EXISTING test in this codebase already declares its own
extern `strlen` for unrelated reasons, and prelude-merged source
shares the SAME flat top-level namespace as ordinary declarations —
real "already declared" collisions resulted immediately. `.len()`
sidesteps the collision risk entirely and avoids an extra FFI round
trip; `write` itself was checked against every existing extern
declaration first, no collision.

**Two real, distinct bugs found by testing, neither caught by the type
checker (both are correctness/ordering bugs, not type errors) — and a
genuine redesign that resulted, not a patch:**
1. **A real use-after-free.** An early draft called `write(1,
   s.as_cstr(), s.len())` directly. `.as_cstr()` CONSUMES its `Str`
   (its own lowering calls `@plum_rc_dec_str` on the original cell —
   confirmed by reading the actual generated LLVM IR, not assumed), and
   call arguments evaluate left to right, so `s.len()` — evaluated
   THIRD, after `.as_cstr()` already ran — read `s`'s length field
   after it may already have been freed. Silent, not a crash: `write`
   simply wrote zero (or garbage) bytes, which is why the very first
   real compile-and-run test caught it immediately (`print`'s output
   was simply MISSING from captured stdout, not obviously "wrong").
   Fixed by binding the length to a local (`let n = s.len()`) BEFORE
   calling `.as_cstr()`, so the read happens while `s` is still
   guaranteed alive.
2. **A real cross-function output-ordering bug**, found only once
   `print` and `println` were tested TOGETHER in the same program
   (`print("a"); println("b"); print("c")` produced `"acb\n"` instead
   of `"ab\nc"`): `println` (still `puts`-based at this point) goes
   through C's block-buffered stdio, while `print`'s `write` is
   unbuffered and reaches the OS immediately — an EARLIER `puts` call's
   still-buffered output could be overtaken by a LATER `write` call's
   immediate one. The exact same class of problem chunk 1's own
   `fflush` fix already solved ONCE (see below), but that fix only
   covers the interpreter CLI's own single final print — it does
   nothing for two DIFFERENT buffering strategies fighting each other
   WITHIN one running Plum program. Fixed properly, not patched around:
   put `println` on the exact SAME mechanism as `print` (`write`)
   instead of leaving two different I/O primitives to interleave
   unpredictably — `println` now builds its newline into the string
   itself (`.concat("\n")`) before one `write` call, rather than a
   second syscall or a different C function. This is the final,
   already-shown `println`/`print` source above.

Chunk 1's `fflush(NULL)` fix in the interpreter CLI (`main.rs`) is KEPT
even though `println`/`print` no longer need it (both now use the
unbuffered `write` syscall) — it remains good, general defensive
practice for any OTHER extern call a user's own program might make
through buffered C stdio (`printf`, `fputs`, ...).

Tests: 2 new native compile-and-run tests (`print` produces no
newline; `print`/`println` mixed in one program, proving the fixed
ordering precisely — this is the test that originally caught bug 2)
and 1 new interpreter-path test. Workspace now 1388 tests (up from
1385 — net +3), clean build, zero warnings. Verified independently: a
temporary diagnostic test dumping the actual generated LLVM IR is what
found bug 1 precisely (reading `@plum_rc_dec_str` being called before
the length load, not guessing from the symptom alone); the full suite
was re-run after each fix, not just at the end; and a real throwaway
Plum project mixing `print`/`println` was built and run through BOTH
the native `build` and interpreter CLI paths by hand, output identical
and correctly ordered through both.

### Chunk 4: `Set` algebra, `set_from_array`, `map_from_arrays` — and two real compiler bugs found and explicitly deferred, not worked around

Asked to extend collections further; scoped, deliberately, to the
smaller/safer of two options (more operations vs. building real
structural equality for non-scalar keys — see "Open questions").
Presented as a real fork via `AskUserQuestion` rather than picked
silently; the user chose the smaller option.

Adds `set_union`/`set_intersection`/`set_difference` (all built from
the existing `set_contains`/`set_insert` primitives, no new backend
work), `set_from_array`, and `map_from_arrays(keys, values)` (zips two
parallel arrays by index — deliberately NOT `map_from_array(Array
[Tuple[K,V]])`: tuples still aren't safely codegen'd through a type
signature, the same reason `Map`/`Set` themselves are recursive enums
rather than `Array[Tuple[K,V]]` in the first place — see chunk 2).

**Two real, previously-unknown compiler bugs were found while probing
what looked like an even smaller, safer scope (`map_keys`/`map_values`/
`set_to_array` — converting a `Map`/`Set` INTO a fresh `Array`) —
neither was worked around; both are explicitly deferred to their own
future chunk:**
1. **An empty array literal (`[]`) can't cross a generic-function-call
   boundary.** `write(1, s.as_cstr(), n)`-style code (chunk 3) already
   proved ordinary values flow through `unsafe`/generic bodies fine —
   but passing `[]` itself AS AN ARGUMENT into a generic function
   (e.g. `map_keys_into(m, [])`, even with an explicit `Array[Int]`
   type annotation forcing it concrete beforehand) hits a hard internal
   codegen error: `an empty array literal reached the non-empty
   array-literal codegen path`. Monomorphization already threads a
   closure's own concrete per-instantiation type across this exact kind
   of boundary (`extra_closure_types`, see "Closures inside generic
   functions" above) — it never grew the equivalent mechanism for an
   empty array literal's element type. This blocks any `Map`/`Set` ->
   fresh-`Array` conversion that needs to build up from `[]` inside a
   generic function.
2. **A closure passed to `.fold()` that calls a CURRIED (multi-param)
   function produces invalid LLVM IR.** `arr.fold(seed, |acc, x|
   set_insert(acc, x))` (`set_insert` is curried, `(s: Set[T]) (x: T)`)
   — `clang` rejects the emitted IR outright: `cannot guarantee tail
   call due to mismatched parameter counts` on a `musttail call` to the
   curried callee. Isolated by direct A/B testing: replacing the
   `.fold()`+closure with a plain hand-written index-based recursive
   loop (no closure at all) compiles and runs correctly. `set_from_array`/
   `map_from_arrays` below both use the index-based form specifically
   to route AROUND this bug, not to hide that it exists — it would
   still bite any future stdlib code (or user code) wanting to use
   `.fold()` with a curried callee.

Both bugs were found via real, direct probing (a throwaway test added
and run, then deleted, matching this whole project's established
practice) BEFORE committing to a final design — not discovered after
shipping something broken. Given the depth of both (real compiler-
internals work, not stdlib-source changes), presented the finding back
to the user via `AskUserQuestion` (ship what works now and defer the
bugs / fix the empty-array bug first / park the whole chunk) rather
than pushing through solo — user chose to ship what works now.

Tests: 3 new native compile-and-run tests (union/intersection/
difference/`set_from_array` together; `map_from_arrays` zip-by-index
and lookup; the same combination instantiated at a `Str`-keyed type,
mirroring this project's established "prove independent instantiations"
pattern) and 1 new interpreter-path test. Workspace now 1392 tests (up
from 1388 — net +4), clean build, zero warnings. Verified
independently: re-ran the full suite after landing on the final,
working scope (not after the earlier, still-broken attempts); built
and ran a real throwaway Plum project exercising every new function
through BOTH the native `build` and interpreter CLI paths by hand,
output identical and correct through both.

### Chunk 5: fixing the two chunk-4 compiler bugs

Asked to fix both deferred bugs. Researched both in parallel via
background `Explore` agents BEFORE any design was written, each citing
exact file:line locations — this section summarizes the fix; see the
chunk-4 writeup above for each bug's own original discovery/reproducer.

**Bug 1 (`musttail` from a closure body): fixed.** Root cause, traced
precisely: `codegen_call`'s direct-callee path
(`plum-codegen/src/codegen.rs`) emits `musttail call` when `allow_
musttail && *ctx.caller_sig == sig` — an implicit assumption that `ctx.
caller_sig` is the CURRENT function's real LLVM prototype. True for an
ordinary top-level function (`emit_function`'s `param_decls`/`caller_
sig` are both built from the exact same `sig.params`) but FALSE for a
closure body: `emit_closure_body_fn` builds `caller_sig` from only the
closure's own DECLARED params, while its real `define` always prepends
an implicit `ptr %env` that `caller_sig` never counts. When a closure's
declared shape happens to structurally match a top-level function's
`FnSig` exactly (genuinely common — a fold/map callback is often
deliberately shaped to match the function it wraps, e.g. `|acc, x|
set_insert(acc, x)` vs. `set_insert(s: Set[T]) (x: T): Set[T]`), the
`caller_sig == sig` check spuriously passes, and `musttail` gets
emitted with one fewer real argument than the closure body function
actually has — `clang`/LLVM correctly rejects the resulting IR.

Confirmed the callee side was never the problem: `ctx.sigs` only ever
names real top-level functions (their `FnSig` always matches their real
prototype exactly), and Plum's curried `let f (a) (b) = ...` syntax is
a pure PARSING convenience — `parse_let_def` flattens every param group
into ONE flat arity; there's no partial-application/currying machinery
to miscalculate. So the fix targets the CALLER side specifically: a new
`Ctx::is_closure_body: bool` field, `true` only when `emit_closure_
body_fn` constructs its own `Ctx` (every other `Ctx` construction site
— ordinary functions, `@plum_init_globals`, the spawn-entry dummy
context — sets it `false`, confirmed via direct grep: exactly 4
construction sites exist in the whole codebase, no others). `codegen_
call`'s `musttail` check now additionally requires `!ctx.is_closure_
body` — a closure body simply never gets `musttail`-optimized calls to
bare-named top-level functions, regardless of signature match; still
falls back to an ordinary `call` + `ret`, correct, just not `musttail`-
guaranteed (a real but narrow, acceptable performance-only regression
for what both the original report and this research confirm is a rare
shape). Existing `self_recursive_tail_call_becomes_musttail`/`mutual_
tail_call_becomes_musttail` tests confirm ordinary (non-closure)
`musttail` optimization is completely untouched by this fix — both
pass unmodified.

Tests: the original `.fold()`+curried-closure reproducer, now compiling
and running correctly (`set_from_array`-shaped code); a dedicated IR-
shape test proving the mechanism directly (asserts an ORDINARY `call`,
not `musttail`, for a closure body's own tail call to a same-shaped
top-level function) rather than relying on the end-to-end symptom
alone.

**Bug 2 (empty array literal across a generic boundary): fixed, both
gaps, per Brad's explicit choice** (presented as a real fork via
`AskUserQuestion` — Gap A alone vs. both — rather than assumed; Brad
chose both, mirroring how "closures inside generic functions" was
fixed with the same two-part shape).

**Gap A** (the exact originally-reported failure): `plum_ir::
monomorphize::plan` never threaded `empty_array_elem_types` at all —
not a rewrite bug, a total omission. Since `MonoPlan::functions`/
`.globals` wholesale-replace `lower_program`'s own output, EVERY
function/global `plumc` actually emits — generic or not — got re-
lowered through `plan`'s own always-empty map, so ANY empty array
literal anywhere fell through to the untyped `Ctor{ARRAY_TAG, []}`
lowering arm, which codegen's own empty-fields guard explicitly
rejects. Fixed by threading `empty_array_elem_types` through `plan()`
as a new parameter and baking it into `base_lctx`, mirroring exactly
how `closure_types` was already threaded — small and mechanical,
already proven safe by that precedent.

**Gap B** (`let f[T](): Array[T] = []` — an empty array literal pinned
only via the ENCLOSING generic function's own type parameter): `Infer::
empty_array_elem_types` had no way to record which function's generics
an unresolved var might belong to, and `resolve_empty_array_elem_types`
had NO tier-2 template fallback at all — a still-unresolved `Var` was
ALWAYS a hard ambiguity error, even when it was genuinely resolvable
once monomorphization instantiated the function. Fixed by mirroring
`resolve_closure_types`'s existing tier-2 mechanism — literally reusing
`resolve_closure_component` directly (a clean, general helper it turned
out to already be shaped correctly for, rather than duplicating its
logic) — plus a new `extra_empty_array_elem_types` per-instantiation
side-channel in `monomorphize.rs`'s `RewriteCtx`, mirroring `extra_
closure_types` exactly.

**A third, related issue found only once both gaps were wired up and
actually tested end-to-end**, not predicted by the original research:
`plumc`'s pipeline eagerly lowers the ORIGINAL, un-instantiated AST
once (for globals/externs, its function output always discarded and
replaced by `monomorphize::plan`'s own — the same structural quirk
that needed a `type_contains_param` fix for closures in an earlier
chunk). With Gap B's tier-2 fallback in place, this eager pass started
hitting an empty array literal whose recorded type was still a `Type::
Param` TEMPLATE, which `lower.rs`'s existing `ArrayLiteral` lowering
arm rejected outright (`type_to_prim_ty` has no `Type::Param` case).
Fixed by giving the `ArrayLiteral` (empty-case) lowering arm the EXACT
SAME treatment the `Closure` arm already has: filter out a template-
containing resolved type via the existing `type_contains_param` helper,
falling back to the untyped `Ctor{ARRAY_TAG, []}` form (treating it as
"no info available," same as a lowering-only caller with no `Infer`
pass behind it at all) rather than erroring.

**Direct, concrete payoff**: `map_keys`/`map_values`/`set_to_array`
(`Map`/`Set` → fresh `Array` conversions) — exactly the shape both bugs
used to block — are now implemented in the stdlib, using precisely the
`[]`/`match`/generic-function pattern that was broken before.

Tests: the exact original `map_keys_into`-shaped reproducer (Gap A),
now compiling and running correctly; `let map_keys[K,V](m): Array[K] =
match m { ... MapEnd => [] ... }`-shaped code (Gap B) instantiated at
TWO different concrete types in the same compiled program; a `plum-
types` unit test for the new tier-2 template fallback directly,
mirroring the existing closure-template test; real compile-and-run
tests for `map_keys`/`map_values`/`set_to_array` themselves, plus an
interpreter-path test (confirmed, not assumed: the interpreter was
NEVER affected by either gap — `run_resolved_program` never calls
`resolve_empty_array_elem_types`/`monomorphize::plan` at all — so this
test proves the new FUNCTIONS work there, not a fix to anything that
was broken). Workspace now 1399 tests (up from 1394 — net +5),
clean build, zero warnings.

Implementation note: a background agent doing the Gap A/B plumbing
stalled mid-edit (a real, infrastructure-level failure, not a design
problem) partway through — the `Task::Global` branch's `RewriteCtx`
construction was left incomplete, and every caller of `monomorphize::
plan` still needed the new parameter threaded through. Took over and
completed it directly rather than re-delegating, since the exact
mechanism (mirroring `extra_closure_types`) was already fully
understood from the research and the `Task::Function` branch the agent
DID finish provided a complete, correct template to mirror exactly.
Verified independently at every stage — forced rebuilds before
trusting diagnostics, re-ran the full suite after each fix, read the
actual diffs directly (not just trusted a "looks done" impression),
and built/ran a real throwaway Plum project through BOTH the native
`build` and interpreter CLI paths by hand for the final, complete
fix — output identical and correct through both.

### Chunk 6: real structural equality for structs, enums, and arrays (both backends) — closes an "Open questions" item

Chunk 4 deliberately deferred this ("more operations vs. building real
structural equality for non-scalar keys" — the user picked the smaller
option then). Asked directly for it now: `Map`/`Set` keys were
restricted IN PRACTICE to `Int`/`Float`/`Bool`/`Str`, since the `Eq`
bound didn't actually enforce backend support — a struct/array key
type-checked fine and only failed later, at codegen/runtime.

**Interpreter** (`plum-interp::values_equal`): a small fix, closing a
documented gap. Structs, enum variants, arrays, AND tuples all already
shared one dynamically-typed runtime representation (`heap::CellData::
Ctor { tag, fields }`), so one recursive tag-then-fields comparison
covers all of them uniformly — no per-shape special-casing needed,
mirroring the existing `to_portable` (spawn/channel deep-copy) walk.
Provably safe from infinite recursion without any cycle-detection
logic: genuine reference cycles are only reachable through the
separate `Ref[T]` heap-cell kind (see "Mutability and cycles" above),
which already does identity-only comparison and never recurses into
its `RefCell` contents.

**LLVM codegen**: bigger, needed new runtime primitives, but followed
an existing architectural precedent exactly rather than inventing a
new shape. Structs/enums both compile to ONE erased `CgType::Heap`
variant, so `@plum_struct_eq` is a single, generic, runtime-tag-
dispatched function — a direct structural sibling of `@plum_release_
fields` (same sequential `icmp`/`br` dispatch chain over every known
tag, not an LLVM `switch`), emitted once per program regardless of how
many struct/enum types it declares. Arrays are NOT part of that
tag-dispatch system at the LLVM level (a dedicated `{refcount, len,
elems}` layout, no tag word at all), so they get their own function
family instead — `@plum_array_eq_<mangled>`, one per distinct element
`CgType` actually used in the program, discovered via the existing
`Ctx::needed_arrays` machinery and emitted alongside `emit_array_
release_fns`. Both dispatch recursively into nested heap-shaped fields/
elements via a new `eq_fn_for` table (a direct sibling of `dec_fn_for`/
`deepcopy_fn_for`), and both short-circuit to "not equal" as soon as
any mismatch is found (a length mismatch for arrays; a tag mismatch,
or any field mismatch in declared order, for structs/enums) rather
than always visiting every field/element. Wired into `codegen_binop`
via the same "call-shaped instruction, not a bare `icmp`" branch the
existing `Str` `==`/`!=` support already established.

**Tuples are a genuine, structural gap in codegen specifically — left
explicitly out of scope, not silently unsupported.** `CgType` has no
`Tuple` variant at all (`plum_type_to_cg_type`'s own doc comment: a
tuple only ever reaches codegen as a fully-destructured LOCAL value,
never through a signature); the flat, non-type-specialized tuple tags
(`"2Tuple"`, etc.) can't safely distinguish two different concrete
tuple instantiations sharing the same arity — the same class of
problem that already forced `Map`/`Set` to use recursive generic enums
instead of `Array[Tuple[K,V]]`. Real tuple equality in codegen needs
per-shape (monomorphized) tuple codegen types first, a separate future
chunk. The interpreter has NO such limitation (tuples share the fully
dynamically-typed `Ctor` shape, no static collision risk), so this is
a genuine, deliberate asymmetry between the two backends: tuple `==`
now works completely in the interpreter, but stays unsupported in
native codegen.

`satisfies_bound`'s `Eq` case (`plum-types::infer`) is now tightened to
match: `Tuple` is explicitly excluded (`Show`'s own bound is untouched
— its narrower, `Int`/`Float`/`Bool`/`Str`-only support is a separate,
pre-existing gap this chunk doesn't touch). This only affects a
GENERIC `[T: Eq]` bound being instantiated at `T = Tuple` (e.g.
`Set[Tuple]`'s `set_insert`) — a direct, concrete `(1,2) == (1,2)`
isn't gated by `satisfies_bound` at all (only generic bound
instantiation goes through it), so it still type-checks and runs fine
in the interpreter, and would only fail in native codegen's already-
existing generic `Err` fallback if ever reached that way.

**Direct, concrete payoff**: a `Map`/`Set` keyed by a STRUCT now
genuinely works end to end, not just type-checks — verified through
both backends, including a real throwaway Plum project built and run
via the actual `plumc` CLI (`build`, native, AND plain interpreter
invocation — output identical and correct through both).

Tests: struct equality (matching/mismatched fields), enum-variant
mismatch (`Circle(1.0) == Square(1.0)` → false), nested struct-
containing-struct equality, array-of-structs equality, a deep
recursive-enum (`List`) equality check proving recursion actually
terminates correctly rather than just working for one flat struct, a
struct-keyed `Map` test (the direct payoff), and a negative test
confirming a tuple element in a `Set` is now rejected at type-checking
time by the tightened bound. Each backend independently (interpreter
tests in `plum-interp`; native compile-and-run tests plus two direct
emitted-IR assertions — `call i1 @plum_struct_eq` and `call i1
@plum_array_eq_Int` actually appear — in `plumc`). Workspace now 1414
tests (up from 1399 — net +15), clean build, zero warnings.

### Chunk 7: real `.to_string()`/`println` support for structs, enums, and arrays (both backends), plus a float-formatting fix

Direct follow-on to chunk 6: `.to_string()`/`println` were still scoped
to `Int`/`Float`/`Bool`/`Str` only. Three scope questions were resolved
with the user via `AskUserQuestion` before design started: (1) structs
render with NAMED fields (`Point { x: 1, y: 2 }`), not positionally —
enum variants stay positional (`Circle(5.0)`), since that already
matches their own construction syntax, Plum variants have no named-
field form at all; (2) fix, in this same chunk, a real pre-existing
divergence this work makes newly visible: the interpreter rendered
`Float` via Rust's `Display` (`3.0` → `"3"`), native codegen via
`printf`'s `%f` (always `"3.000000"`); (3) `Map`/`Set` get NO bespoke
pretty-printing — they're plain recursive generic enums under the
hood, so generic enum rendering gives `MapNode(1, 100, MapNode(2, 200,
MapEnd))` for free, shipped as-is.

**The actual gate for `.to_string()` was never `satisfies_bound`** (that
only fires for a GENERIC `[T: Show]` bound instantiation, and was
already permissive) — it's a separate, narrower, DEDICATED inline check
at the `.to_string()` call site itself (`plum-types::infer`), previously
hard-coded to `Int | Float | Bool | Str | Var(_)` only. Widened to also
permit `Struct`/`Enum`/`Array`, still excluding `Function`, the opaque
runtime handles, and `Tuple` — `CgType` still has no `Tuple` variant
(same structural blocker as `Eq`). Unlike `Eq`, this ONE check governs
`.to_string()` everywhere (not just generic-bound instantiation), so
excluding `Tuple` here blocks it uniformly across both backends with
one clear message — a cleaner outcome than `Eq`'s more accidental
backend asymmetry, though a curiosity survives: `.to_string()` called
INDIRECTLY through an already-generic function body (where the
parameter's concrete type is only known at a later call site, not at
the check site) still isn't re-verified per instantiation — this is a
pre-existing, narrower gap in how generic type-checking here works at
all, not something this chunk introduces or fixes; found and pinned
down via a test that had to be redesigned around it (see below).

**Struct field NAMES were available all along, just discarded at the
same place `Eq`'s deep-dive found tag/type info being discarded.**
`plum_ir::lower::LoweringContext` already builds a `struct_fields:
HashMap<String, Vec<String>>` table (field names in DECLARED order) —
purely to order a struct literal's fields into positional `Ctor` slots
— and simply never exposed it. A one-line accessor was enough to give
the interpreter everything it needed. Codegen's own `TagFields`/
`monomorphize::Plan::tag_fields` derivation sites (`derive_tag_fields`,
`monomorphize::plan`'s `Task::Struct` arm) were ALSO already iterating
`(name, type)` pairs and discarding the name half (`(_, ty)`) — kept
this time into a new parallel `StructFieldNames` table, threaded
through `emit_program` exactly like `tag_fields` itself. Enum variant
tags deliberately get NO entries in that table — Plum enum variants are
already positional at the language level, so a tag absent from
`StructFieldNames` renders positionally (or bare, if zero-field) as the
CORRECT form, not a fallback for missing data.

**Interpreter**: `Interpreter` gained a `struct_field_names` field and
`set_struct_field_names` setter (a setter, not a `load_program`
parameter — that method has ~13 call sites across this crate's own test
suite, almost all unrelated to this feature; an unset/empty table just
renders every struct positionally too, same as an enum variant, never a
crash). `Expr::ToString` now recurses through a new `render_value`
helper mirroring `values_equal`'s own recursive `Ctor` walk — same
cycle-safety argument (real cycles only reachable through `Ref[T]`,
never ordinary struct/enum/array nesting). A nested `Str` value renders
QUOTED and escaped (new `escape_str_for_display`, `\`/`"` only); a bare
TOP-LEVEL `.to_string()` on a `Str` is unchanged — still raw/unquoted.

**LLVM codegen**: the float fix is small and separate — `Expr::ToString`'s
`Float` arm now formats via `%.15g` instead of `%f` (`%g` already omits
trailing zero decimals for whole numbers; 15 significant digits matches
`f64`'s own precision) — closely, not byte-perfectly, matching the
interpreter (extreme-value exponent formatting can still differ,
`1e+20` vs Rust's `1e20` — a documented, honest caveat). Struct/enum/
array stringification needed genuinely NEW runtime primitives — unlike
equality, which only needed to combine booleans, this needs to BUILD
strings, interleaving literal skeleton text with recursively-rendered
values. `@plum_struct_to_string` mirrors `@plum_struct_eq`'s tag-
dispatch-chain shape exactly (one block per tag, sequential `icmp`/
`br`), but each tag's block assembles a fresh `Str` cell via repeated
`@plum_str_concat` calls. Static skeleton text (`"Point { x: "`, `",
"`, `" }"`, a bare enum tag, punctuation) costs ZERO runtime allocation
at all — each piece is a compile-time LLVM constant struct literal
built directly in the `{ i64 refcount, i64 len, bytes }` Str-cell
layout (`tostr_lit_cell`), passed by its `@name` wherever a `ptr` to a
Str cell is expected, exactly like any real allocated cell (every
access anywhere in this backend already reads cells through raw
`getelementptr i8` byte offsets, never a typed struct GEP, so the
declared LLVM type doesn't need to match runtime-allocated cells'
shape). A new `@plum_str_quote` (two `phi`-loop passes, mirroring
`emit_array_release_fns`'s established loop shape: count how many bytes
need an escape first, to size ONE allocation exactly, then copy-and-
escape) handles nested `Str` values. Arrays get their own per-element-
type function family, `emit_array_to_string_fns`, mirroring
`emit_array_eq_fns`'s per-distinct-element-`CgType` discovery/emission
shape exactly. A new `render_word_as_string`/`to_string_fn_for` pair
(direct siblings of `eq_fn_for`/`dec_fn_for`) is shared by both the
struct and array runtime functions for scalar-vs-heap-shaped dispatch.

Two real bugs were found and fixed only once actual `clang`-compiled
IR was tried, not caught by `cargo build` alone (LLVM textual IR is
only checked by LLVM itself, never by the Rust compiler): (1) the two
hand-counted shared format-string byte-array globals (`@plum_tostr_
fmt_int`/`@plum_tostr_fmt_float`) were declared one byte short/long —
`clang` caught the exact mismatch immediately, a "constant expression
type mismatch" error; (2) `@plum_struct_to_string`/`emit_array_to_
string_fns` each independently start numbering their compile-time
literal globals from 0, so BOTH produced a colliding `@plum_tostr_
lit_0` the first time a real program (rather than an isolated unit
test) needed both in the SAME compiled module — fixed by giving each
its own name prefix, since — unlike a shared Rust-side counter, which
would need threading across two otherwise-independent call sites —
distinct prefixes need no shared state at all. A third, Rust-level-
only bug (caught by the Rust compiler itself, not `clang`): the Bool/
Unit rendering branch's basic-block LABELS were built from the SAME
`%v<N>`-prefixed helper used for SSA REGISTER names, producing
malformed `label %bl%v5`-shaped IR text — fixed by using a separate,
plain-numeric label-id source sharing the same counter (so it still
can't collide with any register name) but without the `%v` prefix.

Two pre-existing tests needed updating, both because the FIX genuinely
changed previously-broken behavior, not because anything regressed:
`println` output for a whole-number float (now `3.5`... `3` not
`3.500000`), and a test that used to prove "an unsupported type reached
only through a generic parameter is caught at runtime" using
`Array[Int]` as its example — now genuinely supported, so retargeted at
a `Closure` (which is excluded by the `.to_string()` gate outright AND
genuinely unsupported by the interpreter's own rendering, so it still
demonstrates the same runtime-catches-what-compile-time-permissively-
let-through behavior the original test existed to prove). Also found,
independently, while writing new tests: the Plum SOURCE keyword for the
string type is `String`, not `Str` (`Str` is only `Type::Str`'s
internal Rust name; `plum_types::infer::ast_type_to_type` — the
resolver EVERY struct/enum field and return-type ANNOTATION goes
through — only recognizes `"String"`) — a few new test sources
initially used the wrong keyword and were fixed, not a real bug.

**Direct, concrete payoff**: `println`/`.to_string()` now work for real,
nested, mixed-type program values — verified through both backends,
including a real throwaway Plum project (a struct, an array of structs,
an enum variant, whole-number and fractional floats, and a `Map`) built
and run via the actual `plumc` CLI (`build`, native, AND plain
interpreter invocation — output identical and correct through both).

Tests: named-field struct rendering, nested struct-in-struct, enum
variant rendering (bare zero-field and positional-with-payload), array
rendering, array-of-structs, a `Str` field's quoting/escaping (including
an embedded `"`/`\`, round-tripped through both a Plum-source literal
and its expected escaped rendering — written as raw Rust strings to
keep the double layer of escaping legible), a bare top-level `Str`
staying unquoted (regression guard), `Map` generic-form rendering (and
the `map_insert`-prepends-so-most-recent-is-outermost ordering that
briefly looked like a real bug and wasn't), the float-format fix itself,
and direct emitted-IR assertions that `@plum_struct_to_string`/
`@plum_array_to_string_<mangled>` actually appear. Each backend
independently. Workspace now 1436 tests (up from 1414 — net +22), clean
build, zero warnings.

### Chunk 8: basic file I/O — `read_file`/`write_file`, `Result[T, Str]`-returning, and a real previously-latent `Span` collision bug found and fixed

Next stdlib area after equality/`.to_string()`, with JSON planned as a
direct follow-on. Two scope questions were resolved with the user via
`AskUserQuestion` before design: (1) whole-file convenience functions
(`read_file(path): Result[Str, Str]` / `write_file(path, contents):
Result[Unit, Str]`), not a stateful file handle — no `open`/`read`/
`write`/`close` sequence, no new resource-lifetime story; (2) failures
surface as `Result[T, Str]`, not a runtime abort — the first stdlib
function to ever return `Result` (it already existed in the prelude as
a plain generic enum, but nothing constructed one until now).

**Read genuinely needs new core-language primitives — write doesn't.**
The extern FFI type system is a closed list (`Int`/`Float`/`Bool`/
`CStr`/callback/struct-of-those) with no raw-pointer/buffer type at
all, so `write` (what `println`/`print` already use) fits as an
ordinary `extern "C"` prelude declaration, but `read` fundamentally
needs a mutable out-buffer nothing in the FFI surface can express.
Two low-level IR primitives were added, `ReadFileRaw`/`WriteFileRaw`
(recognized via the SAME bare-`Ident`-named-call shape `ref(v)` already
established, not a new AST/grammar addition), each evaluating to one
new, ordinary, NON-generic prelude struct — `struct __FileIoResult {
ok: Bool, payload: Str }` (`payload` is dual-purpose: file contents on
a successful read, the OS error message on any failure). Being
non-generic, `__FileIoResult` needs zero new tag-registration
machinery. `read_file`/`write_file` themselves are then just ordinary
PRELUDE PLUM SOURCE (like `println`), translating the raw result to
`Ok`/`Err` via a plain `if` — meaning `Result` construction goes
through the exact same path any user program's `Result` usage would.
Two design traps were found and avoided during research: a
tuple-returning primitive was rejected (`2Tuple` is one flat tag
shared by every 2-element tuple program-wide — a compiler-internal
usage could silently collide with an unrelated user tuple); a
hand-registered generic-tag-forcing approach (mirroring
`register_channel_tag`) was rejected as more machinery than needed
once the plain-struct design was found.

**Codegen is a direct sibling of `codegen_as_cstr`, not a hand-rolled
shared runtime function** — unlike equality/`.to_string()` (needing
ONE function dispatched by runtime tag across every shape),
`read_file_raw`/`write_file_raw` are a single fixed operation, ordinary
per-call-site `Emitter`-API codegen. Every `Str` cell already carries a
guaranteed trailing NUL byte, so a path/contents `Str`'s bytes pass
directly to `@fopen` with no `.as_cstr()` copy/RC-dec dance needed.
`@fopen`/`@fread`/`@fwrite`/`@fclose`/`@fseek`/`@ftell`/`@rewind`/
`@strerror`/`@__errno_location` are declared only when actually used
(a new `Ctx::needs_file_io_runtime` flag, mirroring `needs_spawn_
runtime`/`needs_channel_runtime` exactly). `@fseek`+`@ftell`+`@rewind`
size the file before allocating its `Str` cell in one shot, so `@fread`
writes straight into the final cell — no separate copy. The final
`__FileIoResult` value is built via the existing, directly reusable
`codegen_ctor_alloc` helper — the same one ordinary `Ctor` construction
already uses. Interpreter side is a few lines of ordinary Rust
(`std::fs::read_to_string`/`std::fs::write`) — no new architecture.
Neither `read_file` nor `write_file` needs `unsafe {}` — like `.to_
string()`/`ref(v)`, these are core-language builtins, not extern calls.

**A real, previously-latent bug was found and root-caused, not worked
around: `Span`-keyed lookup tables silently collide across
independently-lexed source fragments.** `with_prelude` parses each of
its four fixed source strings (plus, separately, the user's own
program) with its OWN fresh `Lexer`, each restarting byte-offset
counting at 0 — meaning `Span`s are only unique WITHIN one fragment,
never across the merged whole. `plum_types::infer::Infer::generic_
sites` is a `HashMap<Span, RawSite>`; adding `STDLIB_FILE_SRC` shifted
byte offsets just enough that `write_file`'s own `Ok(())` construction
site landed at the EXACT SAME numeric span as `map_get`'s unrelated
`MapEnd` construction in the entirely separate `STDLIB_COLLECTIONS_
SRC` string, silently clobbering it in the hashmap — `write_file`'s
`Ok` was never renamed to its mangled tag, and codegen failed with
"unknown tag \"Ok\"" for EVERY compiled program (`write_file` is a
non-generic prelude function, always present, always processed,
regardless of whether the test's own entry point ever calls it). Found
via a binary-search-style sequence of isolated repro programs (each
successive simplification worked fine — curried functions, `if`/`else`,
`Unit` payloads, struct field access — until the exact real prelude
source was reproduced verbatim, pinning the collision to `with_
prelude`'s span-merging itself, not any single new node's logic).
Root-cause fix, not a narrow workaround: `Lexer` gained a `with_base_
offset(source, base)` constructor (an opt-in sibling of `new`, zero
behavior change for any existing caller) that offsets every emitted
`Span`'s byte range by `base`; `with_prelude`'s own loop now threads a
running cumulative base across its four fragments, and a new `plumc::
PRELUDE_TOTAL_LEN` compile-time constant (just summed `&str` lengths,
no lexing needed) lets every OTHER entry point that lexes a user's own
top-level source (`typecheck_and_run`, `compile_to_ir`, `resolve_
modules`'s per-file loop, a test helper) start its own `Lexer` safely
PAST every prelude fragment's range. `resolve_modules`'s per-file loop
needed the same treatment for a second reason: multiple module files in
one project had the identical latent collision risk against EACH OTHER,
not just against the prelude — previously unexercised since no existing
multi-module test's span ranges happened to collide, but the SAME class
of bug, fixed by the same mechanism.

Also found, independently, while writing new tests: the Plum SOURCE
keyword for the string type is `String`, not `Str` (`Str` is only
`Type::Str`'s internal Rust name; `ast_type_to_type` — the resolver
every struct/enum field and return-type annotation goes through — only
recognizes `"String"`) — not a new discovery (chunk 7 hit the exact
same thing), but confirmed again while writing `__FileIoResult`'s own
declaration.

**Direct, concrete payoff**: verified through both backends, including
a real throwaway Plum project — write a file, read it back, attempt a
read of a nonexistent path — built and run via the actual `plumc` CLI
(`build`, native, AND plain interpreter invocation), a REAL file on
disk in both cases, output equivalent through both (error message
WORDING legitimately differs — the interpreter's is `std::io::Error`'s
own Rust-standard-library text, codegen's is glibc's `strerror(errno)`
— both correctly convey the same real OS error, a documented, honest
difference, not a bug).

Tests: this is the FIRST chunk where any Plum program touches a real
file on disk (no `tempfile`/`NamedTempFile` precedent existed before)
— `std::env::temp_dir()` + a unique per-test filename, mirroring
`unique_temp_dir`'s own existing convention. Write-then-read round
trip, read of a nonexistent path, write to an invalid (nonexistent
parent directory) path — each backend independently. Workspace now
1442 tests (up from 1436 — net +6), clean build, zero warnings.

### Chunk 9: JSON — `json_parse`/`json_stringify`, pure Plum prelude source, plus two real compiler bugs found and fixed

Next stdlib area after file I/O, explicitly named by the user as the
intended direct follow-on. Two scope questions were resolved with the
user via `AskUserQuestion` before design: (1) API surface is **parse +
stringify only** (`json_parse(s: String): Result[JsonValue, String]` /
`json_stringify(v: JsonValue): String`) — no separate accessor helpers;
`JsonValue` is a plain enum, so callers use ordinary `match`/`Array`
operations directly; (2) **no max-nesting-depth guard** — pathological
deeply-nested input could in principle exhaust the native stack (no
depth guard, no tail-call shape for recursive descent), but realistic
JSON is never remotely close to that deep, so this was left unguarded
like a typical hand-rolled recursive-descent parser.

**Unlike file I/O, this needed ZERO new IR/codegen work** — everything
JSON parsing/serialization needs was already expressible in pure Plum:
recursion, `Array`'s `.push()`/`.map()`/`.filter()`, `String`'s
`.split()`/`.concat()`/`==`/`.len()`, `Float`'s `.to_string()` (chunk
7), generic structs (`ParseResult[T]`, mirroring `Map[K,V]`), and
self-referential enums including through `Array` (`JsonArray(Array
[JsonValue])` — the first time this codebase nested a self-referential
array field inside a type reached through generic-struct
monomorphization; confirmed structurally sound and verified with an
early, dedicated smoke test before deeper investment, per the plan).
The whole implementation is one new prelude source constant,
`STDLIB_JSON_SRC`, merged into `with_prelude` exactly like `STDLIB_
FILE_SRC`/`STDLIB_COLLECTIONS_SRC` — no new `Expr` variant, no
type-checker/lowering/codegen change anywhere.

**Two real language gaps were sidestepped by design, not fixed**: no
substring/codepoint-to-string primitive exists at all (`s[i]` returns a
raw byte, `.runes()` returns codepoints, no `Int → String` builder in
either direction) — worked around by parsing over `chars_of(s):
Array[String] = s.split("").filter(|c| c != "")` (Rust's own
`str::split("")` semantics give one-character `String` elements
directly, sidestepping the gap entirely for both comparison and
`.concat()`-based accumulation). No `Int`-to-`Float` cast exists —
worked around via `digit_value(c: String): Result[Float, String]`, a
10-way `if`/`else` chain mapping each digit character straight to its
`Float` literal, so number parsing accumulates in `Float` from the
first digit, no cast ever needed. Object entries use a dedicated
`JsonEntry { key: String, value: JsonValue }` struct, not a tuple —
the same reason `Map`/`Set` use recursive generic enums instead of
`Array[Tuple[K,V]]` (no `CgType::Tuple`; a flat `"2Tuple"` tag can't
distinguish different concrete tuple instantiations). Escaping support
is deliberately narrower than full JSON: only the six escapes Plum's
own string-literal lexer can itself produce (`\"`, `\\`, `\/`, `\n`,
`\r`, `\t`) round-trip; `\b`, `\f`, and `\uXXXX` are rejected with a
clear `Err` on parse (no way to construct a literal backspace/form-feed
/arbitrary-codepoint `String` without an `Int`-to-`String` primitive
that doesn't exist) and never emitted on stringify — a real, honest,
narrower-than-strict-JSON scope boundary, matching this project's
established style for documenting such gaps rather than hiding them.

**Two real, previously-undiscovered compiler bugs were found and
root-caused while building this, not worked around:**

1. `monomorphize::validate_field_type` didn't recognize `Array`/`Task`/
   `Sender`/`Receiver`/`Ref` as the opaque pseudo-generic builtin types
   they are (already special-cased identically in `plum_types::infer::
   ast_type_to_type`, but that special-casing was never mirrored here).
   `ParseResult[JsonValue]` — a real generic struct instantiated at
   `JsonValue`, itself having an `Array[JsonValue]` variant field — is
   the first time this codebase ever nested an `Array`-typed field
   inside a type reached through generic-struct monomorphization; every
   earlier generic struct/enum test happened to avoid that combination.
   Fixed by recursing into the wrapper's own type arguments (registering
   `JsonValue`'s `Task::Enum`, not a nonexistent `Array` struct
   declaration) instead of pushing a doomed `Task::Struct("Array", ...)`.
2. Ten separate reuse-in-place codegen sites (`CtorReuse`, the four
   `Array*Reuse` variants, `StrConcatReuse`/`StrTrimReuse`, str-upper/
   str-lower, `StrReplaceReuse`) all built their final merge `phi` using
   the branch's ENTRY label rather than its actual exit block — a
   documented `Emitter::start_block` footgun, silently violated at every
   one of these ten sites until now, invisible until a branch's own
   codegen (e.g. `codegen_array_push_fresh` calling `inc_copied_array_
   elements`, which opens its own nested nested blocks for heap-shaped
   element types) changed the current block before reaching its final
   `br`. Surfaced as `clang` rejecting the generated IR outright ("PHI
   node entries do not match predecessors") the first time this project
   ever combined array-of-heap-values push with a value threaded through
   a reuse-in-place branch — JSON's own `Array[JsonValue].push(...)`
   accumulator pattern, used throughout `parse_array_entries`/`parse_
   object_entries`. Fixed identically at all ten sites: capture `em.
   current_block()` right before each branch's own final `br`, and phi
   against THAT captured label instead of the original branch-entry one.

Both bugs were found via careful IR-level bisection (isolating a
minimal `ParseResult[JsonValue]`-only repro for the first; a direct
`.ll` dump comparing the phi's claimed predecessor against the actual
generated block structure for the second), fixed at the root cause, and
verified to cause zero regressions across the full workspace suite.

**A third, narrower issue** surfaced as an expected consequence of
prelude growth, not a regression: an existing native-codegen test
asserting NO `musttail call` appeared anywhere in a whole compiled
program broke once the JSON prelude's own legitimately tail-recursive
`skip_ws` became part of every compiled program. Scoped the assertion
to the specific call site it was actually testing (`@add_one`) instead
of the whole program.

**A fourth issue, confirmed as a test-harness artifact, not a product
bug**: the interpreter's `Interpreter::eval` has no tail-call
optimization at all (a native-codegen-only guarantee, via `musttail`),
so each one of this recursive-descent parser's Plum-level calls fans
out into many nested, unbounded-native-stack-growth `eval` calls for
its own sub-expressions. Even a two-element array (`[1, 2]`) was enough
to overflow `cargo test`'s own default ~2 MiB worker-thread stack.
Confirmed via the real `plumc` CLI (whose main thread gets the OS
default ~8 MiB) that ordinary and even fairly large real JSON documents
parse/stringify correctly with no special handling at all — this is
purely about `cargo test`'s own narrower default, not a real user-
facing limitation. Fixed on the test side only: the interpreter-path
JSON tests now run on a dedicated 16 MiB-stack spawned thread (a small
`run_json_test` helper in `plumc::lib.rs`'s test module).

**Direct, concrete payoff**: verified through both backends (8 new
tests: 4 interpreter-path via `run_json_test`, 4 native-codegen-path
via `compile_and_run`), plus a real throwaway Plum project — parse a
real multi-field JSON document (nested object/array/bool/null/string/
number), inspect a field, re-serialize it, re-parse the result, and
check structural equality against the original — built and run through
BOTH `plumc build` (native) and the plain interpreter CLI, identical
correct output through both. Workspace now 1450 tests (up from 1442 —
net +8), clean build, zero warnings.

**Follow-on**: a dedicated integration test (both backends) composing
chunk 8's `write_file`/`read_file` with chunk 9's `json_parse`/`json_
stringify` end to end — build a `JsonValue`, `json_stringify` it,
`write_file` the result to a real temp file, `read_file` it back, `json
_parse` the contents, and check structural equality (`==`) against the
original value — all chained through ordinary `match`-based `Result[T,
String]` propagation with no glue code needed on either library's part.
Confirms the two chunks compose the way a real caller would use them
together, not just independently. Workspace now 1452 tests (net +2),
clean build, zero warnings.

### Chunk 10: `Option`/`Result` combinators — pure Plum prelude source, one real inference-poisoning bug found and fixed

After the testing framework closed out the docs/testing-framework/
toolchain pivot, the user asked what the stdlib should have next.
Auditing `crates/plumc/src/lib.rs` directly (not guessing) turned up
the single biggest, most surprising gap: `Option[T]`/`Result[T, E]`
had **zero** combinator methods — one of this file's own tests already
hand-rolled `unwrap_or` from a bare `match` because there was nowhere
to get one. Given a menu of candidates (Option/Result combinators,
number/array/string utilities, HTTP client, env/time), the user picked
Option/Result combinators first — pure Plum, no new primitives, and
the highest-leverage gap since every fallible stdlib function already
returns `Result`.

**Named `option_*`/`result_*`, not bare `map`/`unwrap_or`/`and_then`.**
There is no dot-method overloading by receiver type in this language —
confirmed by how `map_get`/`set_insert` are always called as plain
`map_get(m, k)`, never `m.map_get(k)`; the `.name(...)` call shape is
reserved for a fixed set of compiler-recognized builtins matched
directly in `infer.rs`/`lower.rs` (`.map`/`.filter`/`.fold` on `Array`,
etc.). Two top-level `let map` — one for `Option[T]`, one for
`Result[T, E]` — would just be a duplicate-name error, since ordinary
function names never overload on argument type. So: `option_map`,
`option_and_then`, `option_unwrap_or`, `option_unwrap_or_else`,
`option_is_some`, `option_is_none`, `option_ok_or`; `result_map`,
`result_map_err`, `result_and_then`, `result_unwrap_or`,
`result_unwrap_or_else`, `result_is_ok`, `result_is_err`.

**Zero new IR/codegen work** — one new prelude constant,
`STDLIB_OPTION_RESULT_SRC`, merged into `with_prelude` exactly like
every prior stdlib chunk. Higher-order parameters (`f: (T) -> U`)
needed no new type-system work either — ordinary function-typed
parameters were already fully supported (confirmed via an existing
`extern "C"` callback test using the identical `(Int, Int) -> Int`
syntax).

**One real bug found and fixed, not by construction**: `option_
unwrap_or_else`/`result_unwrap_or_else` take a **zero-argument**
closure (`f: () -> T`, matching `() -> R`'s real zero-param function-
type syntax — confirmed via `parser.rs`'s own comment on that
production), but the first draft's body called it as `f(())` — a
one-argument call, arity-mismatched against the zero-param type it was
declared with. Because the ENTIRE prelude (including this one bad
function) is type-checked as a single unit prepended to every user
program, this single arity bug inside one never-yet-called prelude
function poisoned type-checking for literally every test in the
workspace — 298 of ~337 `plumc` tests failed with an unrelated-looking
`function arity mismatch` error the moment the broken source was
merged in, before a single one of the new combinators was ever
exercised directly. Root-caused by isolating which of the two new
`unwrap_or_else` variants used `f(())` instead of `f()`, fixed by
correcting the call to match the declared arity. This is the same
class of `PRELUDE_TOTAL_LEN`-style lesson as chunk 8's span-collision
bug: prelude source is not sandboxed per-fragment during type-checking,
so a mistake anywhere in it can look like a mistake anywhere else.

A related, pre-existing inference-order gotcha was hit (not fixed —
out of scope) while writing a test: `.len()`'s builtin dispatch
(`infer.rs`) checks whether the receiver's resolved type is `Str`, and
falls back to unifying it against `Array` if not — but when the
receiver is still an *unresolved* generic type variable (e.g. a
closure parameter whose type isn't pinned down until the closure is
actually applied), that fallback fires prematurely and forces the
variable to `Array`, contradicting a later `Str` use elsewhere. Worked
around in the test (used `.concat()` instead, which unifies against
`Str` unconditionally) rather than touched — a real, narrow, pre-
existing gap in `.len()`'s dispatch order, not something this chunk's
functions caused.

Verified end-to-end both backends (interpreter tests in `lib.rs`,
native-codegen counterparts in `codegen_cli.rs` compiling and running
real binaries), plus a real throwaway project run via `plum run`
confirming `option_map`/`result_unwrap_or` produce the documented
values. Workspace now 1503 tests (net +14), clean build, zero warnings.

### Chunk 11: `Int`/`Float` numeric utilities — pure Plum + libm `extern "C"`, plus a real linker bug found and fixed

Second item off the stdlib-growth list the user picked (Option/Result
combinators, then number utilities, then array, then string). Same
`int_*`/`float_*` prefixing rationale as chunk 10's `option_*`/
`result_*`: `<`/`>` only type-check against a concrete numeric type
(`infer_binary`'s `default_numeric`, not a generic `Ord`-style bound —
none exists), so a single generic `min[T]` can't serve both `Int` and
`Float` anyway, and there's still no dot-dispatch by receiver type.
`int_min`/`int_max`/`int_abs`/`int_clamp` and `float_min`/`float_max`/
`float_abs`/`float_clamp` are ordinary `if`/comparison Plum source, no
FFI. `float_floor`/`float_ceil`/`float_round`/`float_pow`/`float_sqrt`
wrap real libm functions via `extern "C"` (genuinely no pure-Plum way
to compute floor/ceil/round without bit-level float manipulation, or
`pow` without a real transcendental algorithm) — exactly the same
`extern "C" { ... }` + `unsafe { ... }` wrapping shape `print`/
`println` already use for the raw `write` syscall, so zero new
type-system or IR work either.

**One real, previously-latent linker bug found and fixed, not a
language bug**: adding `sqrt` as a prelude-level extern (backing
`float_sqrt`) collided with three PRE-EXISTING tests that each declared
their own `extern "C" { fn sqrt(...) }` — fixed by switching those
tests to `cbrt` (a different, equally-real libm function, keeping their
original intent of testing genuine extern-call mechanics), since a user
program redeclaring a prelude-claimed name is now a real, correct
"already declared" error.

Far more interesting: adding four NEW libm functions (previously only
`sqrt`/`abs` had ever been exercised, both by extern-call test fixtures
scattered across `plum-interp`, never through the prelude) surfaced a
genuine, previously-invisible linker bug. The interpreter resolves
`extern "C"` symbols via `libloading::os::unix::Library::this()` —
looking up symbols already loaded into the CURRENT PROCESS, which only
works if libm is actually linked into whatever binary is running.
`plum-interp/build.rs`/`plumc/build.rs` already anticipated exactly
this (`cargo:rustc-link-lib=dylib=m`) — but that alone wasn't enough:
the default Linux linker `--as-needed` behavior drops a shared
library's `NEEDED` ELF entry entirely if nothing in the STATICALLY
linked object files references any of its symbols, which is true here
by construction (every libm call goes through `dlsym` at runtime, never
a real linked reference the linker itself can see). `sqrt`/`abs`
HAPPENED to keep working regardless — both are ALSO present directly in
`libc.so` on this system's glibc — masking the gap for years, until
`floor`/`ceil`/`round`/`pow` (which genuinely only exist in `libm.so`,
confirmed via `nm -D`) were used for the first time and failed at
`plum test`... no, at plain `plum run`'s own hello-world smoke test,
with "undefined symbol: floor," even though nothing in that program
even calls a number function — because the interpreter eagerly resolves
EVERY declared extern at program-load time, not lazily per call, and
`float_floor`'s extern declaration is unconditionally present in every
program via the prelude.
Root-caused (not worked around) in two steps: first, adding a raw
`-Wl,--no-as-needed` link-arg alongside the existing `-l dylib=m`
directive fixed `plumc`'s own lib+test targets, but NOT the separate
`plum` bin target — because `-C link-arg`s from a build script are
appended at the very END of a DOWNSTREAM crate's own linker invocation,
AFTER that crate's own auto-derived `-lm` (re-inserted by rustc from
the upstream rlib's embedded native-lib metadata, at its NORMAL
position in the link line, still under default `--as-needed`) — so the
`--no-as-needed` arrived too late to affect it. Final fix: emit
`--no-as-needed` and a raw `-lm` back-to-back as two `-C link-arg`s
(not `cargo:rustc-link-lib`), landing adjacent at the very end of
EVERY consuming crate's own link line regardless of how many crates
deep the dependency chain goes — this SECOND, explicit `-lm` reference
is the one that lands under `--no-as-needed`, satisfying the `NEEDED`
entry unconditionally. Verified via `readelf -d` on both the debug test
binary and a real `--release` build: `libm.so.6` now appears as
`NEEDED` in both, where it was absent before.

Verified end-to-end: both backends (interpreter + native-codegen tests
for every new function), plus a real throwaway project run through
BOTH `plum run` (interpreter) and a real compiled `plum build` binary,
confirming identical numeric output on both paths. Workspace now 1509
tests (net +6 — 4 interpreter + 2 native), zero warnings.

### Chunk 12: `Array[T]` utilities — narrowed scope after finding a real, previously-latent type-inference bug (`Subst`/generalization, not fixed here)

Third item off the user's stated order (number, then array, then
string). Shipped: `array_is_empty`, `array_first`/`array_last:
Option[T]`, `array_reverse`, `array_concat`, `array_take`/`array_drop`,
`array_slice`, `array_find: Option[T]`, `array_any`/`array_all`,
`array_index_of: Option[Int]`/`array_contains` (`Eq`-bounded) — all
pure Plum, built entirely on the pre-existing `.len()`/`arr[i]`/
`.push()`/`.fold()` surface, zero new IR/codegen work.

**Deliberately NOT shipped: `array_sort_by`, `array_zip`,
`array_sum_int`/`array_sum_float`.** Building them surfaced a real,
previously-latent, and apparently non-trivial type-inference bug in
`plum-types`, hit via three distinct-looking (but likely related)
symptoms, none caused by anything unusual about the Plum source itself:

1. A self-recursive generic function with TWO SEPARATE call sites to
   itself in one body (an early `array_drop_acc` draft: one recursive
   call in each `if`/`else` branch) sent `Subst::apply` into what a
   `gdb` backtrace confirmed was genuine unbounded recursion — 100,000+
   stack frames deep before aborting, traced as far as `Infer::
   infer_if`'s own branch-unification `compose` call, not further.
   WORKED AROUND for this one function by rewriting it to a single
   call site (`rec(..., if cond { a } else { b })` — pushing the
   branch into a value-level `if` passed as an argument, instead of
   branching the call itself) — a legitimate, equally-idiomatic
   rewrite on its own terms, not a hack, but it dodges rather than
   fixes the underlying bug.
2. `array_sort_by`/`array_sort_insert`, which calls THREE other
   generic recursive helpers together in one branch (`array_concat(
   array_take(...).push(x), array_drop(...))`) alongside its own single
   self-recursive call in the other branch, hits the exact same
   infinite-`Subst::apply`-recursion symptom. No single-call-site
   rewrite is available here — the whole point of the function is
   combining three independent array operations — so this couldn't be
   worked around the same way. Cut instead of forced through.
3. Two ordinary, otherwise-UNRELATED top-level functions each calling
   `.fold()` with a two-argument closure and different CONCRETE
   accumulator types (`array_sum_int`'s `0` vs `array_sum_float`'s
   `0.0`) made a completely separate THIRD function's own `.fold()`
   call fail type-checking with a bogus "expected Int, found Float" —
   confirmed via a minimal standalone repro with no recursion involved
   at all. This is a genuinely separate-looking bug from (1)/(2), not
   the same one wearing a different hat.

**Root cause not fully traced, but the leading suspect (from reading
`plum-types/src/subst.rs` directly, not just the backtrace) is `Subst::
compose`**: individual `bind_var` calls in `unify.rs` DO occurs-check
(rejecting a variable bound to a type that contains itself), but
`Subst::compose`'s merge of two ALREADY-occurs-checked substitutions
never re-checks the MERGED result — and it's easy to construct, on
paper, two individually-acyclic substitutions (`self` has `id2 ->
Var(id1)`, `other` has `id1 -> Var(id2)`) whose `compose`d result
contains a genuine self-reference (`id1 -> Var(id1)`), which `Subst::
apply` would then recurse on forever the next time it's applied. This
would explain symptom 1 (two independently-instantiated recursive calls
to the same self-recursive generic function, each with its own fresh
type variables, threaded through sequential `if`/`else` branch
unification) but hasn't been confirmed to also explain symptom 2 or 3 —
genuinely unclear yet whether all three share one root cause or there
are two separate bugs here.

None of these three were forced through with any workaround beyond the
one explicitly noted (`array_drop_acc`'s single-call-site rewrite,
itself legitimate Plum, not a compiler-bug-shaped hack) — flagged to
the user directly rather than silently patched around or chased
indefinitely inside what was scoped as a stdlib-additions chunk,
per this project's own established discipline (root-cause real bugs,
don't work around them — and when a bug is too deep to safely fix
inline, defer it explicitly and say so, the same as chunks 4/5's own
precedent). A dedicated follow-up session should root-cause `Subst`'s
composition/generalization directly before `array_sort_by`/`array_zip`/
`array_sum_*` (or anything shaped like them, including in a FUTURE
string-utilities chunk) are attempted again.

Verified both backends (interpreter + native-codegen tests for every
shipped function) plus a real throwaway project run through both
`plum run` and a compiled `plum build` binary. Workspace now 1516
tests (net +7), zero warnings.

### Chunk 13: root-causing and fixing both `plum-types` bugs chunk 12 deferred — `array_sort_by`/`array_zip`/`array_sum_int`/`array_sum_float` now shipped

Chunk 12 shipped a deliberately narrowed `Array[T]` stdlib and flagged
two real, previously-latent `plum-types` bugs to the user instead of
working around them further. Asked how to proceed, the user chose to
pause new stdlib growth and dedicate this chunk to root-causing them
properly before resuming.

**Bug 1: `Subst::compose` could produce a self-referential cycle.**
Confirmed by reading `subst.rs` directly (not just the `gdb` backtrace):
`bind_var` occurs-checks every individual binding it produces, and
`Type::Var(a) unify Type::Var(a)` short-circuits to `Subst::empty()`
before ever reaching `bind_var` — so NEITHER input to a `compose` call
can, on its own, already contain a `k -> Var(k)` self-loop or any other
cycle. But `compose`'s merge step never re-checks its OWN combined
output. Concretely: `self = {2: Var(1)}`, `other = {1: Var(2)}` — each
individually fine (`self.apply(Var(2))` is `Var(2)` unchanged, `other.
apply(Var(1))` is `Var(2)`, both terminate) — but naive merging computes
`self.apply(other[1] = Var(2))`, which chases `self`'s own `2 -> Var(1)`
entry and lands on `Var(1)` — a genuine `1 -> Var(1)` self-loop in the
RESULT, which `Subst::apply` then recurses on forever the next time
anything looks up `Var(1)`. A short proof (in `subst.rs`'s own new doc
comment on `compose`) shows this is the ONLY new cycle shape that can
arise from merging two already-acyclic substitutions — a same-key
result can only come from `self`/`other` cross-referencing through
exactly that key, never a longer, still-hidden multi-variable cycle
(that would require one of the two inputs to already be individually
cyclic, which the invariant rules out). **Fix**: `compose` now drops
(never inserts) any merged binding that resolves back to `Var(k)` for
its own key `k` — semantically exact, not an approximation (`T_k = T_k`
after substitution is a tautology, identical in meaning to `k` having
no entry at all), and — by the same inductive argument — sufficient to
keep the "no `Subst` this codebase ever produces can loop under
`apply`" invariant intact through arbitrarily many chained `compose`
calls, not just the one call that happens to trigger it. A new
regression test (`composing_two_individually_acyclic_substitutions_
that_cross_reference_each_other_does_not_produce_a_self_loop`)
encodes the exact minimal repro directly in `subst.rs`.

**Bug 2: `default_numeric` fired too early inside closures passed
directly to `.map`/`.filter`/`.fold`.** A genuinely SEPARATE bug from
Bug 1 — confirmed by first assuming it was cross-function contamination
(two unrelated functions calling `.fold()` at different concrete types)
and then, via direct `eprintln` instrumentation of the `.fold()`
inference arm, discovering `array_sum_float`'s closure argument was
ALREADY typed `Function([Int, Int], Int)` at the moment it was
inferred — wrong regardless of whether any other function existed in
the program at all (confirmed by re-running the repro with `array_sum_
float` as the ONLY function in the source). Root cause: `.map`/
`.filter`/`.fold`'s builtin-call inference arms unify a callback
argument's inferred type against the array's ALREADY-known element
type only AFTER fully inferring the callback — but when the callback
is an unannotated closure literal, `infer_closure` mints its params as
brand-new, totally independent fresh variables, with no connection yet
to what the call site already knows. If the closure body combines TWO
still-fresh params arithmetically with no literal anywhere to pin
either side (fold's own `|acc, x| acc + x` idiom — `map`/`filter`'s
single-param callbacks were never actually affected in practice,
since their bodies almost always touch a literal, which pins the
`Var` to a concrete type via ordinary unification before `default_
numeric` is ever consulted), `default_numeric` greedily and
PERMANENTLY defaults both to `Int` right then, before the real
(possibly `Float`) constraint from the call site ever arrives. **Fix**:
`Infer::infer_closure` gained an `expected_param_types: Option<&[Type]>`
parameter — when `Some` and a param has no explicit annotation, it
seeds that param's type directly from the caller's already-known
expected type instead of minting an unconstrained fresh var. A new
`Infer::infer_expr_as_callback` helper (used by all three builtin
arms in place of a bare `infer_expr`) recognizes a callback argument
that's literally a closure literal and passes the array's already-
resolved element type (plus, for `.fold()`, the accumulator's type)
through as the expectation; anything else (a named function reference,
etc.) falls through to the ordinary path, unchanged.

Both fixes verified independently (new tests exercise the two ORIGINAL
repro shapes — the two-recursive-call-site pattern and the three-
helper-combination pattern — directly, before restoring the stdlib
functions built on them) and then together: `array_sort_by`, `array_
zip` (returning `Array[Zipped[A, B]]`, a plain struct — no tuples,
per chunk 12's own tuple-codegen-gap note), and `array_sum_int`/
`array_sum_float` are now all shipped in `STDLIB_ARRAY_SRC`, matching
chunk 12's original full design. Verified end-to-end: both backends
(dedicated interpreter + native-codegen regression tests for each
fix, plus tests for every previously-deferred function) and a real
throwaway project run through both `plum run` and a compiled `plum
build` binary, confirming identical output on both paths. Workspace
now 1522 tests (net +6 from chunk 12's 1516 — 3 interpreter array
tests, 2 native-codegen array tests, 1 `subst.rs` regression test),
zero warnings, zero regressions to the ~1500 pre-existing tests.

### Chunk 14: `Type.func(args)` — real associated functions, and migrating the whole stdlib to them

The user pushed back on the stdlib's flat prefixed naming (`option_
map`, `array_reverse`, `map_get`, ...) and asked for `Option.map`,
`Array.reverse`, `Map.get` instead — explicitly requested as a REAL
language feature (not parser-level sugar), usable by user-defined
structs/enums too (`Point.add(a, b)`, not just the seven builtin
types), with the old flat names removed entirely, no aliases. Given
the size and depth of a genuine new call-syntax feature, this went
through a full `EnterPlanMode` design pass (two rounds of direct
research reading `parser.rs`, `modules.rs`, `infer.rs`, `lower.rs`,
`context.rs`, `codegen.rs`, and `plumc`'s pipeline entry points) before
any code was written.

**The key finding that made this cheap**: `plumc::modules::qualify`
already proves, end to end through a real compiled binary, that a
function's `LetDef.name` can be an arbitrary dotted string
(`"shapes.Circle"`) with ZERO awareness needed anywhere downstream —
duplicate-name checking, monomorphization, and LLVM symbol emission
(never quoted, `@shapes.Circle` already round-trips) all just treat it
as an opaque string key. So `let Type.func (...)` needed only ONE
parser change (`parse_let_def` optionally consumes a `.` + second
ident, combining into `"Type.func"` as the stored name — `plum-syntax/
src/parser.rs`), and the call side (`Type.func(args)`, which ALREADY
parses into the identical `Call{callee: Field{base: Ident, name}, ..}`
shape `Shape.Circle(1.0)` qualified-variant-construction uses) needed
one new, small resolution pass — `plumc::assoc_fns` (new file) —
rewriting that shape into an ordinary `Ident("Type.func")` call BEFORE
type inference ever runs. **Zero changes to `infer.rs`/`lower.rs`/
codegen** — by the time inference sees `Option.map(Some(1), f)`, it's
indistinguishable from what `option_map(Some(1), f)` looked like
before.

**Disambiguation from `Type.Variant(args)`** (the pre-existing
qualified-variant-construction feature, completely untouched) is free:
this codebase's own convention is variant tags are always
UpperCamelCase, associated functions always lowercase — so `assoc_fns`
only ever matches `Type.lowercase_name`, never touching `Type.
UpperCase`. **Local-shadowing** is guarded the same way `modules::
Resolver` already guards module-qualified names (the exact bug class
that resolver was once fixed for) — a local variable/param sharing a
name with a declared type is never rewritten, ordinary field access
stays ordinary field access.

**A real gap found during implementation** (not anticipated in the
plan): `Int`/`Float`/`Bool`/`String`/`Unit`/`Array` are never declared
`ast::Item`s at all — they're hardcoded string matches in `plum_types::
infer::ast_type_to_type` — so `assoc_fns`'s type registry, originally
built only from scanning `struct`/`enum` declarations, never recognized
them; `Int.min(...)` silently fell through unrewritten. Fixed by
seeding the registry with this fixed builtin-type set directly (`Map`/
`Set` needed no such seeding — both are ordinary prelude `enum`
declarations, already covered).

**A second real gap**: two INDEPENDENT pipeline constructions —
`plumc::testing::run_tests_interpreted` and a `codegen_cli.rs` test
helper (`mono_tags`) — built their own `TypeContext::from_items`/
`infer_program` calls directly, bypassing both of the two choke points
`assoc_fns` was wired into (`lib.rs`'s `run_resolved_program_diag`,
`codegen_cli.rs`'s `compile_program_to_ir_diag`). Missed in the
original research since the pipeline-entry-point survey found only
those two `pub`/`pub(crate)` functions, not test-only helper functions
built inline. Found immediately by the full `cargo test --workspace`
run after migrating the first stdlib group (`option_is_some` used
inside `Array.contains`'s own body failed to resolve, surfacing as a
confusing \"unbound variable: Option\" from these two bypassed paths) —
both patched to call `assoc_fns::resolve_associated_calls` too.

**Migration, one stdlib group at a time, workspace-clean between
each** (matching this project's own established discipline): Option/
Result, then Int/Float, then Array, then Map/Set — every old flat name
removed, no aliases, every affected test's source string updated to
the new syntax, private recursive helpers (`array_reverse_acc` and its
many siblings) deliberately kept their plain flat names, since they're
implementation detail with no public API to namespace.

Verified end to end: a genuinely user-defined `struct Point`/`let
Point.add` (not a builtin) through both backends; `Shape.Circle(1.0)`-
style qualified variant construction confirmed completely unaffected;
the local-shadowing case (a closure param literally named the same as
a declared type) confirmed to still resolve as ordinary field access;
a real throwaway project exercising every migrated stdlib group PLUS a
user-defined associated function, run through both `plum run` and a
compiled `plum build` binary, producing identical output on both
paths; every README example re-run and confirmed to match exactly as
documented. Workspace now 1536 tests (net +14), zero warnings, zero
regressions.

### Chunk 15: `String` utilities — `String.func` from the start, plus a real, previously-latent FBIP reuse-in-place bug found and flagged (not fixed)

The last stdlib group on the user's own previously-stated order (number,
array, string), and the first to be declared as `Type.func` from the
very start (chunk 14 having just shipped that syntax and migrated the
rest of the stdlib to it): `String.is_empty`, `String.slice`, `String.
repeat`, `String.trim_start`/`String.trim_end`, `String.index_of:
Option[Int]`, `String.lines`, `String.parse_int`/`String.parse_float:
Result[_, String]`. Pure Plum, zero new IR/codegen work.

**Codepoint safety, not raw byte indexing.** `String.slice`/`String.
index_of` are built entirely on `chars_of(s): Array[String]` — the
SAME one-codepoint-per-element decomposition `STDLIB_JSON_SRC`'s parser
already established (chunk 9) — never on raw `s[i]`/`.len()` byte
indexing, which would either panic or silently split a multi-byte
UTF-8 character in half. Verified directly with `\"café\"` (`é` is a
2-byte codepoint): `String.slice(\"café\", 0, 3)` correctly yields
`\"caf\"` (3 codepoints), not a corrupted 3-BYTE prefix. `Array.slice`/
`Array.take`/`Array.drop`/`Array.reverse` (chunk 12) do the actual
positional work on the resulting `Array[String]`; a new small, non-
associated `chars_join` helper (internal plumbing, no public API to
namespace — same reasoning as `array_reverse_acc` and its siblings)
folds the array back into one `String`.

**`String.parse_float` reuses `STDLIB_JSON_SRC`'s own already-tested
`parse_number`** directly (handles sign/fraction/exponent already)
rather than writing a second float parser from scratch — just checks
`next_pos` consumed the whole input string, rejecting trailing garbage
a partial JSON-internal parse would otherwise silently accept.

**A real, previously-latent FBIP reuse-in-place bug was found while
writing this chunk's own tests, not by construction.** The first,
more obvious way to write `String.repeat` — `s.concat(String.repeat(s,
n - 1))`, `s` as `.concat()`'s receiver, the recursive call as its
argument — silently corrupted the result: `String.repeat(\"ab\", 3)`
came back with length 8 instead of the correct 6. Confirmed as a REAL
bug, not a test mistake, by isolating the minimal repro (a standalone
recursive function with the exact same shape, no `String.repeat`
involved) and confirming BOTH backends agree on the wrong answer —
ruling out a codegen-only or interpreter-only issue. Root cause not
fully traced to an exact line inside `plum-ir/src/fbip.rs` in this
chunk (the leading suspect, from the shape of the bug, is `StrConcat`'s
reuse-in-place last-use analysis mishandling a value used simultaneously
as `.concat()`'s receiver AND passed again into a nested call that is
itself `.concat()`'s own argument — analogous in spirit to chunk 13's
`Subst::compose` finding, a correctness gap in how liveness/reuse
interacts across two simultaneous uses of the same variable, though
NOT confirmed to share that exact mechanism). Rather than chase it
indefinitely inside a stdlib-additions chunk, this was flagged directly
(see the matching \"Open questions\" entry below) and WORKED AROUND for
`String.repeat` itself by writing it the other, equally correct way —
`String.repeat(s, n - 1).concat(s)`, the recursive call as receiver,
`s` as the argument — which is genuine, idiomatic Plum, not a hack. A
dedicated `codegen_cli.rs` regression test pins the ORIGINAL unsafe
shape directly (a standalone `rep` function, not `String.repeat`
itself), so future coverage of this bug doesn't depend on `String.
repeat`'s own implementation staying this exact shape.

Verified end to end: both backends (dedicated interpreter + native-
codegen tests for every function, including the codepoint-safety case
and the FBIP-bug regression test), plus a real throwaway project
exercising every new function, run through both `plum run` and a
compiled `plum build` binary, confirming identical output on both
paths. Workspace now 1546 tests (net +10), zero warnings, zero
regressions.

### Chunk 16: root-causing and fixing the chunk 15 FBIP reuse-in-place bug

Closed the "Open questions" entry chunk 15 flagged rather than chased.
Root cause and fix are described in full in the matching (now RESOLVED)
"Open questions" entry above — in short: `mark_reuse` (`plum-ir/src/
fbip.rs`) rewrote any bare-variable base into a `*Reuse` reuse-in-place
candidate with no check that `insert_refcount_ops` had actually
protected that name with `Inc`/`Dec`, which it never does for function
PARAMETERS (no type checker in this IR to prove one is heap-shaped).
Two ALIASED uses of the same unprotected parameter (receiver of one
`.concat()` AND passed again into a nested call that's itself another
`.concat()`'s argument) could each see runtime refcount 1 and both
destructively reuse the SAME cell. Fixed by threading the identical
`known_heap` set through `mark_reuse` too, gating every `*Reuse`
rewrite (not just `StrConcat`'s — `ArrayPush`/`Pop`/`Set`/`Remove`,
`StrTrim`/`StrToUpper`/`StrToLower`/`StrReplace`, `Match`'s
`CtorReuse`, all shared the identical unguarded check) on the base name
actually being tracked.

`String.repeat` reverted to the natural `s.concat(String.repeat(s, n -
1))` (the chunk 15 workaround ordering is no longer needed, though
still equally valid Plum). Both orderings now have dedicated regression
tests at every layer: `plum-ir/src/fbip.rs`'s own unit tests (plus new
ones proving a name ABSENT from `known_heap` is correctly refused, and
a full-pipeline `optimize()` test for the untracked-parameter shape),
`plumc/src/lib.rs`'s interpreter test, and `codegen_cli.rs`'s native
test — all asserting the CORRECT answer for the once-unsafe shape
directly. `plum-interp/src/lib.rs`'s existing `tuple_reuse_in_place_
fires_via_fbip` test needed updating too: it had been injecting its
scrutinee straight into `interp.env`, bypassing a real `let` — exactly
the "unprotected free variable" shape this fix now (correctly) refuses
to reuse — fixed by routing it through a real `let p = (1, 2); match p
{ ... }` so `insert_refcount_ops` tracks `p` for real, matching how the
mechanism is actually meant to be exercised.

Verified end to end: full `cargo test --workspace` (1551 tests, zero
warnings, zero regressions), plus a real throwaway project running the
once-corrupting shape directly through both `plum run` and a compiled
`plum build` binary, confirming both backends now agree on the correct
answer.

### Chunk 17: real example projects under `examples/`, and three more real bugs found (one fixed, two flagged)

User-directed: "add some more example files to showcase the language."
Scoped via `AskUserQuestion` (real, runnable projects verified through
both backends, covering everything: ADTs/matching, Option/Result,
JSON+files, concurrency, generics+`Type.func`, `Ref[T]`) rather than
guessed. Six new `examples/<theme>/main.plum` projects, each with a
real `main` and heavy explanatory comments, added alongside the
pre-existing `examples/overview.plum` (an older, explicitly-labeled
syntax SKETCH, not a runnable project — left untouched).

Writing real, from-scratch programs (rather than more stdlib additions
in the compiler's own established idiom) surfaced THREE more real,
previously-latent bugs — exactly the value of this kind of exercise:

1. **A native-codegen CRASH for `Array[Bool]`/`Array[Unit]` `.to_
   string()` — FIXED.** `emit_array_to_string_fns` (`plum-codegen/src/
   lib.rs`) built a counted loop whose back-edge `phi` nodes hardcoded
   `%render_elem` as their predecessor block, but its own per-element
   helper (`render_word_as_string`) branches into EXTRA blocks for
   `Bool`/`Unit` elements specifically (to pick `"true"`/`"false"`
   text) — so the code emitted right after that call actually landed in
   whatever merge block it opened, not literally `%render_elem`, and
   `clang` correctly rejected the stale hardcoded predecessor
   (`"PHI node entries do not match predecessors!"`). Since
   monomorphization emits this function eagerly for every array type
   reachable in the program (whether or not its `.to_string()` is ever
   actually CALLED), this broke `plum build` outright for any program
   with an `Array[Bool]`/`Array[Unit]` anywhere — a fairly easy shape
   to hit by accident (e.g. `xs.map(f).map(println)`, whose outer
   `.map()` produces `Array[Unit]`), first hit while writing the
   `adts_and_matching` example. FIXED: `render_word_as_string` now
   takes an in/out `current_block: &mut String` cursor it updates
   whenever it opens new blocks itself, and the array-loop's phi
   predecessors now read that real final-block label instead of the
   stale hardcoded one. `emit_struct_to_string`, this helper's other
   caller, never needed the fix — it has no fixed block name anywhere
   else in its own output to go stale against. New regression tests in
   `codegen_cli.rs` pin both the direct shape (`[true, false, true].
   to_string()`) and the `.map().map()`-chain shape that first found it.
2. **`Unit.to_string()` renders as `"false"` in native codegen — NOT
   fixed, flagged.** `CgType::Bool`/`CgType::Unit` are merged into one
   shared render arm everywhere in `plum-codegen` — happens to "work"
   for `Bool`, is simply wrong for `Unit` (always renders `"false"`,
   since `Unit`'s runtime word is always `0`). The interpreter instead
   correctly REJECTS `Unit.to_string()` outright — a real behavioral-
   parity gap between backends. Deferred: needs an actual design
   decision (reject, like the interpreter, or render some fixed literal
   like `"()"`), not a one-line patch. Routed around in the new example
   files by never printing a bare `Unit`/`Array[Unit]` directly.
3. **User source identifiers can collide with codegen-reserved LLVM
   names — NOT fixed, flagged.** A closure parameter literally named
   `entry` (`|entry: PriceEntry| entry.name == name` — an entirely
   ordinary name) made `plum build` fail outright: `emit_closure_body_
   fn` uses the RAW Plum source name directly as the LLVM register name
   with zero escaping, and EVERY function's first block is
   unconditionally labeled the bare string `entry` too, so the two
   collide. The closure env pointer (`%env`) is hardcoded the exact
   same unescaped way, so a parameter/capture named `env` would hit the
   identical class of bug. Deferred: a real fix needs a systematic
   escaping/mangling scheme for every user-sourced LLVM identifier
   (parameters, `let` locals, closure captures), not a one-line patch —
   its own scoped session. Routed around in the new example files by
   avoiding `entry`/`env` as identifiers.
4. **A discarded `spawn` statement's result can poison a LATER,
   unrelated `spawn` — NOT fixed, flagged.** A bare `spawn { ... };`
   statement (result discarded, ordinary "fire and forget") left a
   `Task` value bound under `lower_block`'s synthetic `"_"` name for
   the rest of that block; a LATER, completely unrelated `spawn`/`.
   join()` pair in the same function then failed with `"cannot send a
   task handle across a task boundary"`, since `Expr::Spawn` captures
   its WHOLE current environment (not just the names its body actually
   references) and swept the stale `"_"`-bound task into its capture
   set. Same underlying root cause as bug 3's sibling entry in "Open
   questions": whole-scope capture instead of free-variable-analyzed
   capture. Deferred for the same reason. Routed around in the new
   `concurrency` example by wrapping every `spawn`/`.join()` pair
   (including the discarded one) in its own nested `{ }` block, so any
   Task-typed binding goes out of scope before the next `spawn` runs. A
   dedicated `plum-interp` regression test
   (`a_discarded_fire_and_forget_spawn_does_not_poison_a_later_
   unrelated_spawn`) pins that the workaround actually works.

Also surfaced (not a bug): `Ref[T]` has zero native-codegen
representation, already fully decided/documented in "Mutability and
cycles" above — just never mentioned in the README's own backend-parity
list before this chunk, now fixed there too. General tuples similarly
have no native-codegen support (already documented under `Array.zip`'s
bullet) — the `option_result` example uses a small named struct
instead, matching this codebase's own established `Zipped { first,
second }` precedent for the same gap.

Verified end to end: every one of the six new examples run through
both `plum run` and `plum build` (except `shared_mutability`, `Ref[T]`
being interpreter-only), full `cargo test --workspace` clean before and
after. README gained a new "Examples" section pointing at all six, plus
the `Ref[T]` native-codegen-scope bullet.

### Chunk 18: fixing chunk 17's three flagged (not fixed) bugs

User asked to fix everything chunk 17 flagged but didn't fix. All three
are now RESOLVED — full root-cause/fix detail is in the matching (now
RESOLVED) "Open questions" entries above; short version here:

1. **`Unit.to_string()`** — asked the user directly first (a real
   design fork: reject like the interpreter used to, or render some
   fixed literal), since this genuinely wasn't an obvious bug fix.
   Chose to render the literal `"Unit"` in both backends, everywhere.
   `plum-codegen` gained a real `CgType::Unit` arm (previously shared
   `Bool`'s, wrong for `Unit`) in both the top-level `Expr::ToString`
   codegen and `render_word_as_string` (the array/struct-field-nested
   helper — this was the ACTUAL source of the `"false"` mis-render);
   the interpreter's `render_value` gained a matching `Value::Unit`
   arm.
2. **User identifiers colliding with reserved LLVM names** — surveyed
   every `CgType::Bool | CgType::Unit`-merged match arm across `plum-
   codegen` first (there are ~18) to confirm which were genuinely
   relevant (only the stringification ones) versus legitimately
   identical at the representation level (word conversion, equality,
   thread-boundary checks — all correctly left merged). Then found the
   ACTUAL raw-name-leak sites (only two: `emit_closure_body_fn`'s and
   `codegen_function`'s own param declarations — ordinary `let` locals
   and closure captures both already mapped to a compiler-generated
   register, never a raw name). Fixed via one new helper, `codegen::
   param_reg`, `.`-prefixing every user parameter's register — a
   character no Plum identifier can ever contain, making the collision
   class structurally impossible rather than avoided word-by-word.
   Required updating 17 existing `plum-codegen` unit tests that
   asserted exact (now-stale) `%name`-shaped IR text — an expected,
   mechanical consequence of a representation change, not a sign
   anything was wrong with the tests themselves.
3. **A discarded `spawn` statement poisoning a later, unrelated
   `spawn`** — root cause was `Expr::Spawn`'s interpreter handling
   capturing its WHOLE current environment rather than `block`'s actual
   free variables. Fixed by porting `plum-codegen`'s own `free_vars`/
   `free_vars_scoped` (already used there for closure-capture analysis)
   into `plum-interp`, duplicated across the crate boundary rather than
   newly shared — this codebase's own established precedent for small,
   independently-verifiable logic (e.g. `plumc::assoc_fns`'s pattern-
   binding helpers) — and using it to filter the capture set. This one
   fix closes BOTH manifestations chunk 17 hit: the discarded-statement
   case AND the "multiple live Task-typed locals" case (`examples/
   concurrency`'s original array-of-joined-tasks attempt, which needed
   working around with per-task nested blocks) — both were the same
   root cause, just different triggers.

The `examples/concurrency` file was simplified back to its natural,
un-worked-around form (no more per-spawn nested blocks) to prove the
fix directly, not just leave the workaround in place now that it's
unnecessary. Verified end to end: the ORIGINAL corrupting/crashing
repro for all three bugs re-run and confirmed fixed on both backends
(interpreter AND `plum build`, where applicable), full `cargo test
--workspace` clean throughout (net new regression tests: `plum-interp`
gained 4, `plum-codegen`/`plumc` gained several more across the Unit-
rendering and identifier-collision fixes), zero warnings, zero
regressions.

## Toolchain and diagnostics

After JSON, the user redirected from stdlib growth toward user-facing
polish: a README (added — practical usage docs, separate from this
design log), renaming the CLI binary from `plumc` to `plum` (a one-line
`[[bin]]` name change; the crate/package itself stays `plumc`), and —
the largest piece — human-readable compile errors.

### Human-readable errors: `CompileError{span, message}` — Decided, v1 implemented

Every error in this compiler used to print a `Span`'s raw `Debug`
output baked directly into the message text (`format!("... at {span:?}")`,
e.g. `type error: field access \`.radius\` at Span { start: 15677, end:
15685 } requires...`) — actively hostile to read, and the single worst
usability problem surfaced while writing the README's own code samples.
Asked how invasive to fix it, the user chose "restructure the error
types properly" over a regex-based post-processing hack on the existing
Debug text.

**Scope, confirmed by direct research before touching anything**: only
the FRONT END carries `Span` at all — the lexer/parser, `plum-types`
inference, and `plum-ir` lowering/move-checking. `ir::Expr` (the
lowered IR) is *deliberately* span-free by its own pre-existing doc
comment — `plum-codegen` and `plum-interp`'s runtime therefore have
**zero** span info today, and getting either one any would need a real
IR redesign. Those errors — and most of `plum-ir::monomorphize`'s —
stay message-only, exactly as before this work; an honest, documented
scope boundary, not a silent gap. No test anywhere pinned exact error
text (`assert_eq!` against an `Err(...)` value), which is what made the
"zero existing test edits" design below achievable at all.

**`CompileError { span: Option<Span>, message: String }`**, added to
`plum-syntax` (the lowest crate that already owns `Span`) — `Display`
prints just `self.message` (no span embedded), and a blanket `impl
From<String> for CompileError` (spanless) is the load-bearing piece:
it lets ANY function whose return type changes to `Result<_,
CompileError>` keep `?`-propagating an untouched `Result<_, String>`
call with zero edits at that call site (Rust's `?` invokes `From`
automatically) — so only the ~170 sites that actually CONSTRUCT a
span-bearing error (`Err(format!("... at {span:?}"))` and friends)
needed touching, not every function in the call chain. Also added:
`CompileError::context(prefix)` (prefixes a message while preserving
whatever span the error already had — for a caller adding context to
an inner error without discarding its location), `.or_span(span)`
(backfills a fallback span onto an otherwise-spanless error — used at
call sites like `infer_binary`/`infer_unary`/`infer_if`/`infer_pipe`,
whose own inner `unify()` calls have no span of their own at all, but
whose CALLER has the enclosing expression's span as a reasonable
fallback location), and `.contains(&str)` (a passthrough to `self.
message.contains(..)`, letting the many pre-existing `err.contains(
"...")` test assertions across the workspace — written back when every
error was a plain `String` — keep working against a `CompileError`
directly with no call-site changes).

**Two-tier public API — the actual "zero test-file edits" mechanism**:
every function's INTERNAL signature (parser, `infer.rs`'s ~25 span-
touching functions, `context.rs`, `lower.rs`'s ~23, `movecheck.rs`,
`modules.rs`'s `Resolver`) changed to `Result<_, CompileError>`, but
every EXISTING PUBLIC `plumc` function (`typecheck_and_run`, `compile_
and_run`, `resolve_modules`, `resolve_project`, `compile_program_to_
ir`, ...) kept its exact `Result<_, String>` signature, by flattening
`CompileError` via `.map_err(|e| e.to_string())` at its own return —
`CompileError::to_string()` returns exactly `self.message`, the same
text these ~1200 pre-existing tests (many with `.expect_err()`/`.
contains(...)` checks) already expected. New `_diag`-suffixed sibling
functions (`resolve_project_diag`, `typecheck_and_run_project_diag`,
`compile_program_to_ir_diag`, plus `pub(crate)` internal ones like
`resolve_modules_diag`/`run_resolved_program_diag`) expose the real
`CompileError` all the way up to `main.rs`, used ONLY by the CLI. This
mirrors this codebase's own pre-existing `typecheck_and_run` vs. `run_
resolved_program` / `compile_and_run` vs. `compile_to_ir` split-for-
reuse pattern, just one layer further. Verified concretely: after
converting the ENTIRE front end (`plum-syntax`, `plum-types`, `plum-
ir`), the whole workspace rebuilt and re-tested with the EXACT SAME
pass counts as before, at every single intermediate step — the
downstream crates didn't even need recompiling most of the time, since
every existing `.map_err(|e| format!("...: {e}"))`-style wrap already
went through `Display` (`{e}`), which works identically regardless of
whether `e` is a `String` or a `CompileError`.

**`ModuleSources`** (new, `plumc::diagnostics`) rebuilds the SAME
cumulative-base-offset scheme `resolve_modules_diag` already used
internally (per-module `Span` ranges, starting past `PRELUDE_TOTAL_LEN`
— see chunk 8's own `Span`-collision writeup for why that scheme exists
at all) independently, purely from the `&[(module_path, source)]` list
project.rs's directory walk produces, to translate a `CompileError`'s
`span.start` back into `(module, 1-based line, 1-based column)` and
render `error: {msg}\n  --> {module}:{line}:{col}\n   |\n {line} |
{source line}\n   | {caret}`. Column counted in CHARACTERS not bytes
(matters for non-ASCII source). A span whose end lands on a different
LINE than its start (rare — an unterminated construct) gets a single-
character caret at just the start position rather than a multi-line
snippet — a deliberate v1 simplification. A span landing inside the
PRELUDE itself, or otherwise unlocatable, falls back to message-only
rendering rather than panicking. **Scope note, deliberate**: locations
report by MODULE PATH (`"shapes"`, `"<root>"`), not exact source FILE
— `resolve_modules`'s public `&[(&str, &str)]` shape is used directly,
with hand-written module-name strings, by many pre-existing tests, so
threading real per-file paths through it would mean changing that
widely-used signature. In the common case (one file per module/
directory — true of every example in this codebase), this is already
exact file-level precision.

`main.rs` wires both CLI paths (`plum <project>` and `plum build`) to
their `_diag` counterparts, building a `ModuleSources` via a shared
`module_sources(root)` helper (a second, independent directory walk —
an accepted, deliberate inefficiency for a CLI invoked once per run,
not a hot path) and rendering any `Err` through it instead of printing
the raw message.

**Direct, concrete payoff** — a real error today:
```
error: type error: operator: type mismatch: expected Str, found Int
  --> <root>:3:15
  |
3 |     let bad = "hello" + 1;
  |               ^^^^^^^^^^^
```
Verified end to end with a real throwaway multi-file project containing
a parse error, a type error in the ROOT module, and a type error in a
NON-root module file (proving file/module attribution, not just line:
col within one file) — run through BOTH `plum <project>` and `plum
build <project>`, identical rendering through both. A genuinely
spanless error (a real tuple-equality codegen gap) confirmed to still
print cleanly, message-only, no crash. Workspace now 1459 tests (net
+7 — 4 new `diagnostics.rs` tests, 3 new `CompileError` tests in `plum-
syntax`), clean build, zero warnings, and — the headline result — ZERO
edits to any pre-existing test across the whole ~1200-test suite.

**Not done in this pass** (flagged as quick, independent follow-ups,
not started): `plum run <project>` as an explicit alias alongside
`plum build`, and `plum new <name>` project scaffolding.

### `plum run` and `plum new` — Decided, v1 implemented

The two quick follow-ups flagged above. `plum run <project-dir>`
(`main.rs`'s `run_interpreter`) is the interpreter path, factored out
so both it and the pre-existing bare `plum <project-dir>` shorthand
(kept working for backward compatibility — it predates `run` existing
as an explicit subcommand) funnel through identical code; symmetric
with `plum build <project-dir>`. `plum new <name>` (`run_new`/`new_
project`) scaffolds a directory containing one `main.plum` with a
hello-world `main`, so `plum run <name>` works immediately with zero
setup — refuses to overwrite an existing path (checked via `Path::
exists`) rather than silently clobbering it. Both are ordinary `String`
errors (usage/`already exists`/IO failures) — no `Span` involved, so
neither needed the `CompileError`/`ModuleSources` machinery the rest of
this section covers. Verified: `plum new myapp` → `plum run myapp` →
`plum build myapp -o myapp/out` → running the binary, all produce the
exact output the README documents (each command was actually run
before being written down, matching the whole README's own "verify
before documenting" discipline). Workspace now 1461 tests (net +2 —
`new_project_scaffolds_a_runnable_hello_world`, which confirms the
scaffolded source is real, valid Plum by actually running it, not just
checking a file got written; `new_project_refuses_to_overwrite_an_
existing_path`), zero warnings.

### Testing framework: `panic_raw`, `assert`/`assert_eq`/`assert_ne`, `plum test` — Decided, v1 implemented

The last item from the docs/testing-framework/toolchain pivot. Scoped
via `AskUserQuestion` before implementation, matching this project's
own established discipline for anything with real design forks: (1)
test discovery is a naming convention (`test_*`), not a directory
convention or an annotation; (2) real `assert`/`assert_eq` builtins,
not just `Bool`-returning test functions — worth the extra compiler
work for genuinely useful "expected X, found Y" failure output; (3)
native-codegen tests run as one subprocess per test.

**The load-bearing constraint, confirmed by direct research before
designing anything**: a native-compiled program's existing runtime
checks (array-index-OOB etc.) call `@plum_abort` (`printf` the
message, `exit(1)`, `unreachable`) — a HARD process termination, no
unwinding, no recoverable `Result`. The interpreter, by contrast,
already propagates every runtime error as an ordinary `Result<Value,
String>` all the way up through `Interpreter::call` (division-by-zero/
array-OOB are plain `return Err(...)` inside `eval`'s own `match`).
This single fact is why native tests need process-level isolation to
get a real per-test pass/fail at all, while the interpreter gets it for
free by just calling every test in one loaded process.

**`panic_raw(msg: String): Unit`** — a new low-level core-language
primitive, added the SAME way as chunk 8's `read_file_raw`/
`write_file_raw`: a bare-`Ident`-named call shape recognized in `lower.
rs`/`infer.rs` (not a new grammar/AST node), evaluating to a new `ir::
Expr::PanicRaw { message }`. Simpler than the file-I/O primitives:
`PanicRaw` never constructs a value on any live path, so it needs no
monomorphization/struct-tag-discovery hookup at all — just a
mechanical arm added at `fbip.rs`'s ~5 existing `ReadFileRaw`-shaped
spots, plus:
- **Interpreter**: evaluates `message`, `return Err(msg)` DIRECTLY —
  the exact same propagation shape an ordinary runtime error already
  uses, so `Interpreter::call`'s own `Result` needs zero special
  handling anywhere it's consumed.
- **Codegen**: a genuine, deliberate departure from `emit_runtime_
  check`'s own precedent, found while designing this (not by trial and
  error): `emit_runtime_check`'s fail branch ends in `unreachable`,
  which is fine for a plain conditional check with a separate "ok"
  continuation reached only via a branch instruction — but `PanicRaw`
  is an ORDINARY expression, reachable as (for instance) an `if`/
  `else` branch's own tail value, which needs to `phi`-merge with the
  OTHER branch. Ending in `unreachable` would make that impossible (no
  way to `br label %merge` from a block that already terminated). Since
  `@plum_abort` isn't marked `noreturn`, nothing requires treating this
  as truly diverging at the LLVM level at all: `codegen_panic_raw`
  just calls `@plum_abort` as an ordinary instruction and returns a
  placeholder `Unit` value (`"0"`, exactly matching `Expr::Unit`'s own
  codegen) — dead code in practice, but ordinary, well-formed IR
  requiring ZERO special-casing in `If`/`Match`'s own merge machinery.

**`assert`/`assert_eq`/`assert_ne`** — pure Plum prelude source
(`STDLIB_ASSERT_SRC`), built on `panic_raw`, needing no further IR/
codegen work at all (mirrors chunk 9's JSON pattern: one new low-level
primitive, everything else ordinary Plum). `assert_eq`/`assert_ne` are
bounded `[T: Eq + Show]` — both bounds, and the `[T: A + B]` multi-
bound syntax itself, were ALREADY real and enforced before this
(`plum_types::infer::satisfies_bound`, `parser.rs`'s `parse_generic_
param`) — no new type-system work, only using what already existed.

**A real, previously-latent module-resolver bug, found by the
prelude's own new source** (not a hypothetical, an actual test
failure): `assert_eq[T: Eq + Show] (a: T) (b: T)`'s body calls `a.to_
string()`. An EXISTING test (`two_modules_can_declare_a_struct_with_
the_same_name_without_colliding`) declares real modules named `a` and
`b`. `modules.rs`'s `Resolver::resolve_segments`, for a MULTI-segment
path (`a.to_string`), checked `used_modules`/`all_declared` but never
`locals` — so `a` (an utterly ordinary function PARAMETER name,
shadowing nothing) got misinterpreted as a qualified reference into
module `a` purely because the enclosing test file also happened to
`use a;` for something unrelated. Root-caused, not worked around: the
multi-segment branch now checks `locals.contains(&segments[0])` first,
exactly mirroring the single-segment case's own existing local-shadows-
module check just above it.

**A second real bug, found during manual end-to-end verification**
(not by the test suite — this specific interaction had no test until
after it was found): `run_tests_native` initially compiled the shared
IR body once with an arbitrary placeholder `entry_fn` (assumed
irrelevant to `body_ir`'s own contents, since `plum_ir::monomorphize::
plan`'s own doc comment confirms it re-lowers EVERY top-level function
regardless of entry point — true, but incomplete reasoning). A real
throwaway project with its own ordinary `let main (): Unit = ...`
alongside its tests hit `clang failed to compile the generated IR:
invalid redefinition of function 'main'` for EVERY test: `compile_
program_to_ir_diag`'s existing Plum-`main`-vs-native-`main` collision
rename (renaming a Plum-level `main` to `@__plum_entry_main` so it
never clashes with `emit_main`'s own hand-written native `@main()`)
only fires when the entry_fn IT was called with resolves to literally
`"main"` — the placeholder name never did, so the project's real `main`
stayed compiled into the shared `body_ir` under its own unrenamed
`@main`, colliding with every per-test `emit_main` wrapper appended
afterward. Fixed by passing `"main"` itself as `entry_fn` (always
safe, even for a project with no `main` at all — entry resolution
doesn't require the name to exist, only errors on an AMBIGUOUS generic
entry), reusing the EXISTING collision-avoidance mechanism `plum
build` already established rather than inventing a new one.

**`plumc::testing`** (`discover_tests`/`run_tests_interpreted`/`run_
tests_native`) + **`plum test [--native] <project-dir>`**
(`main.rs`): discovery scans the resolved, fully-qualified `ast::
Program` for top-level `let`s whose LAST dot-segment starts with
`test_` — no `pub` requirement, since the CLI calls a test by its
qualified name directly, the same way `main` itself already is
(confirmed via `modules.rs`'s own existing `root_module_declarations_
are_visible_from_any_module_without_use` test). `run_tests_
interpreted` type-checks/lowers/optimizes ONCE (the same front half
`run_resolved_program_diag` runs), then calls each test in the same
loaded interpreter. `run_tests_native` compiles the shared IR body
once, then for each test appends its own `emit_main` wrapper (identical
to what `plum build` does for `"main"` specifically, just looped) and
runs the result as its own subprocess, cleaning up after each. A
resolve-time project error (a real syntax/type error, not a test
failure) renders through `ModuleSources` exactly like every other
command.

**Direct, concrete payoff**: a real throwaway project (a non-root
`shapes` module with two tests, a root module with four, deliberately
mixing passing/failing/struct-`assert_eq` cases) run through both `plum
test` and `plum test --native` — identical 3-passed/3-failed verdicts,
identical failure messages (including the struct case's `.to_string()`
rendering), correct qualified-name attribution for the non-root-module
tests (`shapes.test_area_is_wrong_on_purpose`), and a real project-
level compile error rendering through `ModuleSources` on both paths
identically to every other command. Workspace now 1489 tests (net +28
across `plum-ir`/`plum-types`/`plum-interp`/`plum-codegen`/`plumc`),
clean build, zero warnings.

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
below; disconnect detection is explicitly NOT part of this. More than
one distinct `channel[T]()` element type per program was also
unsupported at the time; that was lifted on 2026-08-16 — see "Channels
of more than one element type"), and — as of a further follow-on chunk —
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
disconnect detection on channels — and a generic instantiated at any
still-unsupported type (e.g. `Box[Array[Str]]` once `.split()` is
needed).

Two entries were removed from this list on 2026-08-17. Value-position
`Assign` is supported (see "Value-position `Assign`" below). And an
`Assign` inside a closure writing back into an enclosing loop's carried
variable was checked directly rather than trusted: it compiles, and agrees
with the interpreter. It also compiled BEFORE that day's work, so the
claim was simply stale.

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
element type was supported per program** — a second, differently-typed
`channel[..]` call anywhere in the same program was a loud, clear
compile-time `Err`, never a silent miscompile.

**Lifted on 2026-08-16.** The cross-crate follow-up work this paragraph
predicted was done, in exactly the shape predicted: a span-keyed side
channel through `Infer`, feeding type-specialized tuple tags. A program
may now use as many distinct channel element types as it likes. See
"Channels of more than one element type" for what it took, and in
particular for why `ir::Expr::Channel` had to start carrying its tuple's
tag.

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

## Editor tooling (LSP)

`plum lsp` (`crates/plumc/src/lsp/`) — an LSP server served straight
out of the `plum` binary itself (the `gopls`-for-Go shape, not a
separate `plum-lsp` binary/crate). **Scope**: diagnostics (parse/
module-resolution/type errors) on open/change/save/close, hover
(resolved type), go-to-definition (variables/params/`let`s, function/
global calls, struct/enum names, `.field` access, enum variant
references — see "Hover and go-to-definition" below), and completion
(keywords + top-level names generally, struct fields after a `.` —
see "Completion" below). Full-document sync only, and whole-PROJECT
semantics:
every edit re-walks and re-typechecks the ENTIRE workspace root (with
any open buffers' unsaved content overlaid), not just the changed
file. `check_modules_diag`, like the rest of this codebase's front
end, reports at most one `CompileError` per check — so does the LSP;
a project with more than one error only shows a diagnostic for the
first until it's fixed, AND gets no hover/go-to-definition at all
until it does (both need a successful `infer_program` to have
anything to answer from). Both of these (whole-project re-walk per
edit, one error at a time) are honest, documented v1 costs, acceptable
while every project this compiler targets stays small — not yet
revisited.

### Recheck staleness/debounce fix — Decided (2026-08-12)

Asked "how can we improve the LSP" as an open discussion, given the
v1 scope's own honest gaps (no hover/go-to-def/completion, single-
error-at-a-time, whole-project-per-keystroke). Presented four ranked
options — this fix, hover, go-to-definition, and multi-error
diagnostics (the last two blocked on more front-end plumbing than the
first two, especially multi-error, which needs the parser/type
checker's own single-`CompileError`-result design to change, not just
the LSP layer) — and this one was chosen as the contained, immediate
first step.

**The gap**: every LSP event handler (`did_open`/`did_change`/`did_
save`/`did_close`) called `Backend::recheck` directly, with zero
debounce, and `recheck` itself had no notion of "am I still the most
recent request." A whole-project re-walk + re-typecheck is not
instant; a realistic edit burst could launch several overlapping
`recheck`s that finish in a DIFFERENT order than they started in, and
an OLDER one finishing LAST would publish its now-stale diagnostic
over a NEWER, already-correct one — a real, live race, not
hypothetical.

**The fix**: a `Generation` type (`Backend::generation`) — a monotonic
`AtomicU64` tag wrapped in a tiny `bump()`/`is_current()` API, pulled
out as its own standalone type specifically so its actual correctness
property is unit-testable directly (three new tests, including one
spawning 50 real OS threads bumping concurrently and confirming
exactly the highest tag stays current) without needing a fake `tower_
lsp::Client` (no public test-double constructor exists for one — the
whole `Backend` struct is otherwise untestable in isolation). Every
event handler now calls a new `recheck_debounced` instead of `recheck`
directly: it bumps the generation, waits a short `DEBOUNCE` (150ms),
and only then calls `recheck` — abandoning silently if a later event
already bumped the generation again during the wait, so a realistic
typing burst collapses into one recheck instead of one per keystroke.
`recheck` itself re-checks the generation immediately before every
publish/state-mutating step (not just once at the top), since the
walk+typecheck itself — not just the debounce wait — is exactly where
a newer recheck can race ahead and finish first.

### Hover and go-to-definition — Done (2026-08-12), same session

Built the two next-ranked options from the same "how can we improve
the LSP" discussion, together, since go-to-definition's plumbing
(declaration-site spans) mostly subsumes what hover needs too (span-
indexed lookups against a live `Infer`). Scoped explicitly with Brad
first: hover shows just the resolved type (not type + doc comment);
go-to-definition covers variables/params/`let`s, function/global
calls, struct/enum names, `.field` access, AND enum variant
references (not just the narrower variables-and-calls baseline).

**The core mechanism, in `plum-types::infer::Infer`** — two new,
deliberately SEPARATE span-keyed side-channels, following the exact
precedent `field_owners`/`empty_array_elem_types` already established
(a narrow `HashMap<Span, T>`, not a full typed IR):

- **`node_types: HashMap<Span, (Type, Option<String>)>`** — every
  expression node's (possibly still-unresolved) type, recorded by
  wrapping `infer_expr` itself: the existing giant match became a
  private `infer_expr_inner`, and the new public `infer_expr` just
  calls it and records the result before returning. Since EVERY
  recursive call anywhere in the module already goes through `self.
  infer_expr(..)`, this one wrapper covers every node in the program
  with zero changes to any of the match's ~80 arms. `resolve_node_
  types()` applies the program's final substitution to every entry
  once inference completes — mirrors `resolve_empty_array_elem_types`
  exactly, except deliberately LENIENT (an entry still unresolved even
  after the closure-component template fallback is silently dropped,
  not a hard `Err` — hover is best-effort UI, and one un-renderable
  node must never make an otherwise-successful `infer_program` look
  like it failed).
- **`definitions: HashMap<Span, Span>`** — a reference's own span ->
  where the thing it resolved to was DECLARED. Needed `TypeEnv` (the
  local scope chain) to grow an `Option<Span>` per binding — but as a
  NEW `extend_spanned`/`extend_scheme_spanned` pair alongside the
  EXISTING `extend`/`extend_scheme` (which now just pass `None`),
  never a signature change to the originals. This mattered for real:
  `TypeEnv::extend` has ~100 call sites, the overwhelming majority in
  tests constructing an env directly with no real span to give — a
  breaking signature change would have touched every one of them for
  no benefit. Only the ~10 call sites that are the AUTHORITATIVE
  binding site for something a user would actually want to jump to
  (a function parameter, a `let`, a closure param, a `for`/`match`
  binding, a function/global/extern's own final signature
  registration) were switched to the spanned variant — every
  SPECULATIVE/internal one (a multi-phase pass's placeholder, a self-
  recursion pre-bind immediately superseded) stayed unspanned on
  purpose, since there's no single well-defined "jump here" site for
  those anyway. `TypeContext` (struct/enum declarations) got the same
  treatment: four new, entirely separate span maps (`struct_decl_
  spans`/`enum_decl_spans`/`struct_field_spans`/`variant_spans`)
  alongside the existing ones, so `struct_fields`'s own return type —
  asserted against directly by ~15 existing tests — never changed
  either.

**Wiring it up, in `plumc`**: `check.rs` gained `check_program(_modules)
_diag_with_infer`, a sibling of the existing `_diag` functions that
returns the `Infer` instance on success instead of discarding it —
`check_program_diag`/`check_modules_diag` themselves are now thin
wrappers around the new functions, one real implementation instead of
two that could drift. `diagnostics::ModuleSources` gained `to_global_
offset`, the exact inverse of its existing `locate_offset` (module
index + local byte offset -> the merged-module GLOBAL offset `Infer`'s
maps are keyed by). `lsp::position` gained `position_to_byte_offset`,
the inverse of `byte_offset_to_position` (LSP's UTF-16-code-unit
`Position` -> a local byte offset) — proven a true round-trip inverse
by a dedicated test iterating every byte offset in a real source
string. `Backend::overlaid_files` was factored out of `recheck` (which
now just calls it) since `hover`/`goto_definition` need the exact same
"current content of every file in the project" view; a new shared
`Backend::resolve_at` builds on it to turn a request's `(uri,
position)` into `(Infer, ModuleSources, global_offset, overlaid_
files)` in one place. A small `smallest_containing` helper picks the
narrowest span containing the cursor out of either side-channel (so
hovering `x` inside `x + 1` shows `x`'s own type, not the whole binary
expression's) — used for both hover and go-to-definition. `render_type`
renders a `Type` in Plum surface syntax (`Array[Int]`, `(Int) -> Bool`,
`Str` as `"String"`, matching the surface name rather than the
internal variant name) rather than leaking `{ty:?}`'s Rust `Debug`
noise into a hover popup the way every internal error message renders
one.

Verified for real, not just via the underlying helpers' own unit
tests: three new end-to-end tests drive `Backend::hover`/`Backend::
goto_definition` DIRECTLY (bypassing JSON-RPC serialization — `tower_
lsp::Client` has no public standalone constructor, so a real one is
captured out of `LspService::new`'s init closure via `Clone`, then a
fresh `Backend` built from it) against a real temp-directory project
on disk — proving the whole path (file walk, module merge, type-check,
span lookup, byte-offset conversion, LSP response construction) works
together, including a "cursor on whitespace, nothing resolves" case
returning `None` cleanly rather than erroring. 6 new tests in `plum-
types` (`resolve_node_types`/`definitions`, one per reference kind)
and 6 new position-conversion tests round out the coverage. Full
workspace suite: 0 failures.

### Completion — Done (2026-08-12), same session

Asked as a follow-up: "what about autocomplete?" Investigated before
committing to a design, and found a real structural wrinkle hover/
go-to-definition never hit: `plum-syntax`'s parser is strict, single-
pass, first-error-wins — confirmed directly (grepped for any recovery/
synchronize machinery: none exists). Hover/go-to-definition only ever
need the code the CURSOR is already sitting in to be complete, which
it normally is. Completion is different — the two moments it matters
most are exactly when the buffer ISN'T complete: right after typing
`.` (`parse_postfix_from` calls `self.expect_ident("a field name")?`
unconditionally — `p.` with nothing after is a hard parse error, not a
tolerated gap) and mid-typing a field/identifier name (parses fine
syntactically, but the wrong-so-far name is a real TYPE error, which
fails the WHOLE project's check under this front end's "one error
stops everything" design — the same boundary hover/go-to-definition
already live with, but completion runs into it far more, at exactly
its most valuable moment).

Presented the real trade-offs and got scope confirmed before building:
dot/member completion via "splice a placeholder + reparse" (always
reflects the CURRENT buffer, zero parser changes — the chosen approach
over a last-good-snapshot heuristic, which risks staleness, or
skipping dot completion for v1 entirely) and general completion via
keywords + last-successfully-checked top-level names (not additionally
locally-in-scope names, which would reopen the same incomplete-parse
problem a second time for comparatively less value).

**General completion**: `Backend::last_good_completions` — a CACHE
(`Mutex<Vec<CompletionItem>>`), refreshed on every successful `recheck`
from a fresh walk of the merged `Program`'s top-level items (`top_
level_completion_items`) — a zero-param `let` is a global, one WITH
params is a function, a `struct`/`enum` contributes its own name plus,
for an enum, every variant tag too, an `extern` block contributes each
declared function. Since `resolve_modules_diag` already injects the
whole prelude before this ever sees it, this naturally covers every
stdlib function/type too, not just the user's own project — with zero
extra plumbing. A cache, not computed fresh per request, specifically
because the moment completion is most useful is often the moment the
current buffer doesn't parse at all — stale-but-mostly-right beats
nothing while the user's mid-edit. Merged with a static `KEYWORDS` list
(every reserved word the lexer recognizes, including `fn`/`mod`, which
the grammar barely uses yet but are still real reserved tokens).

**Dot/member completion**: `Backend::dot_completion` detects a `.`
immediately before the cursor (scanning backward over identifier-
continue bytes to find the start of whatever partial name is being
typed, possibly zero-length), then SPLICES: replaces that partial name
with a fixed placeholder identifier (`__plum_lsp_completion__`) before
handing the modified buffer to the ordinary, otherwise-UNCHANGED parse
+typecheck pipeline. `base.__plum_lsp_completion__` parses exactly
like any other field access — no parser changes anywhere — but since
the placeholder never names a REAL field, `infer_program` still fails
overall (a genuine "no field named …" type error), and the existing
`check_modules_diag_with_infer` DISCARDS its whole `Infer` on any
failure (correctly so, for hover/go-to-definition/diagnostics, which
must never answer with something a broken program didn't actually
prove).

Fixed this with two small, real, and completion-specific changes: (1)
`Infer::field_owners`' recording moved earlier — as soon as `base` is
KNOWN to resolve to a real struct, BEFORE checking whether the (in
this case deliberately fake) field name exists at all. Zero behavior
change for `plum build`/`run`/`test` (a program with an invalid field
name fails to type-check either way, so `lower.rs` — `field_owners`'
only OTHER consumer — never runs on it regardless of exactly when this
line executes); a new regression test pins the ordering directly. (2)
A new `check_modules_diag_lenient_infer` — same inputs as `check_
modules_diag_with_infer`, but NEVER propagates `infer_program`'s own
failure, always handing back the `Infer` with whatever got recorded
before it gave up. Used ONLY by `dot_completion`, nowhere else — every
other handler keeps the strict, error-propagating version, since
answering with a WRONG type/location would be worse than answering
nothing, and only this one narrow probe has a reason to accept a
knowingly-incomplete result. Once the struct name is recovered (via
`field_owners`, keyed by the synthetic Field node's own span — computed
directly from the splice, not searched for, so unambiguous), `Infer::
ctx().struct_fields(..)` (a new `pub fn ctx()` getter, mirroring
`field_owners`/`definitions`) gives the completable field list.

Verified end-to-end: 4 new `Backend::completion` integration tests
(general completion offering both keywords and top-level names; a
BROKEN buffer still offering the last-good snapshot's names, driven
through a real `did_change`; dot completion on a bare trailing `.`;
dot completion mid-typing a partial field name — proving the splice
mechanism doesn't depend on the dot being freshly typed) plus 2 new
`plum-types`/`check.rs` regression tests pinning the `field_owners`
reordering and the lenient variant's own contract directly. Full
workspace suite: 0 failures.

### Multiple diagnostics — Done (2026-08-12), same session, narrower than first asked

Asked as a follow-up: "we also talked about surfacing more than one
error at a time?" Investigated before proposing anything, and found
"multiple diagnostics" actually splits into three genuinely different
problems, not one, each with a very different risk profile:

1. **Multiple PARSE errors across different FILES** — `resolve_
   modules_diag` parses each file independently in a loop, bailing at
   the first `?`. Each file's own parse shares NO state with any
   other's, so collecting all of them instead of stopping at the first
   is safe and mechanical.
2. **Multiple parse errors WITHIN one file** — not tractable without
   real parser error recovery, the same wall hit scoping completion:
   this parser is strict, single-pass, first-error-wins, with zero
   recovery/synchronize machinery anywhere. A structural rewrite, not a
   bugfix.
3. **Multiple TYPE errors across different top-level functions**
   (probably what "why do I only see one error" usually means in
   practice) — genuinely risky: `infer_program`'s Phase 2 threads ONE
   shared, accumulating substitution across every function body ON
   PURPOSE, so mutual recursion works (two functions calling each other
   need to see each other's real, resolved-so-far signature). Skipping
   a failed function's contribution and continuing risks a LATER
   function that genuinely depends on it producing a spurious,
   misleading secondary error that has nothing to do with what's
   actually wrong in it — the classic cascading-error problem, real
   design work on an already-intricate, previously-hard-won algorithm
   (its own surrounding comments document regressions found
   "empirically" while getting the ordering right the first time), not
   just an untried convenience.

Presented all three with their real trade-offs; Brad chose #1 only —
real value for a genuinely common case (more than one broken file in a
project), zero risk, leaving #2 and #3 explicitly out of scope rather
than attempted partially.

**Built**: `parse_every_module_diag` (`modules.rs`) — collects every
FILE's own parse error into a `Vec`, instead of `resolve_modules_diag`'s
existing first-error-wins `?`; a genuinely NEW, separate function, not
a behavior change to the existing one (`plum build`/`run`/`test`'s own
single-error UX is untouched — nobody asked for that to change, and
CLI tooling showing "fix this one, then rerun" is arguably fine as-is).
`Backend::recheck` now tries this FIRST: if any files fail to parse,
every one gets its own `Diagnostic`, published together; only once
EVERY file parses cleanly does it fall through to the existing single-
error `check_modules_diag` path for module-resolution/type errors,
unchanged. `Backend::last_diagnosed` grew from `Option<Url>` to
`HashSet<Url>` to track possibly-many currently-diagnosed files at
once, with the clear-on-fix logic generalized to a set difference
(files that WERE diagnosed but aren't in the new result get cleared;
everything in the new result gets (re-)published) rather than a single
before/after comparison.

Verified with 3 new pure unit tests on `parse_every_module_diag`
itself (clean, one broken file, two broken files with a clean one in
between — proving it doesn't stop at the first) plus a real end-to-end
`Backend` test: three files (two broken, one clean) opened through the
REAL `initialize`/`initialized` startup sequence, `last_diagnosed`
(a private field, inspected directly — same crate's own test module)
asserted to contain exactly the two broken files' URIs and NEITHER the
clean one, then one of the two fixed via a real `did_change` and
`last_diagnosed` re-checked to confirm exactly that one file's
diagnostic cleared while the other, still-broken one's stayed — proving
the set-difference clear logic, not just the initial collection. Full
workspace suite: 0 failures.

### Completion gap found and fixed: `Array.`/`String.`/`Type.func` namespace completion — Done (2026-08-12), same session

Reported directly: "I don't see autocompletion for `Array.`" — a real
gap in what "Completion" above shipped, not a setup problem. Root
cause: `dot_completion` only ever handles ordinary struct field access
(`p.x`), because it resolves `p`'s type through ordinary type
inference. `Array.map(...)` LOOKS identical at the AST level (`Field {
base: Ident("Array"), name: "map" }`), but `Array` is never a bound
VALUE at all — it's `Type.func` associated-function call syntax (see
`assoc_fns.rs`'s own doc comment: a call-site rewrite of `Type.func(..
.)` into an ordinary call against the top-level function literally
named `"Type.func"`), so inferring `Array` as an expression just fails
with "unbound variable," a completely different code path than the one
`field_owners` populates.

Fixed by adding a SECOND, separate completion mechanism —
`Backend::namespace_completion`, checked before `dot_completion` — that
doesn't need type inference OR a reparse at all: every `Type.func`
associated function is already an ordinary `LetDef` whose NAME
literally contains a `.`, sitting right there in the merged program's
own item list. `top_level_completion_items` now splits on that: a
dotted `LetDef` name is excluded from the flat general-completion list
(nobody types `"Array.reverse"` as one token) and instead keyed by its
type name into a new `Backend::last_good_namespace_completions` cache,
refreshed alongside the existing one. `namespace_completion` scans
backward from the cursor (reusing a newly-extracted `dot_trigger`
helper, shared with `dot_completion`) to find the base identifier
before the dot, then just looks it up in that cache — cheaper than the
struct-field splice path (no reparse needed) and correct even on a
buffer broken by the very typing that triggered completion, same
reasoning as general completion's own cache.

One more real gap found finishing this: `Array.map`/`Array.filter`/
`Array.fold` and `String.hash` are genuine COMPILER PRIMITIVES,
recognized directly by AST shape in `infer_expr`'s `Call` arm (`is_
array_builtin_call`/`is_string_builtin_call`) — there's no `LetDef` for
any of them anywhere, so the program-walk approach above could never
find them on its own (confirmed directly: a first test asserting
`Array.map`/`Array.filter` failed, listing everything ELSE `Array.`
has). Fixed with a small, explicitly-scoped `CORE_BUILTIN_ASSOCIATED_
FNS` constant (exactly the 4 real primitives, cross-referenced against
`infer.rs`'s own two helper functions' call sites) seeded into the
namespace map directly, alongside whatever the program walk finds.

Verified with 4 new end-to-end tests: `Array.` (a real parse error on
its own, same as `p.`) correctly answering from the cached snapshot
with real Array members including the hardcoded primitives; a user's
own `Point.add` showing up the same way; confirming dotted names never
leak into general completion's flat list either direction. Full
workspace suite: 0 failures.

### Completion cold-start bug: the cache started genuinely EMPTY — Done (2026-08-12), same session, found via live use

Still not seeing `Array.` complete after the fix above, reinstalling,
and restarting the LSP client (twice — the first "restart" turned out
to be a DIFFERENT Neovim session; a genuinely stale `plum lsp` process
from Aug 11, a child of a still-running `nvim` on another pty, was
found and killed before this bug surfaced). Verified the SERVER itself
directly — a small Python harness driving the real installed binary
over actual LSP JSON-RPC (framed `Content-Length` messages over
stdio), bypassing every Rust-level test harness entirely — and found a
real, separate bug: `Backend::new` started `last_good_completions`/
`last_good_namespace_completions` genuinely EMPTY, populated only by a
SUCCESSFUL `recheck`. A project opened DIRECTLY into a broken state
(exactly what a fresh `Array.` — a real parse error on its own — looks
like on the very first `didOpen`, with no prior edit history) never
had a chance to populate the cache at all — confirmed directly: the
same probe against a file that opened with valid content FIRST, then
transitioned to `Array.` via `didChange`, worked (24 real items,
including the hardcoded primitives); the identical file opened
DIRECTLY into the broken state returned only keywords.

**Fixed**: `Backend::new` now seeds both caches from the PRELUDE ALONE
(`top_level_completion_items(&with_prelude(Program { items: vec![] }))`)
instead of empty collections — the prelude is compiler-controlled and
unconditionally valid, entirely independent of whether the user's OWN
project code has ever type-checked, so this baseline (every stdlib
`Array.`/`String.`/etc. member, every prelude keyword-adjacent symbol)
is available from the very first request, before `initialize` even
finishes. A real project's first successful `recheck` still overwrites
this with the full result — a strict superset, nothing lost.

Verified three ways: a new Rust regression test (`Backend::new` +
`did_open` straight into broken `Array.` content, no prior success at
all); re-running the SAME real-JSON-RPC Python probe against the
rebuilt release binary in this exact cold-start shape (24 items,
`map`/`reverse` both present); and reinstalling `~/.cargo/bin/plum`
again. Full workspace suite: 0 failures.

**Process note**: this is the second real bug in this feature caught
only by testing the ACTUAL installed binary over real LSP JSON-RPC,
not by the (already-passing) Rust integration test suite — those tests
always seed state via `Backend::new()` then real event handlers in a
fresh process per test, which happens to never exercise "first-ever
request, nothing cached yet, opened directly into a broken file" quite
this starkly. Worth remembering for future LSP work: the Rust-level
harness proves the LOGIC is right; driving the real binary over real
stdio JSON-RPC is what catches gaps in when that logic actually runs.

## Open questions (not yet decided, flagged so we don't forget them)

- ~~KNOWN BUG — a discarded statement-expression result can linger in
  the interpreter's environment and get captured by a LATER, unrelated
  `spawn`~~ **RESOLVED in chunk 18.** Root cause: the interpreter's
  `Expr::Spawn` handling captured its WHOLE current environment
  (`self.env.clone()`), not just the names its own body actually
  references — so a discarded `spawn { ... };` statement's `Task`,
  still bound under `lower_block`'s synthetic `"_"` name for the rest
  of the block, could get swept into a LATER, unrelated spawn's capture
  set and rejected (`Task` is never portable). Fixed by adding a real
  free-variable walker (`plum-interp::free_vars`, ported from and kept
  in sync with `plum-codegen`'s own identically-named/identically-
  shaped function, duplicated across the crate boundary rather than
  newly shared — matching this codebase's own established precedent)
  and using it to filter `Expr::Spawn`'s capture set down to only the
  names `block` genuinely references. Also fixes the sibling shape from
  the entry below (multiple live `Task`-typed locals in scope — each
  spawn now only captures what it actually needs, so an earlier
  unrelated task in scope is no longer swept in either). Regression
  tests in `plum-interp` pin BOTH the original unsafe shape (a bare
  discarded `spawn` before a later one, no nested-block workaround
  needed anymore) and that a spawn still correctly captures a local its
  body genuinely depends on.
- ~~KNOWN BUG — user source identifiers can collide with codegen-
  reserved LLVM names in native codegen, with no escaping/mangling at
  all~~ **RESOLVED in chunk 18.** Root cause: `emit_closure_body_fn`
  (closures) and `codegen_function` (top-level functions) both used a
  parameter's RAW Plum source name directly as its LLVM register
  (`format!("%{name}")`), with zero escaping — so a parameter literally
  named `entry` collided with the bare `entry` block label every
  function's first block gets, and a parameter named `env` would
  collide with the closure environment pointer's own hardcoded
  register the same way. Fixed via a single new helper, `codegen::
  param_reg`, which `.`-prefixes every user parameter's register
  (`%.name`) — a character LLVM local identifiers freely allow but a
  Plum source identifier can NEVER contain (`is_ident_start`/`is_ident_
  continue` only ever accept `[a-zA-Z_][a-zA-Z0-9_]*`), so the
  collision is now structurally impossible, not just avoided for the
  two words that happened to get hit. `let`-bound locals and closure
  CAPTURES were already safe (both map to a compiler-generated `%v<N>`
  register, never a raw name) — only these two call sites needed the
  fix. A native-codegen regression test pins both `entry` and `env` as
  parameter/closure-param names compiling and running correctly.
- ~~KNOWN BUG — `Unit.to_string()` renders as `"false"` in native
  codegen instead of being rejected/rendered correctly, a real
  interpreter/codegen behavioral-parity gap~~ **RESOLVED in chunk 18,**
  per the user's own explicit choice (asked directly, given this was a
  real design fork, not an obvious bug fix): `Unit.to_string()` now
  renders the literal `"Unit"` in BOTH backends, everywhere (a bare
  top-level call, nested inside an array/struct field). Native codegen
  gained a real `CgType::Unit` arm (previously merged with `Bool`'s,
  which "happened to work" for `Bool` but was simply wrong for `Unit`)
  in both the top-level `Expr::ToString` codegen (which used to reject
  Unit as a compile error — the array/struct-field-nested path was the
  ACTUAL source of the `"false"` mis-render, via `render_word_as_
  string`'s shared arm) and that shared helper; the interpreter's own
  `render_value` gained a matching `Value::Unit` arm (previously fell
  into the generic "not yet supported" error). Regression tests in
  `plum-interp` and `codegen_cli.rs` (both bare and array-nested shapes)
  pin the literal `"Unit"` output on both backends.
- **KNOWN BUG, FIXED in this same chunk — a real native-codegen crash
  for `Array[Bool]`/`Array[Unit]` `.to_string()`.** Found the same way
  as the entry above (writing a new `examples/` file that chained two
  `.map()` calls, the second returning `Array[Unit]`) — `plum build`
  failed outright with `clang` rejecting the generated LLVM IR:
  `"PHI node entries do not match predecessors!"`. Root cause:
  `emit_array_to_string_fns` (`plum-codegen/src/lib.rs`) builds a
  counted loop whose back-edge `phi` nodes hardcoded `%render_elem` as
  the predecessor block — but its own per-element helper, `render_word_
  as_string`, branches into EXTRA blocks for `Bool`/`Unit` elements
  specifically (to pick between `"true"`/`"false"` text), so the code
  emitted right after that call actually lands in the merge block it
  opened, not literally `%render_elem` — the loop's `br label %loop_
  check` really originates from that merge block, and `clang` correctly
  rejected the stale hardcoded predecessor. Since monomorphization
  emits this function eagerly for every array type reachable in the
  program (whether or not its `.to_string()` is ever actually called),
  ANY program with an `Array[Bool]` or `Array[Unit]` ANYWHERE — e.g.
  `xs.map(f).map(println)`, whose outer `.map()` produces `Array[Unit]`
  — broke native codegen outright, a fairly easy shape to hit by
  accident. FIXED: `render_word_as_string` now takes an in/out
  `current_block: &mut String` cursor, updated whenever it opens new
  blocks itself, and `emit_array_to_string_fns`'s loop-back-edge phi
  predecessors now read that real final-block label instead of the
  stale hardcoded one (built via a two-buffer approach, since the loop
  preamble's phi nodes are emitted textually BEFORE the label they need
  is known). `emit_struct_to_string`, `render_word_as_string`'s other
  caller, never needed this — it has no fixed block name anywhere else
  in its own output to go stale against. Verified via the original
  crashing repro now compiling and running correctly on both backends,
  plus new coverage for `Array[Bool]`/`Array[Unit]` `.to_string()`
  specifically.
- ~~KNOWN BUG — FBIP reuse-in-place correctness gap for aliased
  parameters~~ **RESOLVED.** Root cause: `insert_refcount_ops`
  (`plum-ir/src/fbip.rs`) deliberately never adds function PARAMETERS
  to its `known_heap` set (no type checker in this IR to prove one is
  heap-shaped), so a parameter's refcount is never `Inc`'d even when
  genuinely aliased (used twice in one body). `mark_reuse`, the
  separate pass that rewrites `StrConcat`/`ArrayPush`/`Match`'s
  scrutinee/etc. into their `*Reuse` variants, didn't know or care
  about any of this — it rewrote ANY bare-`Var` base into a reuse
  candidate, on the false claim (a stale comment) that the runtime
  refcount check alone made this safe regardless. For an unprotected
  aliased parameter (e.g. `s.concat(rep(s, n - 1))`, where `rep`
  recurses on `s` again), BOTH the outer `.concat()` and the inner
  recursive call saw refcount 1 and each independently believed itself
  the sole owner, so both destructively reused the SAME heap cell —
  whichever ran second silently clobbered what the first had written.
  Confirmed real (not backend-specific) by both the interpreter and
  native codegen agreeing on the same wrong answer for a minimal
  standalone repro.
  **Fix**: `mark_reuse` now threads the identical `known_heap` set
  `insert_refcount_ops` computes (same `Let`-only growth, same
  non-extension for `Match` arm bindings/`Closure` params/`For` loop
  vars — recomputed via the same `is_syntactically_heap` helper both
  passes now share), and every `*Reuse` rewrite is gated on the base
  name actually being a member. A name `insert_refcount_ops` never
  protected can no longer be reused in place by construction, closing
  the gap for every affected op uniformly (`StrConcat`, `StrTrim`,
  `StrToUpper`, `StrToLower`, `StrReplace`, `ArrayPush`/`Pop`/`Set`/
  `Remove`, and `Match`'s `CtorReuse`), not just `StrConcat`. This is a
  real (bounded) loss of optimization opportunity — a parameter used
  only once could, in principle, still be safely reused, but proving
  that needs real type info this untyped IR doesn't carry yet — traded
  for always being correct. `String.repeat` was reverted from its
  chunk-15 workaround ordering back to the natural `s.concat(String.
  repeat(s, n - 1))`, since it's now safe either way; both orderings
  have dedicated regression tests (`plum-ir/src/fbip.rs`'s own unit
  tests plus `plumc/src/lib.rs`'s and `codegen_cli.rs`'s end-to-end
  ones) asserting the CORRECT answer for the once-unsafe shape
  directly, verified through both `plum run` and a compiled `plum
  build` binary agreeing.
- ~~KNOWN BUG — `plum-types::subst::Subst`'s composition~~ **RESOLVED
  in chunk 13.** Was two genuinely separate bugs, not one: (1)
  `Subst::compose` could merge two individually-acyclic substitutions
  into a self-referential `id -> Var(id)` cycle, which `Subst::apply`
  then recursed on forever — fixed by having `compose` drop (not
  insert) any merged binding that resolves back to its own key. (2)
  `default_numeric` defaulted an unconstrained numeric type too early
  inside a closure literal passed directly to `.map`/`.filter`/`.fold`,
  before the callback's params were ever connected to what the call
  site already knew about them — fixed by seeding a closure-literal
  callback's params from the array's already-known element/accumulator
  type before inferring its body, via a new `Infer::infer_expr_as_
  callback` used by all three builtins. See "Standard library" chunk
  13 above for the full root-cause writeup and fix detail.
  `array_sort_by`/`array_zip`/`array_sum_int`/`array_sum_float` are
  now shipped, unblocked by both fixes.
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
  Decided (see "Strings" above). `.to_string()` for structs/enums/
  arrays (named-field structs, positional enum variants, bracketed
  arrays, `Map`/`Set` rendering generically as their underlying enum) is
  now ALSO Decided, in both backends — see "Standard library, chunk 7"
  above. `Tuple` is the one remaining exclusion (same structural
  blocker as `Eq` — `CgType` has no `Tuple` variant). Still open, for
  strings specifically: other standard string operations (e.g.
  `repeat`), and grapheme-cluster-aware operations (a
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
  basic output/`println`/`print`, then `Map`/`Set` collections plus
  `Set` algebra, then basic file I/O as of chunk 8 — `read_file`/
  `write_file`, `Result[T, Str]`-returning). Still wide open: what
  comes next (JSON — explicitly the stated next step — then HTTP,
  string utils beyond what's already a core-language builtin, ...), and
  whether/when `println`/`print`/`Map`/`Set`/`read_file`/`write_file`
  migrate from the
  prelude into real `use`-based modules once there's enough stdlib
  surface to justify extending the `compile_and_run` test harness to
  drive a real temp project through `resolve_project`. The two real
  compiler bugs found in chunk 4 (an empty array literal unable to
  cross a generic-function-call boundary; a closure passed to
  `.fold()` calling a curried function producing invalid LLVM IR) are
  both FIXED as of chunk 5; `map_keys`/`map_values`/`set_to_array` are
  implemented as the direct payoff. The `Eq`-bound/structural-equality
  gap flagged here previously is now CLOSED as of chunk 6 — real
  structural equality for structs/enums/arrays in both backends, and a
  tightened `Eq` bound that actually reflects it, with a struct-keyed
  `Map` as the direct payoff. One deliberate, documented asymmetry
  remains, not treated as unfinished: tuple equality works fully in
  the interpreter but stays unsupported in native codegen (`CgType`
  has no `Tuple` variant at all — real per-shape/monomorphized tuple
  codegen types would be needed first, a separate future chunk).
- Whether/when to build the scoped incremental cycle collector for
  `Shared` values (see Memory model above — deliberately deferred until
  real Plum code shows the pain is real).
- Whether Plum curries function application by default (ML-family
  norm), deferred because of its interaction with the calling
  convention and FBIP (see Surface syntax above; note the `|>` pipe
  desugaring does NOT depend on this being resolved).
- **Future stdlib/toolchain roadmap, flagged 2026-08-11, none of it
  started yet**: HTTP as a real stdlib module — Brad wants BOTH a
  client and a server, not just the client-side fetch-shaped thing
  most young languages ship first. TCP/UDP sockets — tentative
  ("maybe", Brad's own word), likely only worth it if HTTP's own
  implementation doesn't already need them exposed as a byproduct.
  Cryptographic primitives — scope (hashing only? symmetric/asymmetric
  too?) not yet discussed. Running alongside all of it as an ongoing
  lens on every future stdlib/toolchain decision, not a one-time
  chunk: everything that moves the toolchain closer to SELF-HOSTING —
  a Plum compiler written in Plum. File operations (`read_file`/
  `write_file` exist as of chunk 8, whole-file only, no streaming)
  were flagged as probably belonging in a real `IO` module, built on
  the record-of-closures Reader/Writer pattern just below — not
  staying flat prelude functions forever.

### Interfaces/traits for Reader/Writer-shaped abstractions — Decided (2026-08-11): no new construct, records of closures

Raised by Brad ("should we have interfaces???") specifically wanting
Go's `io.Reader`/`io.Writer`-shaped abstractions for streaming I/O, not
a general OOP interface system. Considered and rejected: a real user-
definable trait system (Rust-style nominal `impl Trait for Type`, or
Go-style implicit structural satisfaction) — the "Ad-hoc polymorphism
(v1)" section above already deliberately deferred this exact thing, on
purpose, to keep the type system's complexity budget on the memory
model rather than generics machinery; reopening it now for Reader/
Writer specifically would be solving a much bigger problem than the one
actually in front of us.

**Decided instead**: Plum already has everything Reader/Writer needs,
today, with zero new language machinery — a STRUCT WHOSE FIELDS ARE
CLOSURES (the standard ML-family "record of functions" pattern, not a
Plum-specific invention):

```
struct Reader { read: (Array[Int]) -> Result[Int, String] }
struct Writer { write: (Array[Int]) -> Result[Int, String] }

let file_reader (f: File): Reader = Reader { read: |buf| unsafe { file_read_raw(f, buf) } }
let socket_writer (s: Socket): Writer = Writer { write: |buf| unsafe { socket_write_raw(s, buf) } }

let copy (r: Reader) (w: Writer): Result[Unit, String] = { ... }  // works on ANY Reader/Writer
```

This already type-checks and runs with the language exactly as it
exists — struct fields can be `Function` types, closures are first-
class values, both true since the very first codegen chunks. Gives, for
free: multi-method "interfaces" (a struct with several closure fields
is a hand-assembled vtable), heterogeneous storage (`Array[Reader]`
mixing a file-backed and a socket-backed reader — closures already
carry their own captured environment, no separate vtable machinery to
invent), and zero tension with "no macros, no typeclasses."

**What this deliberately gives up**, weighed and accepted rather than
overlooked: no compile-time "this type implements Reader" check (you
just call a constructor function like `file_reader` that builds the
record — nothing enforces the shape beyond what building any struct
literal already enforces); no static, monomorphized generic dispatch
(`fn copy[R: Reader]` isn't expressible — every call goes through the
closure, dynamic dispatch by construction); no retrofitting an
existing type with a new "interface" after the fact without writing an
adapter constructor (consistent with everything else in Plum being
nominal, not a new gap this decision introduces). For I/O specifically,
a syscall dwarfs a closure-call's overhead, so the dynamic-dispatch
cost is real but not expected to matter in practice.

**Not revisited preemptively**: if real Plum code (the future HTTP/IO
work above) surfaces a genuine need for compile-time conformance
checking or zero-cost static dispatch that records-of-closures can't
give, that's the trigger to reopen user-definable traits as their own
real design conversation — not something to build ahead of that need.

### Nested field-update path sugar (`ship.position.x: nx`) — Decided and implemented

Motivated directly by porting an Asteroids demo (`games/plum/`): deeply
nested structs (`Game { ship: Ship { position: Vec2 { .. } } }`) made
even a single-field deep update require hand-reconstructing every
intermediate level with `..` spread. Considered and rejected: (a)
Elm-style shallow-only record update — Elm itself doesn't have a deep
variant; this is a known, still-unresolved pain point in real Elm code,
not a precedent to port. (b) Composable lenses (Haskell/Elm-community
style) — fully general (works through generics, computed access) but
needs real new machinery (a `Lens` type, composition combinators) and a
noticeably different call-site shape; too heavy for what this language
needs, and in tension with Plum's existing "no macros, no typeclasses"
bias. Landed on Haskell's `RecordDotSyntax`/Idris's record-update
precedent instead: a dotted path AS the field key.

`Game { ship.position.x: nx, ship.position.y: ny, ..g }` expands, via a
new pre-inference AST-rewrite pass (`plumc::nested_struct_update`,
architecturally identical to `plumc::assoc_fns` — see its own doc
comment), into `Game { ship: Ship { position: Vec2 { x: nx, y: ny, ..g.
ship.position }, ..g.ship }, ..g }` — paths sharing a prefix merge into
ONE nested literal per level. Requires the literal to also carry a `..`
spread (nothing else to read the intermediate values from otherwise).
Once expanded, the completely UNCHANGED existing struct-literal type-
checking/lowering/FBIP-reuse-in-place machinery handles the rest —
including, for free, "unknown field"/"missing field"/"duplicate field"
errors (a path colliding with a plain field of the same name becomes
two ordinary `FieldInit`s with the same name, already rejected).

**Known v1 scope limit, accepted deliberately**: an intermediate
segment whose declared field type is still a bare generic parameter
(`struct Wrapper[T] { inner: T }`, `w.inner.field: x`) can't be
resolved to a concrete struct name at this stage — the pass only has
`TypeContext`'s DECLARED field shape (`Type::Param("T")`), not whatever
`T` happens to be instantiated to at any particular use site (real
per-call-site type inference would be needed, a materially bigger
change). A clear compile error, not silently wrong behavior.

**A real, subtle bug found and fixed while implementing this**: the
first version gave every synthesized `Field`-access node (`g.ship`,
`g.ship.position`) THE SAME reused span (the outer literal's, or later,
one shared per-original-field span reused unchanged at every nesting
depth). `infer.rs`'s `field_owners` is a `HashMap<Span, StructName>`
side-channel lowering depends on (lowering has no type information of
its own) — two DIFFERENT `Field` nodes sharing one span silently
clobber each other's entry, so lowering read back the WRONG struct's
field list for one of them and failed with a spurious "struct X has no
field Y", despite the desugared AST itself being structurally correct
end to end (confirmed by direct inspection — the bug was invisible at
the AST level, only surfacing at lowering). Root-caused by realizing
per-nesting-depth spans need to be genuinely UNIQUE, not just "real
spans from parsing" reused across levels. Fixed by giving `ast::
FieldInit` a new `name_span: Span` field (the span of JUST the field
name, separate from `span`'s whole-`FieldInit` range) and extending
`extra_path` from `Vec<String>` to `Vec<(String, Span)>`, so every
segment at every nesting depth has its own real, distinct span to give
its synthesized `Field` node — not a fabricated/offset one.

### String interpolation (`"hello, ${name}!"`) — Decided and implemented

`${...}` inside any double-quoted string, no `f"..."` prefix. Desugars
entirely at lex/parse time into ordinary `.concat()`/`.to_string()`
calls (`"a".concat(x.to_string()).concat("b")`) — both already real,
already generic over every type — so `plum-types`/`plum-ir`/both
backends need ZERO new code; the whole feature lives in `plum-syntax`'s
lexer and parser.

**The real design fork**: how much can legally appear inside `${...}`.
Considered and rejected: full arbitrary expressions (Kotlin/Swift/JS
template-literal style), including nested block expressions/closures/
struct literals and even a nested string with ITS OWN `${...}`. That
needs a genuinely stateful lexer — a mode stack toggling between
"scanning string characters" and "scanning ordinary tokens," tracking
brace depth so `${if x { 1 } else { 2 }}` or a doubly-nested `${f("${
inner}")}` don't get the wrong closing `}` — a class of complexity
nothing else in this lexer has needed. Landed instead on a RESTRICTED
scope, deliberately mirroring the `_` placeholder-chain sugar's own
precedent (GRAMMAR.md: "not a general Scala-style placeholder usable
anywhere"): `${...}`'s closing `}` is found by tracking ONLY `(`/`[`
depth (so `${f(a, g(b))}` works) and skipping a nested plain string's
content wholesale (so `${f("a}b")}`'s embedded `}` doesn't end the
interpolation early) — `{`/`}` themselves are never depth-tracked, and
a nested string's own `${...}`, if it had one, is never re-interpreted
(interpolation does not recurse). A block expression, closure with a
block body, struct literal, or `if`/`match` inside `${...}` will, at
worst, grab the wrong closing `}` and produce a raw substring that
fails to parse as a valid expression — a real, visible parse error
(enriched with a hint when the failed substring contains `{` or `"`,
since that's the overwhelmingly likely cause), never silently wrong
behavior. Covers the vast majority of real uses (`"${g.score}"`,
`"${p.x}"`, `"${f(x, y)}"`, `"${a + b}"`) with a bounded, single-level
lexer change (no mode stack) instead.

**Implementation shape**: the lexer's `lex_string` now optionally
produces `TokenKind::InterpStr(Vec<InterpPart>)` (a NEW variant sitting
ALONGSIDE the existing `TokenKind::Str(String)`, which stays completely
unchanged and is still what every non-interpolated string literal — the
overwhelming common case — lexes to) — an alternating `Literal(String)`/
`Expr(String, Span)` sequence, where each `Expr` part is the interpolated
expression's RAW SOURCE TEXT plus its real span in the ORIGINAL file
(`Lexer::with_base_offset`'s existing offset mechanism, already proven
for merging the prelude into user source, gives this for free — no new
span machinery). The PARSER, on seeing `InterpStr`, re-lexes+parses each
`Expr` part's raw text as an ordinary expression and folds the whole
thing into a left-associated `.concat()`/`.to_string()` call chain,
skipping a `.concat("")` for an empty literal segment (so `"${x}"`
desugars straight to `x.to_string()`, not `"".concat(x.to_string()).
concat("")`) purely to keep the generated tree lean, not for
correctness. Zero new `ast::Expr` variant either — the desugared output
is indistinguishable from the same call chain hand-written directly.

### TCP sockets — Decided and implemented (2026-08-11): first piece of the networking roadmap, UDP deferred

Kicked off the "HTTP/TCP/crypto/self-hosting" roadmap bullet above,
starting with TCP as the foundation HTTP client/server will eventually
sit on (an HTTP/1.1 client/server is just a parser + state machine over
`send`/`recv` — belongs entirely in Plum-level code once TCP exists,
not a separate C HTTP library dependency, same self-hosting-friendly
instinct as everything else on this list).

**How other languages handle this, checked rather than assumed**: every
one of them crosses an FFI boundary somewhere — `socket()`/`connect()`/
etc. are OS syscalls, not implementable in userspace. Go makes raw
syscalls directly (bypassing libc, with per-OS files for the differing
ABI); Rust's `std::net` calls libc sockets on Unix and Winsock on
Windows via two separate code paths; Python/Ruby/Node/Java all go
through a C extension or native layer either way. So `extern "C"` +a
hand-written shim isn't a workaround here, it's the only path in,
matching what everyone else does.

**Unix-only (Linux/macOS) for v1 — a real platform gap, honestly
documented, not silently swept.** Windows' Winsock is a genuinely
different API in the details that matter for a shim (needs `WSAStartup`
/`WSACleanup`, `SOCKET` isn't `int`, `closesocket()` not `close()`, and
needs `ws2_32.lib` explicitly linked) — a POSIX-sockets shim won't even
compile against it. This mirrors the EXISTING extern-symbol-resolution
scope exactly (`libloading::Library::this()` is already "Unix-only for
v1... Windows has no clean equivalent... an honest, documented gap, not
a silent wrong answer" — see the FFI section above), so this isn't a
new kind of gap for the project, just the same one showing up again.

**A native C shim was required, not optional**, for the same reason
`raylib_shim.c` exists: POSIX sockets need `struct sockaddr`/`struct
addrinfo`/`socklen_t *` — none of which fit Plum's `extern "C"` type
surface (`Int`/`Float`/`Bool`/`CStr`/qualifying-struct only, no raw
pointers). `native_stdlib/net_shim.c` hides all of that behind flat
`Int`/`CStr` functions (`tcp_connect(host: CStr, port: Int) -> Int`,
`tcp_listen`, `tcp_accept`, `tcp_send`, `tcp_recv`, `tcp_close`),
building the real `sockaddr`/`addrinfo` structs internally.

**A genuinely new gap found and closed along the way: `CStr` had no way
back into a usable `String` at all.** `.as_cstr()` only ever went `Str
-> CStr`; `Type::CStr` had zero operations of its own (the existing
`getenv` test immediately discarded its `CStr` result specifically
because there was nothing else to do with it — `env_var`/`read_file`
sidestepped this entirely via compiler-builtin IR nodes that never
touch `CStr` for the VALUE at all). This blocked `tcp_recv` from
returning usable data, and isn't sockets-specific — it would've blocked
ANY future extern call meant to return real string content (`getcwd`,
`readlink`, ...). Closed generally, not with a one-off workaround:
added `.as_string()` (the symmetric inverse of `.as_cstr()`, same
shape-based-recognition precedent, `ir::Expr::AsString`), rather than
routing `tcp_recv` through ANOTHER bespoke compiler-builtin IR node the
way `read_file`/`env_var` did. Interesting codegen wrinkle found while
building it: a `CStr`-typed extern call's OWN return value ALREADY gets
copied into a real `CgType::Str` register automatically, by `codegen_
extern_call`'s existing `Some(ir::ExternType::Str)` arm (previously only
reachable via `let _ = ..`-and-discard, e.g. `getenv`'s own null-check
test) — so `.as_string()` on that path is a pure type-level pass-
through, no copy of its own; it only needs a REAL copy
(`@strlen`+`@plum_alloc_str`+`@memcpy`) for the other, rarer origin: a
bare `CgType::CStr` buffer from `.as_cstr()` itself (e.g. the round trip
`s.as_cstr().as_string()`). Verified both origins directly with real
compile-and-run tests, not just unit-level IR assertions.

**`tcp_recv` returns `CStr`, not `Int` count** — deliberately, so
`.as_string()` can turn it into usable data at all. Two real, honestly-
documented v1 scope trades follow from that:
- **Not binary-safe.** A `CStr` is NUL-terminated; an embedded `\0`
  byte in a response silently truncates it. Acceptable for a v1 scoped
  at line-oriented HTTP/1.1 (headers + mostly-text bodies); would be a
  real problem for arbitrary binary payloads over raw TCP.
- **"Peer closed" and "real socket error" are collapsed into the same
  empty-string result.** A null `CStr` return is a hard runtime ABORT
  under Plum's existing FFI semantics (see the `CStr` null-return note
  above) — crashing the whole program on an ordinary, everyday
  connection close isn't acceptable for a `Result`-shaped Net API, so
  `tcp_recv` never returns null, on EITHER path, discarding the real
  distinction between them. "Stop reading" is the right response either
  way, which is exactly what an empty `String` already signals to a
  caller looping until end-of-stream.

**A `--gc-sections`/`dlsym` linking wrinkle, found and fixed empirically
(not guessed at)**: getting `net_shim.c`'s functions merely COMPILED
into `plum-interp`/`plumc`'s own process wasn't enough for `plum run`'s
extern-call resolution (`Library::this()` + `dlsym(RTLD_DEFAULT, ..)`)
to actually find them — two separate problems, confirmed one at a time
with a minimal standalone repro before touching the real build scripts:
1. Rust's default linker flags include `--gc-sections` (dead-code
   stripping) — since nothing in the STATICALLY linked Rust code ever
   references `tcp_connect`/etc. directly (every call goes through
   runtime symbol resolution, same "no real linked reference" shape
   `-lm`'s own `--no-as-needed` note already describes for a different
   reason), the whole `net_shim.o` translation unit was silently
   dropped from the binary entirely. Fixed with a per-symbol `-Wl,-u,
   <name>` for each of the six functions — forces the linker to treat
   each as a real GC root, without disabling `--gc-sections` (and its
   real binary-size benefit) for the REST of the binary the way a
   blanket `--no-gc-sections` would.
2. Even once kept, the symbols still weren't `dlsym`-visible: `dlsym
   (RTLD_DEFAULT, ..)` only searches a process's DYNAMIC symbol table
   (`.dynsym`), and a normal PIE executable's own locally-defined
   symbols aren't exported there by default (unlike `sqrt`, which lives
   in `libm.so`'s OWN already-dynamic table — a separate shared object
   `dlsym` also searches). Fixed with `-Wl,--export-dynamic`.

Both `plum-interp`'s and `plumc`'s own `build.rs` independently compile
`native_stdlib/net_shim.c` and apply both flags (a dependency crate's
build-script link-args don't propagate to a separate downstream binary
target — the EXACT same reason `plumc/build.rs` already had to re-emit
its own `-lm`/`--no-as-needed` rather than trusting `plum-interp`'s copy
to cover it, confirmed real, not just a defensive guess, the first time
`-lm` was wired up).

**A SEPARATE linking story for `plum build`'s own output.** `net_shim.c
`'s source is `include_str!`'d directly into the `plumc`/`plum` binary
at compile time (not read from disk at runtime — an installed `plum`
binary has no guarantee the original source tree is anywhere nearby),
written back out to a real temp `.c` file and passed to `clang`
alongside the generated `.ll` for every `plum build` invocation, and —
found only once the FULL test suite was run, not just the new TCP
tests — for EVERY OTHER independent `clang` invocation this crate's own
test harness has (`run_via_clang_with_c_helper`, `run_under_sanitizer_
with_src`, and one inlined ASan test), since the `Net` prelude's `tcp_*`
wrapper functions are ordinary, non-generic top-level functions —
unlike a generic function (only ever codegen'd per call-site
instantiation), those get emitted into EVERY compiled program's IR
unconditionally, whether a given program calls any of them or not. 12
previously-unrelated codegen tests broke on exactly this ("undefined
reference to `tcp_connect`") the first time the full suite ran after
wiring up `STDLIB_NET_SRC` — found and fixed before calling the chunk
done, not left as a surprise for later.

**UDP was raised, then explicitly deferred** rather than shipped
alongside TCP in the same pass: `recvfrom()` needs to hand back the
sender's address as well as the data, and Plum's FFI has no multi-value
return (and `CStr` can't live inside a struct either) — the only way to
get it is a SECOND call reading state a first call stashed C-side,
which is a genuine race if two `spawn`ed tasks ever call it
concurrently. Brad chose "ship TCP only now" over shipping something
concurrency-unsafe just to say UDP was included, or dropping the
sender's address entirely (unusable for request/reply protocols). Real
follow-up work, not forgotten: needs its own concurrency-safe design
for the sender-address problem before it's worth building.

**Exposed as `Net`-flavored plain functions** (`tcp_connect_to`, `tcp_
listen_on`, `tcp_accept_connection`, `tcp_write`, `tcp_read`, `tcp_
close_connection`), merged into `with_prelude` as `STDLIB_NET_SRC`, same
"no `use` needed yet" pattern every other stdlib piece uses today —
`Type.func(...)` associated-function styling wasn't a fit here since a
socket is just an `Int` fd, no natural product-type receiver to hang
associated functions off of. Every fallible one wraps the raw `extern`
call in the same `let r = ..._raw(...); if <ok> { Ok(..) } else { Err
(..) }` shape `STDLIB_FILE_SRC`/`STDLIB_ENV_SRC` already established,
sentinel `-1` for a failed `Int`-returning call standing in for the
`errno`-message detail those two have and this genuinely doesn't (no
richer error code survives crossing this particular shim).

Verified with real compile-and-run tests in BOTH backends (interpreted
`plum run` and native `plum build`) — an actual loopback `listen`/
`connect`/`accept`/`send`/`recv`/`close` round trip over a real TCP
socket, not a mocked/stubbed one, confirming the whole stack end to
end: the real shim, the linking/exporting fixes above, and `.as_string
()` turning `tcp_recv`'s result into a comparable `String`.

### HTTP client — Decided and implemented (2026-08-11): http:// only, built as pure Plum on top of TCP

Second piece of the networking roadmap, immediately after TCP. Built
entirely as ordinary Plum source (`STDLIB_HTTP_SRC`, merged into `with_
prelude` same as everything else) on top of the `Net` module above —
zero new IR/backend/extern surface, the whole point of doing TCP first.
Exposes `http_get(url)`, `http_post(url, body)`, and the general `http_
request(method, url, headers, body)`, all `Result[HttpResponse, String]`
(`HttpResponse { status: Int, headers: Array[HttpHeader], body: String
}`).

**`http://` only.** `https://` is explicitly rejected with a clear
`Err`, not silently attempted — TLS needs a real implementation or FFI
to a library like OpenSSL/LibreSSL, a genuinely new native dependency
and its own design question, deliberately weighed against and deferred
rather than bundled into this pass (the same "flag the big fork before
building" instinct as everything else on this list).

**Response body framing — a real, honest v1 scope trade, same spirit as
`tcp_recv`'s own.** A `Content-Length` header is read exactly that many
bytes; a `Transfer-Encoding` header (chunked or otherwise) is REJECTED
with a clear `Err`, never silently mis-parsed as a literal body;
anything with neither is read until the connection closes (safe since
every request sends `Connection: close`). Response header NAME matching
is exact-case only (`Content-Length`, not case-insensitively) — true in
practice for virtually every real server, a real simplification
nonetheless.

**No `while` loop exists in this language** (only `for i in a..b`), so
every "read until X" operation (waiting for the header/body separator,
reading a known-length body, reading until close) is a tail-recursive
accumulator function — the exact same idiom `STDLIB_STRING_SRC`'s own
parsers (`string_parse_int_digits_acc`, etc.) already established.
Nothing new stylistically, just a bigger, more realistic exercise of it.

**Two real bugs found while building this, both fixed, one predating
this chunk entirely:**

1. **A genuine, previously-undiscovered PARSER restriction**: the
   struct-literal-vs-block disambiguation heuristic (`GRAMMAR.md`) keys
   off the type name's first character being UPPERCASE — `Identifier {
   ... }` is only recognized as a struct literal when `Identifier`
   starts uppercase. `__FileIoResult`/`__EnvResult` (the double-
   underscore internal-struct convention `STDLIB_FILE_SRC`/`STDLIB_ENV_
   SRC` already use) never tripped this because those are ONLY ever
   constructed by Rust-side compiler-builtin codegen, never as real
   Plum struct-literal SOURCE TEXT. This HTTP module's own internal
   parsing-result structs (`HttpUrlParts`, `HttpHead`) ARE constructed
   as ordinary Plum expressions, so the double-underscore convention
   genuinely breaks there — confirmed directly with a minimal repro
   (`struct __Foo { x: Int } let go (): __Foo = __Foo { x: 1 }` fails
   with a confusing "expected an item... found LBrace", not an obvious
   error pointing at the real cause). Fixed by simply using plain
   capitalized names for these two structs instead — not a language bug
   to fix, a real constraint on anything actually built via struct-
   literal syntax, worth knowing for any FUTURE internal struct that's
   also constructed in real Plum source (not just returned/consumed by
   Rust-side codegen).
2. **A real use-after-free in `tcp_write` — predates this HTTP chunk
   entirely (it shipped with `STDLIB_NET_SRC`), surfaced only now
   because a bare string literal with no other reference (`tcp_write
   (fd, "hello")`) hits the unlucky case an already-referenced variable
   (`STDLIB_NET_SRC`'s own round-trip TEST happened to use) doesn't
   reliably hit.** `tcp_send(fd, data.as_cstr(), data.len())` evaluates
   `.as_cstr()` (which FREES `data`'s cell if this was its last
   reference — see `.as_cstr()`'s own doc comment) before `.len()`,
   reading a potentially-already-freed cell. This is the EXACT bug
   `println`'s own doc comment (right there in the same file) already
   documents finding and fixing once before (`write(1, s.as_cstr(), s.
   len())` → `let n = s.len(); write(1, s.as_cstr(), n)`) — missed the
   first time because `tcp_write` was written without re-reading that
   warning. Confirmed directly, not theorized: an actual byte-for-byte
   capture on the wire (a real `recv()` on the other end) showed `"hello
   "` followed by ~4KB of garbage (a freed allocator chunk's leftover
   freelist bookkeeping, read back as if it were the string's length)
   instead of exactly 5 bytes. `write(2)`'s own "silent, not a crash"
   character (per that original doc comment) is exactly why the
   existing `tcp_round_trip` test — a like-for-like comparison of what
   was SENT against what was READ BACK, both ends in the SAME process —
   never caught it: whatever got sent (correct or garbage) was also
   whatever got echoed and compared, a tautology that can't detect a
   send-side bug. Fixed the same way `println`'s was: `let len = data.
   len();` bound BEFORE `.as_cstr()` runs.

**A genuine, backend-SPECIFIC recursion-depth limitation, found and
scoped honestly rather than worked around silently.** `String.index_of`
(used internally to find the header/body separator) recurses to a depth
proportional to the search string's length. `plum-interp`'s tree-
walking `eval` has NO tail-call optimization (unlike native codegen's
real `musttail` guarantee), so a REALISTIC-sized HTTP response (a ~138-
byte header block, nowhere near contrived) already needs a stack far
beyond even the workspace's existing 16 MiB test-thread bump (sized
against much shorter strings) — 256 MiB was needed to pass reliably
under `cargo test`. Confirmed this is genuinely backend-specific, not a
property of the algorithm: the equivalent native-codegen test needs NO
special stack handling at all. This is a real, pre-existing
characteristic of the interpreter (not something this chunk introduced,
just the first thing to exercise it with realistic data) worth knowing
before leaning on `plum run` for any workload doing substantial string
search/processing — `plum build` has no such ceiling.

Verified with real compile-and-run tests in BOTH backends: an actual
HTTP/1.1 round trip against a real `std::net::TcpListener` fixture (not
mocked), confirming URL parsing, request building, response header/
body-framing parsing, and the `https://` rejection path, end to end.

### HTTP server — Decided and implemented (2026-08-11): sequential (one connection at a time), built on the same request/response parsing as the client

Third piece of the networking roadmap, immediately after the HTTP
client. Sequential for v1 (Brad's explicit choice, weighed against
spawning a task per connection): `accept → read request → call handler
→ write response → close → repeat`. Real concurrency (spawn-per-
connection, using the `spawn`/`.join()` primitive whose whole-scope-
capture bug was just fixed this same session) is a real, isolated
follow-up once this is proven correct — not bundled in now, so this
pass doesn't compound network+parsing+concurrency risk at once.

Exposes `http_serve_once(port, handler)` (listens, accepts and handles
exactly ONE connection, then returns — a real, useful one-shot server
on its own, and what this module's own tests exercise directly) and
`http_serve(port, handler)` (the real long-running server: accept,
handle, close, repeat, forever). `handler: (HttpRequest) -> HttpResponse`
— `HttpRequest { method, path, headers, body }`, reusing `HttpResponse`/
`HttpHeader` from the client side unchanged. Every request gets exactly
one response and the connection is always closed afterward — no keep-
alive, symmetric with the client always sending `Connection: close`.

**Two real bugs found while building this:**

1. **`let _ = expr;` is not a supported `let`-binding shape** — `_` is a
   valid MATCH-arm pattern but plum-types' `let`-binding inference only
   handles `Ident`/`Tuple`/`Struct` patterns, hitting its own catch-all
   \"destructuring let-bindings of this shape\" error otherwise. Wanted
   to discard `http_handle_connection`'s `Result` inside `http_serve_
   loop` (one bad connection shouldn't kill the server) — fixed by using
   a bare statement instead (`http_handle_connection(conn, handler);`),
   the SAME idiom `println`'s own `write(1, s.as_cstr(), n);` already
   established for discarding a non-`Unit` value. Not a language bug to
   fix — a real, existing constraint, just not one this codebase had
   run into with a `let` before (only ever inside `match` arms).
2. **A genuine, real protocol-framing asymmetry between requests and
   responses — found via an actual live deadlock, not by inspection.**
   Originally reused the CLIENT's own `http_read_body` unchanged for
   request parsing too, on the assumption body-framing rules (`Content-
   Length`/`Transfer-Encoding`/read-until-close) don't care which side
   of the connection they're reading. That assumption is WRONG: on the
   response side, \"no `Content-Length`, no `Transfer-Encoding`\"
   legitimately means \"read until the peer closes\" — safe, because
   every request this client sends already includes `Connection:
   close`, bounding that wait by the SERVER eventually closing. Applied
   to a REQUEST, the same rule is a deadlock waiting to happen: a
   bodyless `GET` (no `Content-Length`, since there's no body to
   measure) has no reason to ever close its own write side, so a server
   \"reading until close\" on it blocks forever waiting for a body that
   will never arrive. Root-caused by isolating the exact hang site with
   `println` debugging (accept → headers → parse-head → body-read, one
   step at a time) after a real test hung indefinitely against a real
   socket — not solved by inspection first. Fixed with a genuinely
   separate `http_read_request_body`, identical to `http_read_body`
   except its \"neither header present\" case returns an EMPTY body
   instead of reading until close.

Verified with real compile-and-run HTTP/1.1 round trips in BOTH
backends — a plain `std::net::TcpStream` fixture acting as the CLIENT
this time (the reverse of the HTTP client's own tests), sending a real
bodyless `GET` (deliberately, since that's the exact request shape that
caught bug #2 above) and checking the handler saw the real parsed
method/path, not just that SOME 200 came back.

### OS module: directory listing + subprocess exec — Decided and implemented (2026-08-11): the two hard self-hosting blockers

Picked directly out of a "when does it make sense to self-host"
discussion with Brad: two HARD blockers (a self-hosted `plum build`
needs to shell out to `clang`; module resolution needs to walk a
project's file tree) plus lower-priority should-haves (real hash-based
`Map`/`Set`; `?`/early-return sugar) — Brad chose to close the two hard
blockers now.

**Same `net_shim.c` shim pattern throughout** (see that file's own doc
comment for the general story) — `dir_shim.c`/`process_shim.c`, flat
`Int`/`CStr` extern functions hiding real POSIX types (`DIR *`, `struct
dirent`, `fork`/`execvp`/`waitpid`) that don't fit Plum's extern
surface. **Handle-based, multi-call return** — the same pattern
sockets' fds and `.accept()` loops already established: `dir_open`/
`process_run` return an opaque `Int`; further calls (`dir_read_next`,
`process_exit_code`/`process_stdout_data`/`process_stderr_data`) read
results back out. This is what makes a "run a process, get back THREE
things (exit code/stdout/stderr)" operation possible at all — the
extern surface still has no multi-value return.

**A real refactor forced by scale, done proactively**: the "compile a
shim, force-keep its symbols against `--gc-sections`, export them to
`.dynsym`, write its source into every native `clang` invocation"
recipe was hand-duplicated per shim across SIX call sites (`plum-
interp`/`plumc`'s own `build.rs`, and FOUR independent `clang`
invocations in `codegen_cli.rs`). Adding a 2nd and 3rd shim in the SAME
pass as the 1st would have meant hand-duplicating it a 2nd/3rd time —
refactored FIRST, before adding the new shims, into a shared `native_
shims()` list (`build.rs`) / `ALL_NATIVE_SHIMS` + `write_native_shims`
(`codegen_cli.rs`) that every call site loops over. The three lists
still have to be kept in sync by hand (a `build.rs` can't easily share
code across crates, and `codegen_cli.rs` embeds via `include_str!` at
compile time) — an accepted duplication, not fully eliminated, but
reduced from "N call sites × M shims" hand-written blocks to "N call
sites, one loop each."

**Directory listing**: `list_dir(path): Result[Array[String], String]`
(entry NAMES only, `.`/`..` already skipped by the shim) and
`is_directory(path): Result[Bool, String]` (a real three-way `stat`
outcome under the hood — nonexistent path is `Err`, not silently
folded into `Ok(false)`). `dir_read_all_acc`'s tail-recursive
accumulator is the SAME "no `while` loop, read until empty" idiom every
other stdlib "read until done" function already uses.

**Subprocess exec**: `run_process(program, args): Result[ProcessResult,
String]`, `ProcessResult { exit_code, stdout, stderr }`. A non-zero
CHILD exit code is an ordinary, successful `Ok` (a failing compile is
routine for a compiler-shaped caller to inspect) — `Err` means the
process could never even be STARTED. **Captures stdout/stderr via TEMP
FILES, not pipes** — deliberately, to sidestep the classic pipe-
deadlock class of bug (`fork`+pipe+`waitpid`-before-draining can
deadlock if a child writes more than the pipe's kernel buffer before
anyone reads it; a temp file has no such buffer, so there's no writer/
reader ordering dependency at all). **Arguments joined with a TAB
separator, not a rarer control byte** (the natural choice, e.g. ASCII
Unit Separator `0x1F`, would have been genuinely safer against a real
argument containing it) — forced by what Plum's own string-literal
lexer can actually express (`\n`/`\t`/`\r`/`\\`/`\"` only, no `\xNN`
hex escape); a real, honest trade, not the ideal choice, same spirit as
`CStr`'s NUL-truncation.

**A real, separate bug found while testing, NOT fixed here — filed
instead.** A top-level GLOBAL `let` (e.g. `let g = "hello"`) used
TWICE, where at least one use goes through a heap-consuming operation
like `.as_cstr()`, corrupts under NATIVE codegen: confirmed with a
minimal repro entirely unrelated to this chunk's own new code (`extern
"C" { fn strlen(s: CStr) -> Int; } let g = "hello" let main (): Int =
unsafe { let a = strlen(g.as_cstr()); let b = strlen(g.as_cstr()); a +
b }` fails with \"embedded null byte\" — the SAME symptom class the
`tcp_write` use-after-free earlier this session had). Root cause,
diagnosed but not yet fixed: `plum-ir::fbip`'s last-use analysis
(`insert_refcount_ops`) reasons about heap-value ownership PER FUNCTION
BODY, keyed by locally-scoped variable names — a reference to a GLOBAL
inside a function body isn't in that function's own `known_heap`
tracking, so no protective refcount `Inc` is ever inserted before a
consuming use, and `.as_cstr()`'s own unconditional decrement (see its
doc comment: \"the ONLY place that ever discharges the incoming Str's
refcount ownership\") frees the shared global cell on its FIRST use
anywhere in the whole program, corrupting every subsequent reference.
This chunk's OWN new stdlib code and tests are unaffected (every
`list_dir`/`is_directory` call site uses a LOCAL `let`, verified
directly, both by the fix working when switched from global to local
and by dedicated tests exercising exactly this double-use shape with a
local). A real, standalone bug — filed on the roadmap, not fixed in
this pass; **fixed in a following pass, same session — see "`.as_cstr
()` use-after-free on untracked variables — Decided and fixed" below**
for the real fix (which turned out to be broader than "globals": a
plain function PARAMETER has the identical bug, found while root-
causing this one).

Verified with real compile-and-run tests in BOTH backends: a real temp
directory (multiple files + a subdirectory) for `list_dir`/`is_
directory`, and a real `echo`/`sh -c "exit N"`/nonexistent-program
subprocess invocation for `run_process`, including the routine-vs-
Err distinction (nonzero exit code is `Ok`; a nonexistent program still
starts and exits 127 via the shim's own `_exit(127)`, also `Ok` — `Err`
is reserved for a genuine failure to even fork/create the capture temp
files, not exercised by these tests since it's not realistically
triggerable in a unit test).

### `.as_cstr()` use-after-free on untracked variables — Decided and fixed (2026-08-11)

The real fix for the bug filed in the "OS module" section above —
turned out to be broader than that section's own framing ("globals").

**Root cause, precisely**: `plum-ir::fbip`'s last-use analysis
(`insert_refcount_ops`/`transform`) only tracks a name as heap-shaped
(`known_heap`) when it's bound by a real `let` to a directly-provable-
heap value (a `Ctor`, a `Str`/`EmptyArray` literal, or an alias of an
already-tracked name) — **by design**, documented as conservative scope
("without a type checker") since the pass predates real type info
reaching it. Two other kinds of names are NEVER added to `known_heap`,
also by design: function PARAMETERS (confirmed by an existing test's
own comment: "a bare free variable is exactly the 'unprotected
parameter' shape") and top-level GLOBALS (never touched by the per-
function `known_heap` set at all). For nearly every heap-consuming
operation in this compiler, that's harmless — `.concat()`'s `StrConcat`,
`.push()`'s `ArrayPush`, etc. never touch their operand's refcount
directly; ALL refcount management for them is delegated entirely to
whatever `RcAnnotated` `fbip` separately decides to insert, so an
untracked name just never gets an Inc OR a Dec — a leak, never a
use-after-free. **`.as_cstr()` is the one architectural exception**:
its own codegen (`codegen_as_cstr`) unconditionally decrements its
operand's refcount as part of producing the `CStr` copy — by design
(see its own doc comment: "the ONLY place that ever discharges the
incoming `Str`'s refcount ownership"), because SOMETHING has to
discharge it, and the reuse-preventing copy it makes means the fresh
`CStr` buffer can safely outlive the original regardless. That
unconditional Dec is only actually SAFE when `fbip` can guarantee this
was the value's true last use — a guarantee that only ever existed for
TRACKED names. For an untracked name (a parameter or a global), no such
guarantee exists, so `.as_cstr()` called on it — even ONCE, if the
value is used again afterward, or TWICE, as both this bug's real
repros and its regression tests do — frees a cell something else still
holds a live reference to.

Found via `list_dir`/`is_directory`, but proven to be nothing to do
with directory listing specifically: a minimal repro with a bare
top-level `let g = "hello"` and two `strlen(g.as_cstr())` calls
reproduces it with zero new stdlib code involved. Then, while narrowing
the repro down, found the SAME bug for a plain function PARAMETER too
(`let double_strlen (s: String): Int = unsafe { let a = strlen(s.
as_cstr()); let b = strlen(s.as_cstr()); a + b }`) — confirming the real
scope is "any untracked name," not "globals" specifically. Both
confirmed via a real libc `strlen` call over an ACTUAL corrupted
buffer (observed directly: `.as_cstr()`'s own embedded-NUL validation
correctly caught the corruption and aborted, rather than silently
returning a wrong answer — the SAME "silent, not a crash" character the
`println`/`tcp_write` UAFs of earlier this session had was, this time,
caught by an existing safety check instead of slipping through).

**The fix**: `fbip::transform`'s `AsCStr` arm now special-cases the
untracked case specifically — when `.as_cstr()`'s operand is a bare
`Var(name)` where `name` ISN'T in `known_heap`, wrap the whole thing in
a protective `RcAnnotated::Inc` on `name` first. `.as_cstr()`'s own
guaranteed `Dec` then just cancels that back out — net zero effect on
the value's REAL refcount — turning what would otherwise be an
unconditional consume into an ordinary, safe borrow-and-copy. A TRACKED
name needs no such protection (the EXISTING `mark_last_uses` machinery
already proves whether a consume is genuinely safe there, and inserts
its own `Inc` first if it isn't — confirmed this doesn't double up via
a dedicated test); a non-`Var` operand (a literal, an inline call
result) needs none either, since it's a fresh, unshared value by
construction with no name for a protective `Inc` to even target.

**A second, separate fix this surfaced**: `Expr::RcAnnotated`'s own
CODEGEN only ever resolved its `target` against `env` (locals/params) —
which is exactly why the parameter case started working the moment the
`fbip` fix landed, but the GLOBAL case still failed with "unbound
variable" until `RcAnnotated`'s codegen arm was ALSO extended to fall
back to the same `ctx.globals` resolution (a `load` of the already-
materialized `@global.{name}` slot) `Expr::Var`'s own codegen arm
already used. Two genuinely separate gaps, both real, both needed:
`fbip` didn't know an untracked-`Var`-as-`.as_cstr()`-operand needed
protecting at all; codegen's `RcAnnotated` didn't know how to apply
that protection to a global even once `fbip` asked it to.

Verified via the two SAME minimal repros that first found the bug (a
plain-parameter double-`.as_cstr()`, and a top-level-global double-
`.as_cstr()`), now passing correctly in both the interpreter AND native
codegen (the interpreter, it turned out, never actually had this bug —
its OWN `.as_cstr()` evaluation is a pure pass-through with no refcount
side effect at all, unlike native codegen's real malloc+memcpy+decrement
— confirmed directly rather than assumed, which is also why no
interpreter-side regression test was needed, only `plum-ir::fbip`'s own
unit tests plus two native `compile_and_run` regression tests). Full
workspace suite green, zero new clippy warnings.

### Hash-based Map/Set — Decided and implemented (2026-08-11): pure-Plum hash table on one new `String.hash` primitive

Picked directly out of the self-hosting discussion — `Map`/`Set` were
association-list based (`O(n)` per lookup, explicitly documented as
fine for small maps, not a performance-critical hash table), exactly
the workload a self-hosted compiler's own symbol tables/type
environments would be. Weighed against a true native collection type
(Array-scale effort — new heap layout, dedicated IR nodes, FBIP
awareness) and deliberately built the LIGHTER way instead: an ordinary
Plum stdlib rewrite (Array-of-buckets) on top of ONE new compiler
primitive, similar scope to the TCP/HTTP/OS work earlier this session,
not a multi-session undertaking.

**The one new primitive: `String.hash(s): Int`.** A REAL FNV-1a hash
(64-bit offset basis/prime), computed independently in BOTH backends —
the interpreter as a plain Rust loop (`fnv1a_hash`), native codegen as
a real hand-emitted LLVM phi-based loop (`codegen_str_hash`, mirroring
`codegen_for`'s existing loop-codegen idiom) — and cross-checked
against a THIRD, independent from-scratch Python FNV-1a implementation
before being trusted (not just the two Rust implementations agreeing
with each other, which could both share the same typo'd constant).
Always non-negative (top bit cleared) so a caller can `%` a bucket
count directly, no negative-modulo handling needed. Recognized via the
SAME shape-based-recognition precedent `Array.map`/`filter`/`fold`
already established (`Type.func(value)`, not a dot-call — hashing
isn't one of the small fixed set of zero-arg CONVERSIONS `.to_string()`
/`.as_cstr()`/etc. are) — a NEW `is_string_builtin_call` helper,
generalizing the existing `is_array_builtin_call` one, in both `infer.
rs` and `lower.rs`.

**A fully GENERIC structural hash (recursing into any type, mirroring
`ToString`'s own per-type dispatch) was considered and rejected as the
compiler-level primitive.** `.to_string()` ALREADY is a deterministic
structural representation for every type it supports — so `value_hash
[T](x: T): Int = String.hash(x.to_string())`, written entirely in the
PRELUDE on top of this one `String`-only primitive, gets the same type
coverage for free (confirmed directly: a `Map` keyed by a real struct
already worked, via `.to_string()`'s existing recursive struct
rendering, with zero extra code). No new recursive per-type codegen
needed at all — `ToString`'s own native-codegen implementation
required a substantial amount of per-shape specialized code generation
(`render_word_as_string`, per-tag/per-array-elem-type functions); a
hash primitive with the SAME breadth, built the SAME way, would have
been a comparably large undertaking for no real benefit once `.to_
string()` can already be reused as the structural basis.

**The hash table itself, pure Plum**: `struct Map[K, V] { buckets:
Array[Array[MapEntry[K, V]]], size: Int }`, `MapEntry[K, V] { key: K,
value: V }`. Starts at 8 buckets, resizes (doubling) whenever `size *
4 > buckets.len() * 3` (a 0.75 load factor, integer arithmetic —
comparing cross-multiplied products avoids needing float division for
something this simple). `Set[T]` is a thin wrapper around `Map[T,
Unit]` (`struct Set[T] { inner: Map[T, Unit] }`), not a second parallel
bucket implementation — halves the amount of new stdlib code needed,
`Set.to_array` is literally `Map.keys`.

**A real recursion-depth bug found and fixed WHILE building this
(not shipped, caught before landing)**: the very first draft of `map_
make_buckets` (building the initial `Array[Array[MapEntry[K,V]]]` of N
empty buckets) was written recursively, matching the \"no `while` loop,
recurse instead\" idiom this whole stdlib otherwise uses — and hit the
interpreter's own well-documented non-tail-call-optimized recursion-
depth ceiling at a mere ~256 levels (triggered by growing past 128
buckets during a real stress test), overflowing even the DEFAULT thread
stack. Confirmed by bisecting the exact entry count that broke (95
worked, 99 didn't — precisely the point a real resize, 128 -> 256
buckets, first fires). Fixed by rewriting `map_make_buckets` with a
`for` loop instead — `for` loops are real ITERATION, proven not to grow
the interpreter's Rust call stack per iteration regardless of count (a
1000-entry stress test, `for`-loop-based throughout, passes on the
interpreter's completely default, un-boosted stack). A genuinely useful
data point for anything ELSE built on `for i in a..b`/`for x in arr`
going forward: prefer it over a hand-written recursive accumulator
whenever the iteration count could meaningfully grow, not just for
style — it's a REAL, not cosmetic, difference under this interpreter.

**A genuine semantic change from the old implementation, decided
explicitly, not silently**: the old linked-list `Map`'s `insert`
PREPENDED rather than overwrote — a repeated key left BOTH values in
the structure (newest shadows oldest; `remove` uncovered the older one;
`len` counted entries, not unique keys) — extensively tested as
deliberate-looking behavior, but really just an accidental byproduct of
the simplest possible linked-list `insert`. Brad confirmed switching to
standard overwrite-on-insert semantics (unique keys, `len` = key count)
for the new hash table — matches every mainstream language's map, and
is almost certainly what any real caller actually wants. The three
tests that specifically probed the old shadow behavior were rewritten
to assert the new, correct one, not deleted — regression coverage for
the NEW contract, not a gap.

Verified with real compile-and-run tests in both backends: basic
insert/get/contains/remove, overwrite-not-shadow semantics, `len` =
unique-key-count, `Set` dedup/union/intersection/difference, struct-
keyed maps (exercising `value_hash`'s reuse of `.to_string()`'s
existing recursive struct rendering), and a real 1000-entry stress test
crossing multiple resize boundaries with every key individually
verified afterward — not just "didn't crash."

### `?`/early-return sugar — Decided (2026-08-12): not built; adopted pipe + `Result.and_then`/`Result.map` as house style instead

Raised as the last remaining self-hosting "should-have" from the
earlier roadmap discussion. Brad was skeptical going in — right to be.

**Why `?` isn't just sugar over `match` here.** Checked directly: Plum
has NO `return` statement/keyword at all. Every function body is a
single expression flowing to its value (`if`/`match`/blocks are all
expressions) — `?` fundamentally means "stop evaluating this function
and produce a value right now, from the middle of an expression tree,"
which is a genuinely new kind of control flow, not a desugaring of
something that already exists. Two more real costs, not just the
`return` gap: (1) Rust's `?` is pleasant specifically because it auto-
converts the error type via `From` — Plum's ad-hoc-polymorphism story
is deliberately closed (`Num`/`Eq`/`Show` only, confirmed directly via
`satisfies_bound`, no user-extensible conversion trait), so `?` here
would only work cleanly when every fallible call in a function already
agrees on the exact same error type; (2) an early return needs to
release any live local heap values at that exit point, a genuinely NEW
exit path `fbip`'s last-use analysis (which currently only ever sees
one exit, the function's tail) has never had to reason about.

**Decided instead: adopt pipe + `Result.and_then`/`Result.map` as the
house style for straight-line Result chains**, which already existed
(no new language work at all) and get most of the real readability
win. Proven by rewriting a real, already-shipped chain BOTH ways and
running both against the REAL counterpart before choosing — not just
theorized:

```
// Before (nested match, 12 lines):
let http_do_request (fd: Int) ... : Result[HttpResponse, String] =
    match tcp_write(...) {
        Err(e) => Err(e),
        Ok(_) => match http_recv_headers_acc(fd, \"\") {
            Err(e) => Err(e),
            Ok(raw) => match http_parse_head(raw) { ... },
        },
    }

// After (pipe + and_then, 4 lines) — SHIPPED:
let http_do_request (fd: Int) ... : Result[HttpResponse, String] =
    tcp_write(fd, http_build_request(method, parsed, headers, body))
        |> Result.and_then(_, |ignored| http_recv_headers_acc(fd, \"\"))
        |> Result.and_then(_, http_parse_head)
        |> Result.and_then(_, |head| Result.map(http_read_body(fd, head.headers, head.leftover_body), |response_body| HttpResponse { status: head.status, headers: head.headers, body: response_body }))
```

Verified by writing `_v2` alternates of `http_do_request`/`http_
request`/`http_parse_head`/`http_parse_request_head`/`http_handle_
connection` calling the SAME already-shipped underlying helpers,
running the CLIENT rewrite against the REAL, unmodified server and the
SERVER rewrite against the REAL, unmodified client (both directions,
both backends) — proving each rewrite independently correct against a
known-good counterpart, not just \"compiles.\" Then applied to the real
prelude source; the FULL existing HTTP test suite (unchanged) passed
against the rewritten implementation with zero modifications needed.

**The honest limit found along the way, not glossed over**: `Result.
and_then`-chaining loses access to earlier bound values once you move
past them — `http_do_request`'s last step needs BOTH `head` (from step
3) and `response_body` (step 4's own result), so the chain can't stay
FLAT; it needs one level of closure nesting so `head` stays in scope
via capture. `http_request` (needing `fd` alive for cleanup regardless
of outcome) and `http_handle_connection` (needing `head` alive across
two more steps) hit the same thing, one level deeper each. Real local
variables from a genuine `?`/early-return wouldn't have this problem —
this is exactly the shape of chain `?` is actually good at that the
combinator style doesn't fully replace. Not built anyway: none of the
chains that existed needed MORE than one level of this, and the
version with it stayed clearly more readable than the original nested
`match`. Revisit if real self-hosting work surfaces a chain where this
actually bites — not before.

### Contracts (`require`/`ensure`) — Decided and implemented (2026-08-12)

Raised by Brad as a specific question about adopting C3's contracts
feature (`<* @require ... @ensure ... *>`), while explicitly open to
other C3 ideas too. Surveyed the rest of C3's feature set first —
faults/optionals (`Option`/`Result` already cover this), macros (real
compile-time-metaprogramming scope, no self-hosting payoff proportional
to the cost), `@pure` (would need an actual purity/effects analysis;
"Effect/unsafe tracking" above already deliberately drew that line at
just the FFI trust boundary), distinct types (already expressible today
as a one-field struct, a dedicated `distinct` keyword would need real
nominal-conversion machinery to pay for itself) — none of those cleared
the bar. Contracts did: genuinely cheap (pure sugar over machinery that
already existed) and genuinely valuable (self-documenting function
boundaries, more valuable the more a self-hosted compiler leans on
itself).

**The key realization that made this cheap**: `panic_raw`/`assert` (the
"Testing framework" section above) already IS what a contract check
compiles down to. A `require`/`ensure` clause is nothing but an
`assert`-shaped call spliced into the function body — no new IR node,
no backend work, no new runtime semantics. Exactly the same shape
decision as string interpolation ("lexer+parser only, zero IR/backend
changes") and the polar opposite of `?`/early-return sugar just above
(which got rejected specifically because it needed a genuinely new
control-flow primitive `fbip` had never had to reason about). Contracts
don't need that: `require` clauses are ordinary statements prepended to
the body; `ensure` clauses just need to see the body's own return value
before it returns, which a single injected `let` already gives for
free.

**Grammar** — added to `LetDef` between the return-type annotation and
`=` (GRAMMAR.md's own "Contracts" note has the formal rule):

```
let divide (a: Int) (b: Int): Int
  require b != 0 : "b must be non-zero"
  ensure result >= 0
= a / b
```

Fixed order (every `require` before any `ensure` — interleaving is a
clear parse error), each clause optionally taking a `: "message"`
suffix appended to a generic base message
(`"precondition failed: b must be non-zero"` vs. bare
`"precondition failed"`) — cheap to parse, and a real step up from
`assert`'s own generic "assertion failed" with no indication of WHICH
condition fired or why. Brad confirmed including this in v1 rather than
deferring it.

**`require`/`ensure` are contextual keywords, not reserved words** —
`Parser::peek_is_contextual_kw` checks token TEXT, not a new
`TokenKind`. Safe and unambiguous specifically because the only other
legal token in that exact grammar slot (right after a `LetDef`'s
params/ret_ty) is `=` — no existing valid program could have an
identifier there already, so recognizing `require`/`ensure` by text
costs nothing and regresses nothing. The payoff: `let require = 5`
stays perfectly ordinary Plum everywhere else in the language, unlike
adding these to the `unsafe`/`spawn`-style hard keyword list would have
meant. A new regression test pins this directly.

**Desugaring** (`Parser::desugar_contracts`, entirely inside
`parse_let_def` — `LetDef`'s own struct shape doesn't change at all, so
nothing downstream of the parser needs to know contracts exist):

```
let f (a: Int) (b: Int): Int
  require b != 0 : "b must be non-zero"
  ensure result >= 0
= a / b
```
becomes, before `plum-types`/`plum-ir` ever see it:
```
let f (a: Int) (b: Int): Int = {
  __contract_require(b != 0, "precondition failed: b must be non-zero");
  let result = a / b;
  __contract_ensure(result >= 0, "postcondition failed");
  result
}
```
`__contract_require`/`__contract_ensure` are two new one-line prelude
functions appended directly to the existing `STDLIB_ASSERT_SRC` const
(same file, same category, and — since `PRELUDE_TOTAL_LEN`'s span-offset
math derives from `.len()` on that same const — zero other wiring
needed anywhere `with_prelude`'s fragment list is threaded through).

**Two deliberate design decisions, both scoped via `AskUserQuestion`
before writing any code** (per the project's own established practice
of discussing new checks before building them solo):

1. **Custom `: "message"` syntax — included in v1** (the alternative was
   shipping message-less clauses now, adding text later). Worth the
   small grammar cost for meaningfully better failure output.
2. **A parameter literally named `result` + an `ensure` clause is a
   parse-time error, not silent shadowing.** The injected `let result =
   …` would otherwise make a same-named parameter invisible for the
   rest of the body with zero warning — `desugar_contracts` checks for
   this and rejects with a clear message before any lowering runs.
   `require`-only functions are unaffected (no `result` binding gets
   injected when there are no `ensure` clauses at all) — pinned by its
   own regression test.

**The one real, inherent trade-off, flagged explicitly rather than
found by surprise later**: an `ensure` clause costs that function's own
tail-call optimization. A postcondition has to intercept the return
value before returning it — `let result = BODY; check(result); result`
moves whatever tail call `BODY` had out of tail position. This isn't a
Plum bug; it's inherent to postconditions in any language (Eiffel/
Dafny have the identical property) — but worth calling out loudly given
recursion-depth limits have genuinely bitten this project more than
once already (the HTTP client's non-TCO interpreter recursion, the
`map_make_buckets` stack-overflow bug). `require` alone costs nothing
here: `desugar_contracts` deliberately keeps the original body in TAIL
position (not a statement) when there are no `ensure` clauses, so a
`require`-only function's own tail-call shape is preserved exactly.
Verified directly, not just reasoned about: a `require`-only function
tail-recursing 2,000,000 times compiled and ran to completion under
native codegen with no stack growth, identical to the same function
with no contract at all.

**Verified end-to-end**, not just via parser unit tests: real
`plum run`/`plum build` round trips through a temp project — a
precondition violation (`divide(1, 0)`), a postcondition violation
(`ensure result > 100` on a function that returns `1`), and the
parameter/`result` collision error — all producing the expected,
correctly-labeled failure, in BOTH backends. 10 new `plum-syntax`
parser tests (contract-free bodies unchanged, `require`-only preserves
tail position, `ensure`-only binds+yields `result`, both together,
multiple `require`s, `require`/`ensure` staying usable as ordinary
identifiers elsewhere, the interleaving-order error, the `result`-
collision error). Full workspace suite (`cargo test --workspace`) green
throughout, zero regressions.

**Explicitly out of scope for v1** (both flagged up front, not
discovered as gaps later):
- **`old(x)`** (pre-call snapshots of mutable arguments, for `ensure`
  clauses that need to reason about a value BEFORE the call) — no real
  use case in the stdlib yet to motivate the extra machinery (capturing
  a copy at function entry before the body runs). Revisit if one shows
  up.
- **Release-mode stripping** of contract checks — there's no debug/
  release build distinction in the compiler at all today, so nothing
  exists yet to strip against. Also revisit together if that
  distinction is ever added.

**`examples/contracts/main.plum` added (2026-08-12)**, same style as
the rest of `examples/` — an `Account.withdraw` with two `require`
clauses (one with a custom message) and one `ensure`, plus an
`average` guarded against division by zero, contrasted directly in
comments against `option_result`'s `Result`-based handling of the
"same kind" of failure (contracts are for invariants a caller should
never violate; `Result` is still right for genuinely expected
failure). Verified via both `plum run` and `plum build`; the two
commented-out violation lines were independently confirmed to produce
exactly the messages their comments claim. Listed in README.md's
Examples section.

### Currying (partial application) — Decided and implemented (2026-08-12)

Raised by Brad directly, prompted by the contracts conversation above:
multi-param function definitions (`let divide (a: Int) (b: Int) = ...`)
already LOOK curried — the "Surface syntax" section (way above) is
explicit that this was a deliberate cosmetic borrowing from OCaml/F#'s
`let f (a: int) (b: int) = ...`, while "Deliberately deferred, not
adopted" explicitly punted on making it REAL, citing "a fully-applied
direct-call fast path plus a closure-allocating path for partial
application, touching the calling convention." Brad's question: does
that deferral still make sense, or should it be revisited now?

**Scoped and confirmed via `AskUserQuestion` before writing any code**
(per this project's established practice for anything touching a new
static-checking behavior): build real partial application at CALL
SITES, leaving function `Type::Function`'s representation and function
DEFINITION syntax completely untouched; and keep `f()` on a multi-param
`f` meaning exactly what it means today (never a vacuous "give me `f`
back" 0-of-N partial application).

**The architecture turned out to already be halfway there.**
`plum-codegen::codegen_call` already has a two-tier calling convention,
for an unrelated reason (functions are already first-class values): a
DIRECT fast path (a bare identifier naming a known top-level function
compiles to a real LLVM `call @name(...)`, even `musttail` when
eligible) and an INDIRECT path (anything else — a closure value, a
variable holding a function — loads a code pointer out of a closure
cell and calls through it). That means the runtime representation for
"a function value waiting for more arguments" — a closure — already
existed, already refcounted correctly by `fbip`, and already understood
by monomorphization. **This is why currying didn't need a new IR node,
a `Type::Function` representation change, or ANY new codegen at all**:
it's built as "infer under-application as valid, producing a residual
function type" + "lowering rewrites the under-applied call into an
ORDINARY `Expr::Closure`" — everything downstream (codegen, `fbip`,
monomorphization) already knows how to handle a closure, zero new cases
needed anywhere.

**A real correction made mid-design, not glossed over**: the obvious
worry going in was that real currying would turn DESIGN.md's own
documented footgun (`sum (n - 1) (acc + n)` — juxtaposed single-arg
calls, missing a comma, currently a compile error since Plum has no
currying) into something silently wrong instead of loudly rejected.
Working through the actual semantics of a CORRECTLY-threaded partial
application shows the opposite: `sum(n - 1)` produces a closure over
sum's remaining parameter whose body calls `sum(n - 1, acc)`; calling
that closure with `(acc + n)` supplies the missing slot. Net effect:
`sum(n - 1)(acc + n)` and `sum(n - 1, acc + n)` become PROVABLY
IDENTICAL — the well-known ML property (`f(a)(b) === f(a, b)`) — not a
new footgun, a *resolution* of the old one (the ambiguity that made it
a footgun dissolves once both parses mean the same thing). Verified
directly, not just argued in prose: `plum-types::infer`'s own
`chained_partial_application_calls_equal_one_fully_applied_call` test,
and its real compile-and-run counterpart in `plumc::codegen_cli`.

**Implementation, three layers:**

1. **`plum-types::infer` (`infer_call_with_callee`)**: the existing
   exact-arity `unify` attempt is tried FIRST, unchanged; only on ITS
   failure does a new fallback try `try_partial_application` — unifies
   each SUPPLIED argument against the callee's own param list positionally
   (stopping short, not the full list), and returns the REMAINING
   (unsupplied) param types as a residual `Type::Function`. Two hard
   gates, both deliberate: `args.is_empty()` never takes this path (the
   confirmed "f() stays f()" scope decision), and the callee's type must
   ALREADY be a concrete `Function` (never for a still-unconstrained
   callee — there's no "right" residual shape to infer from a bare type
   variable). A genuine type mismatch on a supplied argument (not an
   arity issue) surfaces its own real error, not a misleading "arity
   mismatch" one. New `Infer::partial_calls: HashMap<Span, usize>`
   records WHICH call sites took this path (residual param count) —
   lowering's stable, span-keyed handle back to "rewrite me," the same
   `field_owners`/`unit_sugar_calls` precedent. The residual param/
   return TYPES themselves are recorded into the ALREADY-EXISTING
   `closure_types` map (keyed by the call's own span — there's no
   closure-literal span to key by, which is fine, `closure_types` was
   never actually required to be one) rather than inventing a parallel
   side-channel, since a partial application's residual shape genuinely
   IS a closure's shape.
2. **`plum-ir::lower`**: the ordinary `Call`-lowering arm checks `ctx.
   partial_calls` first; a hit rewrites the node into `Expr::Closure`
   with synthetic params (`__partial_arg0`, ...) whose body is an
   ordinary, FULLY-applied `Call` to the original callee (supplied args
   followed by the synthetic params as `Var`s) — mirroring the EXISTING
   bare-variant-reference eta-expansion in the very same function
   almost exactly (`Circle` alone already lowers to a synthesized
   `Closure` wrapping `Ctor`; this is the identical idiom one level up).
   Param/return types come from `closure_types` via the SAME `type_
   contains_param` filter the real closure-literal case already applies
   — `None`/`None` (harmless everywhere except native codegen, which
   requires `Some`/`Some` for any closure) falls out for free whenever
   a partial-application site sits inside a still-generic function's own
   body, since that's exactly the shape `closure_types` was never given
   a tier-2 template-fallback for (see point 3).
3. **`plum_ir::monomorphize::plan`**: `MonoPlan::functions` re-lowers
   EVERY function through its OWN `base_lctx`, not the caller's already-
   lowered output — so `partial_calls` needed threading through here
   too (a new parameter, `.with_partial_calls(partial_calls.clone())`
   on `base_lctx`), or partial application would silently break for
   every function compiled natively, not just ones inside generic
   bodies. Threaded as a FLAT pass-through, mirroring `unit_sugar_
   calls`'s (simpler) precedent rather than `closure_types`'/`empty_
   array_elem_types`'s per-instantiation MERGE logic.

**Deliberate v1 scope cut, stated up front rather than found as a
gap**: a partial-application call site written INSIDE a still-generic
function's own body has no tier-2 template-fallback resolution (unlike
`closure_types`/`empty_array_elem_types`, which each gained one after a
real reported bug — see this file's "Chunk 5" and the OS-module
section). This produces a CLEAR native-codegen error ("closure literal
it can't resolve static param/return types for"), never a silent
miscompile — the interpreter is entirely unaffected (dynamically typed,
never needs static closure types at all). Revisit if a real generic-body
partial-application need shows up, the same "ship the common case,
close the generic gap later" precedent `empty_array_elem_types` itself
already set.

**Verified**: 8 new `plum-types::infer` unit tests (residual type
inference, full application unaffected, the chained-equals-fully-
applied proof, argument type-checking still enforced, the `f()`/
over-application/unresolved-callee scope decisions all still error
exactly as before), 3 new `plum-ir::lower` unit tests (the closure
rewrite's exact shape, the `None`/`None` fallback, full calls
unaffected), and 4 new real compile-and-run `plumc::codegen_cli`
tests — under-application producing a working closure, the chained-
equals-fully-applied proof compiled and run for real (not just typed),
a partial application escaping its creating closure's own defunct stack
frame (mirroring an existing ordinary-closure escape-analysis stress
test exactly), and full application confirmed unaffected. Full
workspace suite green throughout, zero regressions. Also manually
verified end-to-end in both backends against a small standalone project
mixing all four shapes (full call, chained partial call, a partial
application bound to a variable and called later, and a partial
application nested inside an ordinary closure) — every value came out
correct.

**`examples/currying/main.plum` added (2026-08-12)**, same style as
the rest of `examples/` — a `scale(factor)(x)` demonstrating full
application, partial application bound to a variable, the chained-
call-equals-fully-applied-call proof, composing with `Array.map` as an
ordinary higher-order argument, and a partial application built inside
a closure and returned. Verified via both `plum run` and `plum build`.
Listed in README.md's Examples section.

## Self-hosting bootstrap corpus — Decided and implemented (2026-08-13)

Brad, after agreeing to start self-hosting (see the project roadmap),
raised a real worry before any Stage 1 code got written: build the
whole lexer/parser in Plum, discover partway through that some real
syntax construct was never exercised, and end up needing to rework
already-written self-hosted code to cover it — a genuinely expensive
way to find a gap. Proposed instead: build the comparison corpus
FIRST, entirely against the existing Rust implementation, so "did we
miss something" becomes a fast, already-answered question before Stage
1 starts, not a discovery made partway through it.

**Motivating evidence found while scoping this, not hypothetical**:
GRAMMAR.md itself had already drifted from the real parser — it
documented array literals as "intentionally absent, not yet decided"
(false; `ast::Expr::ArrayLiteral` has existed and been used throughout
`examples/` for a long time) and still described the `f (a) (b)`
footgun as a silent-wrongness trap "even though Plum doesn't have
currying" (stale as of yesterday's currying work). Both fixed in this
same pass. This is the concrete case FOR building a corpus straight
from the real parser's actual behavior rather than trusting prose
documentation to have kept up — the docs already hadn't.

**Three pieces, all reusable independent of Stage 1 ever starting:**

1. **`plum_syntax::render`** — the s-expression AST printer (`(let
   double ((n:Int)) ->Int (* n 2))`) that already existed, but only as
   a private, test-only helper inside `parser.rs`'s own test module,
   scattered across several locations in that file. Promoted verbatim
   (same output, same function names, zero behavior change — the
   existing 175 `plum-syntax` tests all still pass unchanged, now via
   `use crate::render::*` instead of local definitions) into a real
   `pub mod`, because it now has a second real consumer beyond this
   crate's own tests. Deliberately NOT `Debug`-derived output: `Debug`
   includes exact byte-offset `Span`s, which would make every golden
   file brittle to irrelevant whitespace/comment reformatting in a
   fixture — this format drops spans entirely, so two semantically-
   identical parses always render identically.
2. **`plum dump-ast <file>`** (`plumc::main`) — a new CLI subcommand,
   parsing exactly one `.plum` FILE (no module resolution, no prelude
   injection, no `assoc_fns`/`nested_struct_update` AST rewriting —
   just this crate's real `Lexer`+`Parser` on precisely what's in the
   file) and printing its `render_program` output to stdout. This is
   what generated every golden file in the corpus below, and what a
   future self-hosted parser's own equivalent tool gets checked
   against.
3. **`bootstrap/corpus/`** — a NEW top-level directory (deliberately
   outside `crates/plum-syntax/`, since this corpus is a contract
   between implementations, not one crate's own fixtures — see
   `bootstrap/README.md`), 98 small, focused `<topic>/<name>.plum` +
   `<name>.expected` pairs, one isolated grammar construct per fixture
   rather than the big narrative demos `examples/` already has.
   Topics: literals (incl. string interpolation), let-defs (incl.
   generics/bounds/associated functions/destructuring params),
   structs (incl. nested field-update sugar), enums, types, expressions
   (incl. pipe/placeholder sugar, currying), control flow, closures,
   patterns (incl. or-patterns), contracts, extern blocks, use
   declarations, blocks, and concurrency (spawn/unsafe/select). Found
   and fixed 4 real fixture-authoring mistakes along the way by
   actually running them through the real parser rather than assuming
   correctness (extern fn syntax uses `->`, not `:`, for its return
   type; extern params are one flat list, never curried like ordinary
   functions; tuple type ANNOTATIONS aren't implemented — see
   GRAMMAR.md's own note) — exactly the kind of real, easy-to-get-wrong
   detail this corpus exists to pin down before Stage 1 has to
   rediscover it independently.

**`crates/plum-syntax/tests/golden.rs`** walks the corpus (via
`CARGO_MANIFEST_DIR`-relative pathing to the repo-root `bootstrap/`
directory) and asserts the real parser's output still matches every
golden — two jobs at once: an ordinary regression guard for THIS
parser today, and the corpus's own self-consistency check as fixtures
get added or edited over time. Verified the test genuinely catches
real mismatches (not just trivially passing): corrupted one golden
file by hand, confirmed a real, correctly-attributed failure, restored
it, confirmed green again. Full workspace suite green throughout, zero
regressions — 98/98 fixtures passing.

**Token-level goldens added the same session, right after, prompted by
scoping "what's next" (Stage 1 itself).** The AST-level corpus above
only gets a real signal once BOTH a self-hosted lexer AND parser exist
— a Stage-1 lexer bug would only surface once the parser was also far
enough along to expose it, and could easily be misattributed to the
wrong stage. Fixed by giving the lexer its own independent golden:
`plum_syntax::render::render_tokens` (a flat, space-separated, span-
free token list — `Let Ident("x") Eq Int(5)`, `Eof` deliberately
omitted as constant boilerplate) and `plum dump-tokens <file>`, exact
mirrors of `render_program`/`plum dump-ast`. Every existing fixture got
a second, paired `<name>.tokens` golden (98 generated, reusing the same
source files — no new fixtures needed, since lexing and parsing exist
on the same source either way), and `golden.rs` gained a second test
function validating them, verified the same way (a hand-corrupted
`.tokens` file caught, then restored). One real, deliberate design
choice inside `render_tokens`: it strips `InterpPart::Expr`'s embedded
`Span` (the one token payload that isn't already span-free) but
otherwise reuses `TokenKind`'s own derived `Debug` output directly for
every other variant (`Ident("x")`, `Int(5)`, bare `Let`/`LParen`/...) —
deliberately NOT hand-writing a match arm per token kind the way
`render_program` hand-writes one per AST node kind, since a `Debug`-
derived unit variant has no field-order/verbosity risk to hedge
against (that risk is real for STRUCT-shaped `Debug`, not empty tuple/
unit variants). Full workspace suite green throughout — 98/98 fixtures
now passing on BOTH goldens.

**What this deliberately doesn't cover**: type inference, lowering, or
codegen — this corpus tests the PARSER specifically (Stage 1's own
scope), not the whole pipeline. A fixture like the nested-field-update
sugar's golden shows the RAW, un-expanded `ship.position.x=nx` path
(that expansion is a `plumc`-level pass, `nested_struct_update`, which
never runs inside `plum dump-ast` at all) — correctly narrow, not an
oversight.

## Stage 1: self-hosted lexer — Decided and implemented (2026-08-13)

`bootstrap/self_host/lexer/main.plum` — the first real self-hosted
Plum source in this project, written to answer the bootstrap corpus's
own whole purpose: does a Plum lexer, written in Plum, actually
reproduce what the real Rust lexer produces, for real syntax, checked
against goldens generated entirely independently of this code. Mirrors
`crates/plum-syntax/src/lexer.rs::Lexer` structurally (same `Token`
variant set — prefixed `Tok`/enum-payload shape, not required to match
Rust's names textually, since `render_token` is hand-written and
decouples internal naming from golden-format text entirely; same
keyword table; same operator-lexing lookahead rules; same `${...}`
interpolation depth-tracking scan, including verbatim-copying a nested
double-quoted string's content). Built on the exact recursive `(chars:
Array[String], pos: Int) -> Result-shaped-record` idiom `json_parse`/
`String.parse_int` already established in the real stdlib — not a new
pattern, reused directly. `String.index_of`/`.parse_int`/`.parse_float`
called via the namespaced `Type.func(value, ...)` form (dot-sugar only
works for genuine compiler primitives like `.concat()`/`.push()`/`.len()`,
not stdlib-defined associated functions — a real, if minor, gotcha hit
immediately on the first run and fixed in seconds by the golden-comparison
tool itself, not discovered by inspection). `panic_raw`'s own `Unit`
static type (never `!`-typed, unlike Rust's `panic!`) meant the
"unexpected character" branches needed the same "sequence panic_raw,
then return a placeholder value of the right type" shape `assert`'s
own definition already established, not something new invented here.

**Two real, concrete findings, both found via the corpus/tooling
itself, not by inspection — exactly the payoff this whole exercise
was built for:**

1. **A genuine format bug in the golden generator itself**:
   `render_token_kind`'s fallback (`{other:?}`, Rust's derived `Debug`)
   forces a decimal point on every float (`1.0`), but `render_program`'s
   own `Expr::Float` arm already uses `Display` (`f.to_string()`, `1`)
   — and Plum's own `Float.to_string()` naturally produces the SAME
   `Display`-shaped output, with no reason any correct Plum
   implementation would ever reproduce Rust `Debug`'s forced-decimal
   quirk. Verified directly (a real `rustc`-compiled 4-line program,
   not assumed) that Rust's `Display`/`Debug` for `f64` genuinely
   differ this way before concluding which side was "wrong." Fixed by
   adding an explicit `TokenKind::Float` arm to `render_token_kind`
   (matching `render_program`'s existing convention instead of standing
   apart from it) and regenerating all 98 `.tokens` goldens — the
   corpus's OWN infrastructure had a real, if narrow, bug, caught
   before Stage 1 needed to work around it.
2. **A genuine, previously-undocumented interpreter scaling limit,
   distinct from every prior instance of "the interpreter's recursion
   depth" already on record**: `plum run` genuinely stack-overflowed
   on fixtures as small as 14 tokens (`{ let x = 5; x }`), while `plum
   build` compiled and ran the identical program and fixture
   correctly. Root-caused, not worked around: `render_token`/
   `lex_operator`/`keyword_or_ident` are each one wide `match`/`if`-
   chain (50+ arms for `render_token`), evaluated fresh per token —
   `plum-interp` is a tree-walking interpreter, so evaluating a deeply
   NESTED expression costs real Rust call-stack depth proportional to
   that expression's own AST nesting, not just proportional to
   explicit Plum-level recursive calls (every previously-documented
   instance of "interpreter recursion depth" in this file was about
   the LATTER). Across many tokens, each re-walking the same wide
   chains, that per-token cost compounds. Confirmed directly by
   building the exact same source natively and re-running the exact
   fixture that overflowed — passed immediately, no code change
   needed. Documented in `lexer/main.plum`'s own top comment: validate
   this program (and likely any future large self-hosted source with
   wide dispatch tables) via `plum build`, not `plum run`.

**Also found and fixed, smaller**: native codegen's `.map()`/`.filter()`
require a closure LITERAL written directly at the call site — passing
an already-named top-level function value (`Array.map(pieces,
render_interp_piece)`) fails monomorphization with a clear error, not
a silent miscompile. A real, existing v1 scope limit (not something to
fix in the compiler for this), worked around the intended way: wrap in
a literal (`Array.map(pieces, |p| render_interp_piece(p))`).

**Result**: 98/98 corpus fixtures pass, validated via the native
build. First real self-hosted Plum source in the project, and it
already justified the corpus-first sequencing twice over — one bug in
the corpus's own tooling, one genuine new interpreter limit — both
found before Stage 1 (the parser) had to rediscover either
independently.

## Stage 2: self-hosted parser — Decided and implemented (2026-08-13)

`bootstrap/self_host/parser/parser.plum` — mirrors `crates/plum-syntax/
src/parser.rs`'s `Parser` structurally: same recursive-descent shape,
same one-function-per-precedence-level expression grammar (loosest to
tightest — pipe, or, and, compare, range, add, mul, unary, postfix,
primary), same capitalization-based struct-literal/generic-
instantiation disambiguation, same `${...}` string-interpolation
desugaring (re-lexing/re-parsing each piece's raw text through THIS
SAME lexer/parser, recursively), same `require`/`ensure` contract
desugaring (DESIGN.md's "Contracts" section) done entirely at parse
time. Restructured `lexer/` from its own standalone project into a
proper library MODULE (`bootstrap/self_host/lexer/lexer.plum`, `pub`
on `Token`/`InterpPiece`/`tokenize`/`render_tokens`), with `bootstrap/
self_host/main.plum` as the one real project root (`use lexer; use
parser;`) dispatching to either stage by its first process arg
(`tokens`/`ast`) — real, filesystem-level proof that Go-style directory
modules (DESIGN.md's "Module system" section) work for exactly the
purpose they were designed for.

**Deliberate v1 scope cuts, stated up front**: no `no_struct_literal`
suppression (GRAMMAR.md's struct-literal-vs-block ambiguity in `if`/
`match` scrutinee position — checked directly, none of the 98 corpus
fixtures need it, a real but currently-inert gap versus the Rust
parser); no parse-error recovery or diagnostics (every "expected X"
failure is a bare `panic_raw`, matching `lexer/lexer.plum`'s own
"unexpected character" precedent — the corpus is 100% valid Plum by
construction, nothing here needs to parse malformed input gracefully
yet).

**A second genuinely new, previously-undiscovered compiler bug found
and FIXED, not just documented** (the lexer stage's two findings were
a golden-generator bug and a fully-worked-around interpreter limit;
this one is a real gap in `plum-types::infer` itself, fixed at its
root): a non-generic function calling a LATER-declared, non-generic
function and immediately doing FIELD ACCESS on its struct-typed return
value failed with "field access requires a struct value with a
statically known type" — the exact same constraint already documented
for GLOBALS (`a_function_can_do_field_access_on_a_struct_typed_global`,
above) turned out to have never been extended to ordinary FUNCTION-to-
FUNCTION calls: `infer_program`'s Phase 1 pre-declares every function's
signature with bare, disconnected fresh `Var`s for its return type,
REGARDLESS of whether it has an explicit annotation — the annotation
only gets unified into place once Phase 2 reaches that function's OWN
body, in file order. A genuinely CYCLIC call graph — exactly what a
recursive-descent parser's own expression grammar is (`parse_expr` ->
`parse_primary` -> `parse_block` -> `parse_expr`, forever) — means no
source reordering can route around this, unlike an ordinary forward-
reference in an acyclic program. **Root-caused via a minimal, isolated
repro before touching the real 90-function parser** (a 9-line
throwaway program reproducing the exact shape), confirmed the fix in
that repro FIRST, then confirmed it against the real file. **Fix**: a
non-generic function's explicitly annotated return type is now seeded
directly into Phase 1's placeholder (via the same `ast_type_to_type`
converter `extern` signatures already use) instead of an unrelated
fresh `Var` — purely additive and best-effort (falls back to today's
plain fresh `Var` on ANY resolution failure, matching `global_types_
early`'s own "opportunistic extra precision, never a source of truth"
philosophy exactly), so it can only make MORE well-typed programs infer
correctly, never change what an already-working program means. Scoped
to non-generic functions only — a generic function's return type may
mention a generic name with no `Var` minted for it yet (that minting
is per-function, inside Phase 2's own loop). New regression test,
`a_function_can_do_field_access_on_a_later_declared_functions_struct_
typed_return_value`. Full workspace suite green throughout — 446 tests
in `plum-types` alone (net +1), zero regressions.

**One remaining, narrower inference gap found, NOT fixed — worked
around in Plum source instead**: a bare `None` passed as an argument to
a self-recursive call, inside one branch of a 3-way `if`/`else if`/
`else` where sibling branches also recurse (each required to unify to
the same overall block-parsing result type), couldn't always have its
own `Option[T]` pinned down, even with the concrete-annotation fix
above and even though the callee's own parameter is concretely
annotated `Option[PExpr]`. A minimal isolated repro of the same general
shape (self-recursive call passing `None`) type-checked FINE — this is
narrower than the fixed bug, tied to the specific 3-way-branch-
unification shape, not chased further given the real fix above already
unblocked the actual work. Worked around with `no_tail(())`, a tiny
explicitly-annotated wrapper (`let no_tail (unit: Unit): Option[PExpr]
= None`) whose OWN return-type annotation pins the ambiguity before
`None` is ever passed anywhere — cheaper than a deeper compiler dive,
and the honest, narrower gap is documented in `parser.plum`'s own
comment at that exact call site rather than silently papered over.

**Result**: 98/98 corpus fixtures pass on BOTH the `tokens` and `ast`
entry points, validated via the native build (see `lexer/lexer.plum`'s
own top comment for why `plum run` isn't the validation path here
either — the exact same wide-`match`-per-node interpreter-stack cost
applies to `render_expr`/`lex_operator`-shaped code). Two self-hosted
Plum stages now exist and pass their full corpus; Stage 3 (type
checker? codegen? interpreter?) remains deliberately unscoped until a
real need narrows it down, matching this project's own "close one
phase fully before scoping the next" pattern throughout.

## Stage 3: self-hosted interpreter — Decided and implemented (2026-08-13)

Asked "what's next" after Stage 2; recommended and confirmed a minimal
self-hosted INTERPRETER over the much bigger type checker, since `plum-
interp` is dynamically typed at runtime and needs no static type
information to execute correctly — the smallest slice that gets self-
hosted Plum to actually RUN a program (lex -> parse -> run, all in
Plum), deferring Hindley-Milner to a later Stage 4.

**`bootstrap/self_host/interp/interp.plum`** walks `parser.PExpr`/
`parser.PPattern`/etc DIRECTLY — no lowering step, no IR, a genuine
simplification versus the real AST -> IR -> `plum-interp` pipeline,
deliberate for v1. A `Value` enum (`VInt`/`VFloat`/`VStr`/`VBool`/
`VUnit`/`VTuple`/`VStruct`/`VVariant`) carries its own shape at
runtime — struct/enum DECLARATIONS are never consulted at all (a
`VStruct`'s fields already carry their own NAMES, a `VVariant`'s tag is
just a string), so this interpreter needs no declaration-metadata table
a statically-typed backend would require. **Deliberate v1 scope
cuts, stated up front**: no closures-as-values (rules out `Array.map`/
higher-order functions; ordinary named-function calls including full
recursion work fully), no `Array`/indexing/generic-instantiation
syntax, no `spawn`/`select`/`unsafe`/extern FFI, no struct-literal
`..spread`. Every unsupported shape fails with a clear `panic_raw`,
never silent wrong behavior.

**`bootstrap/exec_corpus/`** — a new, separate corpus (token/AST dumps
can't validate an interpreter at all): 12 small runnable programs, each
`<name>/main.plum` + `<name>/expected.txt` (the real `plum run`'s
stdout, minus its own trailing return-value echo). Two originally-
planned fixtures (`match` with a guard, `match` with an or-pattern)
were DROPPED, not worked around — direct testing found the REAL Rust
interpreter itself can't run those exact shapes yet (pre-existing,
unrelated compiler limits: a guarded bare-identifier arm anywhere but
truly last, and any or-pattern, both rejected before this session's
own code ever ran), so there was no golden to validate against at all.

**Real bugs found while getting the first fixture to run, same
methodology as every prior stage — build, run against the corpus,
root-cause every real failure before moving on:**

1. **A genuine misunderstanding of `()` corrected immediately by a
   real failure, not assumed away**: `let main (): Unit = ...`'s own
   `()` parses as ONE param (the Unit pattern `ParamPatternTy(PTuple([]),
   None)`), not zero params — Plum's "every function takes exactly one
   argument" convention applies to `main` too. The interpreter's own
   param-name collection needed to treat the Unit pattern as "one
   param, contributing zero bound NAMES" (filtered via `Option[String]`,
   `None` for `()`) while still classifying the DECLARATION as a
   function, not a global, by the RAW (pre-filter) param count —
   matching `plum-types::infer_program`'s own Phase 1 rule exactly.
   Getting the classification wrong either way broke every single
   fixture identically (either "no top-level main found" or a
   "destructuring not supported" panic on `main` itself), a strong
   signal the bug was structural, not fixture-specific.
2. **Two more native-codegen pattern-lowering gaps, both distinct from
   the tuple-of-variant-patterns gap Stage 2 already knew to avoid**:
   a LITERAL nested inside a variant's own payload (`VBool(true)`,
   `EField(base, "to_string")`) also fails monomorphization with
   "lowering not yet implemented for this pattern shape nested inside
   another pattern" — confirmed via minimal isolated repros before
   rewriting the real file, exactly like Stage 2's own methodology.
   Worked around the same way throughout: bind to a plain identifier,
   follow up with an ordinary `==`/`if` check instead of a literal
   pattern. A bare zero-arg variant tag nested inside another variant's
   payload (`EBinary(BRange, lo_e, hi_e)`) was confirmed, via the same
   kind of isolated repro, to NOT hit this gap — the restriction is
   specifically about literal patterns, not all nested patterns.
3. **A real, substantial design gap in the interpreter's own env
   model, found via the ONE fixture DESIGN.md itself calls "the
   classic case" for local mutability** (`let mut` + `for`-loop
   accumulation): `total = total + i` inside a `for` loop body never
   reached the `println` after the loop, because the interpreter's
   environment is a purely functional `Array[EnvEntry]` — each `for`
   iteration re-derived its own env from the SAME pre-loop snapshot
   instead of threading forward, so every iteration's assignment was
   silently discarded once that iteration's own `eval_block` call
   returned. Scoped the fix deliberately narrower than a fully general
   env-threading rewrite (which would need EVERY `eval_expr` call to
   return `(Value, env)`, touching the entire file): a dedicated
   `eval_stmt_for_env` path, used ONLY by `eval_stmts`'s own `SExpr`
   arm, threads env across `for`-loop iterations AND back out to
   subsequent statements in the same block — the one shape DESIGN.md's
   own "classic case" needs. Explicitly documented as a real, narrower
   gap than the fully general fix: an `if`/`match`/bare `{ }` block
   used directly as a statement (not a `for` loop) that reassigns an
   outer name doesn't propagate that reassignment either — not
   exercised by any exec-corpus fixture, not chased further.

**Result**: 12/12 execution-corpus fixtures pass, validated via the
native build (`./sh run <file>`). Full workspace suite green throughout
— no Rust compiler changes needed this stage, unlike Stage 2. Three
self-hosted Plum stages now exist (lexer, parser, interpreter), each
validated against its own real corpus; Stage 4 (a type checker,
finally needed for static safety and eventual codegen) remains
deliberately unscoped until a real need narrows it down.

## Stage 4: self-hosted type checker — Decided and implemented (2026-08-13)

Asked directly whether to build the real thing (full Hindley-Milner
unification, as the actual compiler does) or a much smaller annotation
-driven checker; Brad chose full HM over the smaller recommendation.
Built it in validated layers — same discipline as every prior stage,
scaled up for a genuinely bigger piece: (1) `types.plum` — the `ITy`
representation, `Subst`, and `unify`, ported faithfully from `crates/
plum-types/src/types.rs`/`subst.rs`/`unify.rs`, INCLUDING `Subst::
compose`'s documented self-loop-avoidance fix (a real bug the real
compiler found via an actual 100,000+-frame stack overflow once — the
fix was ported deliberately, not rediscovered here the hard way);
verified in isolation (a throwaway smoke-test project checking
`unify`/`subst_apply`/`subst_compose` directly) before building
anything on top of it. (2) `context.plum` — struct/enum field/variant
templates + AST-annotation -> `ITy` resolution. (3) `infer.plum` — the
real inference engine: fresh type variables, a `TyEnv`, and `infer_
expr`/pattern-typing/whole-program checking.

**Two real, deliberate simplifications versus the real compiler, both
stated up front, that made "full Hindley-Milner in one session"
tractable at all**:

1. **Top-level function signatures must be fully annotated** (every
   param, and the return type). This sidesteps the real compiler's
   single hardest, most historically bug-prone piece — the whole
   reason `infer_program`'s Phase 1/Phase 2 split (fresh-var
   placeholders, unified with annotations once each body is checked)
   exists at all is to support signatures that might need to be
   INFERRED. When every signature is already concrete text, mutual
   recursion — even a genuinely CYCLIC call graph, exactly the shape
   this project's own recursive-descent parser has — "just works" for
   free: every signature is already in the environment before any body
   is checked, no unification-order dependency, no Phase split needed
   at all. Real unification/`Subst`/fresh variables are still fully
   exercised, just for expression bodies and unannotated LOCAL `let`s
   (`let mut total = 0`'s own type is genuinely inferred, not
   annotated), not signature bootstrapping.
2. **No `Scheme`/`generalize` machinery.** Plum's generics are always
   explicitly declared (`let f[T] (x: T): T = ...`), never discovered
   by generalizing an inferred type the way classic ML let-polymorphism
   does — a genuine, real difference between Plum's own generics story
   and textbook Hindley-Milner, not a cut corner invented for this
   checker. A generic function's own declared parameter names ARE its
   scheme; `context.plum`'s `instantiate_template` just substitutes
   fresh `ITVar`s for them at each call site.

**Error handling**: `fail_tc` (`panic_raw`), matching this bootstrap
effort's own established house style throughout every prior stage
(`lexer.plum`'s `unexpected_char`, `parser.plum`'s `fail`, `interp.
plum`'s `fail_interp`) — not the real compiler's `Result`-based
diagnostics. This checker can say "type-checks" or crash loudly
explaining why not; everything needed to validate real accept/reject
behavior, not to recover and keep checking past one error.

**A real bug found immediately by the FIRST real test failure, not by
inspection**: `Option.unwrap_or(tyenv_lookup(env, name), { fail_tc(...);
ITUnit })` — assignment to a `let mut` variable declared earlier in the
SAME block failed with "unbound name" every single time, even for
completely ordinary, valid code (`let mut total = 0; total = total +
1;`). Root cause: `Option.unwrap_or`'s fallback is an ordinary VALUE
argument (unlike `unwrap_or_else`'s closure), and Plum evaluates
function arguments eagerly, not lazily — so `fail_tc(...)` inside that
fallback position ran UNCONDITIONALLY before the call, regardless of
whether the lookup actually found anything. Fixed by using `match`
instead (which only evaluates the branch actually taken) — the
general lesson (any `unwrap_or`-shaped fallback with a side effect is
almost certainly a bug, `unwrap_or_else`/`match` are the correct
tools) generalizes to any future Plum code, self-hosted or otherwise.

**Validated two ways, since "prints `ok` for everything" and "actually
discriminates well-typed from ill-typed programs" are indistinguishable
without both**:
- `bootstrap/exec_corpus/` (reused from Stage 3, not rebuilt) — 11 of
  12 fixtures type-check successfully. The one exclusion,
  `tuples/main.plum`, is real and expected, not a bug: it uses an
  UNANNOTATED tuple-shaped parameter, and Plum has no tuple type-
  ANNOTATION syntax at all (only tuple values) — there's no way to
  satisfy this checker's own annotation requirement for that specific
  fixture, full stop.
- `bootstrap/typecheck_corpus/` (new) — 5 deliberately ill-typed
  programs (wrong return type, wrong argument type, mismatched `if`/
  `else` branches, an unbound variable, a wrong struct-field type),
  each independently CONFIRMED to be genuinely rejected by the real
  Plum compiler first, before being added — a fixture that happened to
  accidentally be valid Plum would prove nothing. All 5 correctly
  rejected by the self-hosted checker too, with real, specific error
  messages (`"function f: declared return type Int doesn't match body
  type Bool"`, not a generic failure).

**Result**: 11/12 `exec_corpus` fixtures accepted (1 real, documented
exclusion) + 5/5 `typecheck_corpus` fixtures correctly rejected,
validated via the native build. Full workspace suite green throughout
— no Rust compiler changes needed this stage. Four self-hosted Plum
stages now exist (lexer, parser, interpreter, type checker), each
validated against its own real corpus.

## Pipeline wiring: `./sh run` now type-checks before interpreting — Decided and implemented (2026-08-13)

Asked directly "what's next" after Stage 4; the four stages existed
only as four independently-invoked modes — `./sh run` never called the
type checker at all, unlike the real `plum run`, which always type-
checks first and never executes an ill-typed program. Small, well-
scoped fix: `run` mode now calls `typecheck.check_program` before
`interp.build_program_state`/`run_main`, matching the real pipeline's
own order exactly. `check` stays its own separate mode (useful for a
program whose `main` isn't meant to run yet). Result: 11 of 12
`exec_corpus` fixtures pass end to end unchanged; `tuples/` now fails
at the type-check step specifically (the same, real, already-documented
exclusion `./sh check` alone already had — Plum has no tuple type-
annotation syntax), confirmed to surface the identical error either
way, not some other crash. All 5 `typecheck_corpus` fixtures confirmed
to stop at the type-check step too, never reaching the interpreter.
Full 98/98 lexer/parser corpus reconfirmed unaffected.

Real self-hosting is still genuinely far off, stated plainly rather
than oversold: none of these four stages can process THEMSELVES yet —
`interp.plum`/`infer.plum` lean on closures and `Array.map`/`filter`/
`fold` throughout, both explicitly scoped OUT of both the interpreter
and the type checker so far. Asked whether to keep pushing toward true
self-hosting (extend interp/typecheck to cover closures + arrays, then
eventually build a real codegen backend — genuinely the biggest
remaining piece, a different KIND of problem than tree-walking
interpretation) or declare the 4-stage proof of concept complete and
return to other roadmap work (crypto). Brad chose to keep pushing.
Next concrete slice: closures-as-values in the interpreter first (the
single most-blocking gap — without it, neither `Array.map` nor any
other higher-order stdlib function is reachable at all), the type
checker to follow once the interpreter's own shape is proven.

## Closures-as-values in `interp.plum` and `infer.plum` — Decided and implemented (2026-08-13)

The blocking gap from the previous slice: neither self-hosted stage
could treat a closure literal as a real VALUE — bind it to a name, pass
it as an argument, store it in a struct field, return it. Both stages
closed this the same session, since the interpreter's own shape needed
proving before mirroring it in the type checker (as planned).

**`interp/interp.plum`**: a new `Value` case, `VClosure(Array[String],
parser.PExpr, Array[EnvEntry])` — params, unevaluated body, and a
CAPTURED environment snapshotted at the closure literal's own creation
site (genuine lexical scoping, not the call site — the one real
semantic difference from an ordinary top-level function, which
`call_fn` always gets a fresh, EMPTY environment for). `eval_call` was
restructured to check, in order: a LOCAL name bound to a `VClosure`
(checked FIRST, so a local shadows a same-named top-level function
correctly — a real gap in the OLD `eval_call`, which never consulted
`env` at all for its `EIdent` case); built-ins (`println`/contracts);
top-level functions; capitalized-name variant construction. `EField`
calls also gained a fallback: if the field name isn't `to_string`/
`concat`, evaluate the base, look for a struct field by that name, and
call it if it's a `VClosure` — enables the "interfaces via records of
closures" pattern (this file's own Reader/Writer story, "Interfaces/
traits for Reader/Writer-shaped abstractions" above) to actually run
under the self-hosted interpreter. Any other callee shape (an
immediately-invoked closure literal, a closure returned from a call/
`if`/`match`) falls to a general case: evaluate it, expect a
`VClosure`. `Some(VClosure(..))` as ONE nested pattern was deliberately
avoided (two separate `match`es instead) — consistent with this file's
established practice of routing around the native-codegen "pattern
nested inside another pattern" gap, even in a spot not directly
confirmed to hit it, since the existing examples of that gap are all
variant-nested-in-variant shapes too.

**`typecheck/infer.plum`**: `EClosure(params, body)` gets fresh type
variables for each param (no annotations exist on `|a, b| ...` syntax
at all), infers the body against them, and produces `ITFunction(param_
tys, body_ty)` — the closure's own type is pinned down entirely by how
it's later USED. `infer_call` mirrors the interpreter's own ordering
exactly: a local binding is checked first (`infer_closure_call` unifies
it against a freshly-built `ITFunction`, then checks each argument),
falling through to `infer_named_call` (the existing println/contract/
signature/variant path) only when the name isn't locally bound.
`infer_method_call` gained the same struct-field-closure fallback as
the interpreter's `EField` case.

**Validated against two new `exec_corpus` fixtures** (`closures/` —
returning a closure from a closure, passing one as a higher-order
argument, capturing an outer local; `closures_in_structs/` — the
record-of-closures dispatch pattern), generated against the REAL `plum
run` first, then confirmed byte-for-byte identical via `./sh run`. All
13 non-`tuples` `exec_corpus` fixtures and all 5 `typecheck_corpus`
rejections reconfirmed unaffected; full 98/98 lexer/parser corpus
reconfirmed unaffected too.

**One incidental but real fix along the way**: the installed `plum`
CLI (`~/.cargo/bin/plum`) was stale — built before this session's
uncommitted `plum-types::infer.rs` fix (the Phase 1 return-type-seeding
fix from Stage 2) — so it failed to build `bootstrap/self_host` at all
with an unrelated-looking error inside `parser.plum`. `cargo install
--path crates/plumc --force` fixed it. Not a new bug, just this
project's own documented "stale diagnostics" trap (a background build
finishing doesn't mean the *installed* binary reflects it) catching
itself.

Still not self-hosting: `Array`/`EIndex`/`EGenericInst` remain
unsupported in both stages, so `interp.plum`/`infer.plum` still can't
process themselves (both lean on `Array.map`/`filter`/`fold`
throughout). Arrays are the next real blocker; a codegen backend
remains the biggest piece after that.

## Arrays in `interp.plum` and `infer.plum` — Decided and implemented (2026-08-13)

The remaining blocker from the previous slice, closed the same session
("great, let's continue"). Both stages gained: `EArray`/`EIndex`
(array literals + indexing), the `.len()`/`.push(v)`/`.set(i, v)` dot-
call trio (arity-disambiguated exactly like the real compiler's own
`plum-ir::lower` — `len` takes zero args, `push` one, `set` two), and
`Array.map`/`filter`/`fold` as NAMESPACE calls (`Type.func(value, ...)`
syntax, matching the real language's own convention — `.map`/`.filter`/
`.fold` dot-call sugar was deliberately removed, see the language-
ergonomics chunk earlier in this doc).

**`interp/interp.plum`**: a new `Value::VArray(Array[Value])` case —
notably, this wraps a real HOST `Array[Value]`, since `interp.plum`
ITSELF is ordinary Plum source compiled by the real compiler. `.push`/
`.set`/`.len`/indexing/`Array.map`/`filter`/`fold` on the wrapped
`elems` all reuse the real, already-correct host `Array` primitives
directly — this interpreter never reimplements array semantics from
scratch, only wraps/unwraps `VArray`. The one real design wrinkle:
`Array.map(arr, f)` parses as `ECall(EField(EIdent("Array"), "map"),
[arr, f])` — `EField`'s `base` is the bare capitalized name `"Array"`
itself, which is a NAMESPACE, not a bound value. Evaluating it as an
ordinary expression would have silently "succeeded" the wrong way:
`eval_ident`'s own capitalized-unbound-name rule (any capitalized name
with no other binding is a zero-payload variant construction) would
happily produce a bogus `VVariant("Array", [])` instead of failing
loudly. Fixed by checking `EField(EIdent(ns), method)` BEFORE
evaluating `base` at all, routing `ns == "Array"` to a dedicated
`eval_array_ns_call`, mirroring the SAME check this file already had to
add for local-binding-vs-top-level-function dispatch in the closures
chunk just before this one. `eval_call`'s `EField` handling was
refactored into a standalone `eval_dot_call` to keep this dispatch
readable.

**`typecheck/infer.plum`**: `Array[T]` is represented as `ITStruct
("Array", [T])` — the EXACT same representation the real compiler's
own `ast_type_to_type` already uses (`Type::Struct("Array", vec![elem_
ty])`, `plum-types::infer.rs`), reused as-is rather than adding a
dedicated `ITy` case: every existing `ITStruct` mechanism (`unify`,
`subst_apply`, `instantiate_template`) already handles it correctly
with zero new type-system primitives. `context.plum`'s `resolve_named`
gained one new branch (`"Array" => ITStruct("Array", args)`) so
`Array[T]`-annotated top-level signatures resolve correctly too (this
checker requires full top-level annotations, so this was needed even
though `interp.plum` itself never consults annotations at all).
`infer_call` gained the identical `EField(EIdent(ns), method)`
namespace check as the interpreter, routing to `infer_array_map`/
`filter`/`fold` — each unifies the array argument against a fresh
`ITStruct("Array", [<fresh elem var>])`, then unifies the closure
argument against the appropriate `ITFunction` shape built from that
same element type (mirrors `infer_fn_call`'s own fresh-var-then-unify
shape). `infer_method_call` gained `.len()`/`.push()`/`.set()`, arity-
guarded the same way `interp.plum`'s own `eval_dot_call` is.

**Validated with one new `exec_corpus` fixture** (`arrays/` — literal
construction, indexing, `.len()`/`.push()`/`.set()`, `Array.map`/
`filter`/`fold`, and an empty-array literal), generated against the
REAL `plum run` first, then confirmed byte-for-byte identical via `./sh
run`. All prior fixtures reconfirmed unaffected: 14/15 `exec_corpus`
(the documented `tuples` exclusion still the only one), 5/5
`typecheck_corpus` rejections, 98/98 lexer/parser corpus, full
workspace suite green throughout — no Rust compiler changes needed this
stage either.

**Deliberate v1 scope cut, stated up front**: `Array.map`/`filter`/
`fold`'s function argument must evaluate to a `VClosure` — a bare named
top-level function passed by reference (`Array.map(xs, some_named_fn)`,
as opposed to a closure literal `Array.map(xs, |x| some_named_fn(x))`)
isn't a first-class `Value` in this interpreter at all yet (matches the
"only bare-name calls...are callable" scope this whole file already
has), so it fails the same way calling one as a local binding would.
Not exercised by any exec-corpus fixture. `EGenericInst` (explicit
turbofish-style generic instantiation) remains unsupported in both
stages too — nothing in either file consults declared generics at all
yet, and no fixture needs it.

Worth noting: `interp.plum`'s own `array_map`/`array_filter` helpers
call the REAL, HOST `Array.map`/`Array.filter` on the wrapped `elems`
— this file was already relying on host `Array.map`/`filter`/`fold`
throughout (`resolve_ann`, `instantiate_template`, etc.), so this isn't
new dogfooding, just one more ordinary use. Neither file can process
ITSELF yet (that needs a great deal more: generics, more of the
standard library surface, and eventually real codegen). Closures and
arrays — the two biggest blockers identified when this push toward
true self-hosting began — are both done now; a real codegen backend
remains the single biggest remaining piece.

## Array push scaling bug — partially fixed, real gap remains open (2026-08-14)

Attempting the "try self-interpretation first" experiment (running
`./sh check`/`./sh tokens` against the self-hosted lexer's OWN ~450-
line source, the smallest useful test of whether the four self-hosted
stages could ever process themselves) OOM-killed the compiled `sh`
process at **44–45 GB** — repeatedly, across several attempts, and
because that process shared the invoking terminal's cgroup, each OOM
kill took the entire terminal (and the Claude Code session running in
it) down with it, not just the one process. Confirmed via the kernel
OOM log each time, not assumed.

**Root cause, isolated via careful bisection under `ulimit`/`timeout`
guards**: `Array.push()`'s reuse-in-place path — the fast path taken
when the array being pushed to is uniquely owned — built a full CLONE
of the entire backing storage via `.to_vec()`/an exact-size `realloc`
BEFORE ever deciding whether reuse would even apply. So even the
"already uniquely owned, safe to mutate" case still paid an O(current
length) copy on every single `.push()` call, making any accumulation
loop O(n²) instead of O(n) — confirmed with a minimal isolated repro
(a plain `Array.push`-in-a-loop function, nothing else) showing
textbook quadratic memory growth (16,000 pushes → ~900MB peak, both
backends) purely from this, unrelated to anything array-CONTENT-
specific.

**Fixed, part 1 — `plum-interp` (the tree-walking interpreter)**:
`Heap::array_push_in_place`/`array_pop_in_place`/`array_set_in_place`/
`array_remove_in_place` (`crates/plum-interp/src/heap.rs`) mutate the
cell's own `Vec<Value>` directly via `&mut` access when the caller has
already confirmed unique ownership — reusing Rust's OWN `Vec`'s
amortized-doubling growth, zero hand-rolled capacity tracking needed on
this side. `Interpreter::eval`'s four `*Reuse` cases (`lib.rs`) now
check `refcount == 1` FIRST, then either call the new in-place method or
fall back to the original clone-then-allocate-fresh behavior for the
genuinely-shared case (unchanged, still correct — sharing requires a
copy, that part was never the bug). Verified with a new, deliberately
SAFE unit test (`array_push_in_place_reuses_the_same_cell_at_real_scale`)
that calls the heap API directly in a plain Rust loop — 20,000 pushes,
`alloc_count` staying at 1 the whole time — specifically avoiding
`Interpreter::eval`'s own separate, pre-existing, unrelated non-tail-
call-optimization limit (a real, different constraint this whole
project has always validated deep recursion around via native `plum
build`, never `plum run`). All 274 pre-existing `plum-interp` tests
still pass.

**Fixed, part 2 — native codegen (`plum-codegen`)**: array cells gained
a genuine THIRD header word, `capacity` (`{ refcount, len, capacity,
elements[capacity] }`, 24-byte header — up from `Ctor`'s shared 16-byte
one), so `.push()`'s reuse-in-place codegen can check whether there's
already room before ever calling `realloc`, and when it does need to
grow, doubles capacity (`select`-based `max(capacity*2, new_len)`)
instead of growing to the bare minimum — genuine O(1) amortized growth,
O(log n) reallocations total instead of n. `array_elem_byte_offset`/
`store_array_elem_static` (codegen.rs) are arrays' own dedicated
counterparts to `field_byte_offset`/`store_field_word` — ordinary
`Ctor` cells (structs/enums/tuples) are completely untouched, they have
no use for a capacity field since they never grow. `ArrayPopReuse`/
`ArrayRemoveReuse` simplified to skip `realloc` entirely (capacity only
ever needs to grow, never shrink). This is a genuinely wide, layout-
sensitive change — EVERY array-cell producer in the runtime needed its
element offsets updated (`plum_alloc_array` itself, the four `codegen_
array_*_fresh` functions, `codegen_array_literal`, and — found only by
enumerating every single `@plum_alloc_array(` call site directly, not
by guessing — `.runes()`, `.split()` (both its empty- and non-empty-
separator paths), `args()`'s own array-building loop, and the array
release/equality/to-string/deepcopy runtime-generated functions).
Verified two ways: the full existing `plum-codegen` (80 tests) and
`plumc` (493+27 tests) suites — covering array-of-structs equality/
to-string/deepcopy-under-spawn, nested heap elements, bounds checks,
etc. — all still pass unchanged (a wrong offset anywhere would have
shown up as wrong VALUES or a crash in one of these, not silent
success), plus one new dedicated regression test (`repeated_array_
push_reuse_grows_correctly_across_many_capacity_doublings`, 2,000
pushes crossing ~11 doubling boundaries, checking both final length AND
sum).

**NOT fixed — the actual pattern this project's own bootstrap code
uses, discovered only after the fix above still didn't resolve the
original crash.** Two SEPARATE, deeper gaps in `plum-ir::fbip`'s
`known_heap` tracking mean `.push()`'s now-genuinely-O(1) reuse path
frequently never gets REACHED at all, regardless of how correct it is
once reached:

1. **A function PARAMETER is never added to `known_heap`** (a real,
   pre-existing, already-documented limitation — no type checker exists
   at the FBIP stage to prove a parameter is uniquely owned). The
   self-hosted bootstrap's own idiom for building up a collection —
   `build_acc (n) (i) (acc) = if i >= n { acc } else { build_acc(n, i+1,
   acc.push(i)) }`, used throughout `tokenize_acc`/`parse_items_acc`/
   etc. — threads its accumulator through a PARAMETER, never a `let`.
   Confirmed directly by dumping the generated LLVM IR for exactly this
   shape: the recursive call compiles to a plain `codegen_array_push_
   fresh` (unconditional fresh alloc + full memcpy, no refcount check
   at all) with the OLD array cell never freed either — leaked outright,
   not just slow.

2. **A `for`-loop-body accumulator gets a conservative `Inc` inserted on
   EVERY iteration**, discovered while chasing down why REWRITING
   `tokenize` to the "safe" `let mut acc = []; for ... { acc = acc.push
   (x); }` shape (assumed safe, based on reading `fbip.rs`'s `For`/
   `Assign` handling, which passes `known_heap` through unchanged) still
   didn't fix the original crash. Confirmed directly the same way: `chars_
   of`'s generated IR (`chars_of` = `.split("")` + `Array.filter`, and
   `Array.filter` desugars to exactly this `let mut`+`for` shape) shows
   `call void @plum_rc_inc(ptr %v9)` immediately before EVERY push inside
   the loop body — FBIP's static last-use analysis can't prove any one
   textual occurrence inside a loop body is the "last" one across N
   runtime iterations, so it conservatively treats the accumulator as
   potentially-still-needed and forces the fresh-alloc-and-copy path
   every time, same as gap 1's symptom, different mechanism. This means
   the "rewrite the bootstrap code to use `let mut`+`for`" plan (chosen
   over extending FBIP itself, specifically to avoid touching memory-
   safety-critical refcounting code again) does NOT actually work — `for`
   loops hit their own, different flavor of the same underlying "can't
   prove this is safe to reuse" conservatism.

Both remaining gaps need the SAME kind of fix: teaching FBIP's `known_
heap`/last-use analysis to recognize a genuinely safe-to-reuse case it
currently can't prove (a parameter used in a self-tail-recursive
"consume once, pass the result on" pattern; a `for`-loop accumulator
reassigned via `Assign` where the old value is always immediately
superseded, never read again). That's real, memory-safety-critical
compiler work — not a source-level workaround — which is exactly the
scope the "rewrite to `let mut`+`for`" choice was meant to avoid. `lexer/
lexer.plum`'s own `tokenize` was rewritten to the `let mut`+`for` shape
regardless (a real, harmless simplification, verified against the full
98/98 corpus + `exec_corpus`, all still passing) but does NOT resolve
the original self-interpretation crash on its own.

**Where this leaves self-hosting**: still not resolved. `Array.push`
itself is now genuinely O(1) amortized in both backends when its fast
path is actually reached (a real, tested, valuable fix on its own,
independent of self-hosting) — but the specific accumulation idioms
this project's own bootstrap code and standard library helpers
(`Array.map`/`filter`/`fold`, `chars_of`, `.split()`'s `Array.filter`
composition, every `_acc`-suffixed parser/lexer helper) actually use
still don't reach it. A future session extending FBIP's `known_heap`
analysis to cover BOTH gaps is the real next step for this specific
line of work; recommend treating that as its own carefully-scoped,
carefully-tested piece, not a quick follow-on, given the two real
sessions' worth of OOM-kill-driven crashes this investigation already
cost.

## Gap 1 (parameter tracking) — attempted, found unsafe, REVERTED (2026-08-14)

Prioritized closing gap 1 first (the lower-risk of the two, per its own
narrow justification: `.push()`/`.pop()`/`.set()`/`.remove()`/`.concat()`
/etc only exist on arrays or strings syntactically, so finding a
parameter in one of those OPERAND positions proves it's array/string-
shaped there without needing general type info). Built `plum-ir::fbip::
confirmed_array_or_str_params`, wired into `optimize_program` to seed
`known_heap` with any parameter it confirms, each then getting the same
`mark_last_uses`-at-its-own-binding-site treatment a `Let`-bound local
already gets (so a genuinely ALIASED parameter still gets its
protective `Inc`, not just reuse eligibility with no safety net).

**Found a real bug in the FIRST version via a real crash, not
inspection**: `exec_corpus/closures` (previously passing) segfaulted
after this landed. Root cause: the scan was shadowing-UNAWARE — a
NESTED binding (a `Closure` param, `Let`, `Match`-arm binding, or `for`
variable) reusing an OUTER parameter's name could have its own,
unrelated array use wrongly attributed to the OUTER parameter, seeding
a genuinely SCALAR parameter into `known_heap` — which then made
`transform` insert nonsensical Inc/Dec/reuse machinery on a raw Int,
crashing when the runtime tried to treat it as a heap pointer. Fixed
with proper scope-narrowing (removing a name from the tracked set at
every point that could shadow it, mirroring the exact discipline
`mark_last_uses`'s own `Let`/`Match` arms already use) — verified with
a dedicated regression test pinning this exact shape, plus reconfirmed
the full corpus.

**Found a SECOND, deeper bug immediately after, via another real
crash**: `exec_corpus/closures` failed again — same fixture, different
line (`apply_twice(add_one, 3)`, calling the same closure twice).
Traced to `interp.plum`'s own `bind_params`, whose `acc` parameter is a
genuine, correctly-detected `acc.push(...)` accumulator — but the value
handed into it, `call_closure`'s `captured_env`, is extracted from a
`VClosure` via a `Match`-arm binding that this whole pass has NEVER
tracked as `known_heap` (match-arm bindings are conservatively left
untracked everywhere else in this file too — "we don't know a
constructor's field types without a type checker," per `transform`'s
own existing comment). Nothing upstream necessarily Inc'd that
extracted binding before handing it into `bind_params`, so `bind_
params`'s now-correctly-threaded Inc/Dec bookkeeping for its OWN `acc`
parameter can end up trusting a refcount that was never actually
correct to begin with — corrupting the closure's own stored environment
across its second call. In short: **syntactic proof that a parameter is
array-shaped is not proof that it's safe to reuse** — safety also
depends on every CALLER of that function having correctly threaded
ownership into the argument being passed, which is a strictly bigger
question than this pass (or arguably any purely intraprocedural, type-
free analysis) can answer for match-extracted values.

**Decision: reverted `optimize_program` back to its original,
unmodified form** rather than ship something merely believed safe after
one fix. `confirmed_array_or_str_params`/`collect_confirmed_params`
(the shadowing-aware scan itself, which IS genuinely correct on its own
terms) are kept in the source, `#[allow(dead_code)]`, with their own
tests retargeted to test the function directly rather than through
`optimize_program` — real, hard-won groundwork for a future attempt,
not thrown away, but not wired in until the match-extracted-binding
question has a real answer too. `bootstrap/self_host/lexer/lexer.plum`'s
`tokenize` reverted back to the `let mut`+`for` workaround it had
before this attempt (gap 2, still open, so it still doesn't resolve the
original self-interpretation crash on its own — but it's the same
already-validated-safe state as before this whole gap-1 detour).
Reconfirmed clean afterward: full workspace suite (1,766 tests) green,
98/98 corpus, 13/14 `exec_corpus` (including `closures`, now passing
again), 5/5 `typecheck_corpus` rejections, all under the guarded runner
with zero session crashes during this cleanup.

**Where this leaves things**: both gap 1 and gap 2 are still open.
A genuinely correct future fix needs MORE than parameter-shape
evidence — it needs a real answer for ownership across match-extracted
bindings (which touches far more than just this one accumulator
pattern; `Match` arm bindings are untracked EVERYWHERE in this pass,
by design, today). That's a bigger, more careful piece of work than
either gap looked like in isolation, and it should be scoped as its
own dedicated design pass — not attempted casually again, given it
already produced two distinct real crashes in the course of one
session's work.

## Loop-accumulator last-use rule ("gap 2") — implemented (2026-08-14)

`mark_last_uses`'s `For` arm forced `live_after = true` across the whole
loop body, so every use of an outer heap-tracked name inside a loop got
an `Inc`. That pinned refcounts at >= 2 and forced `.push()` onto its
fresh-allocate-and-copy path for `let mut acc = []; for .. { acc =
acc.push(x) }` — the idiomatic collection-building shape in this
language, and exactly what `Array.map`/`Array.filter` lower to
(`lower.rs::lower_array_filter`). Quadratic accumulation for the most
common collection idiom in the language.

**The rule**: if the loop body REBINDS the name (`expr_assigns_var` —
shadowing-aware, and deliberately not counting rebinds inside `Closure`/
`Spawn` bodies, which may not run on this iteration), the value the next
iteration reads is the NEW binding, so the old reference's liveness does
not cross the iteration boundary and ordinary backward analysis is
sound. Otherwise the conservative force stays.

**Safety**: relaxing to the ordinary walk does NOT assume every body use
is a last use. `let snap = acc; acc = acc.push(x); use(snap)` walks
backward, sees `acc` still needed by the later `Assign`, and Inc's the
earlier read into `snap` — so the push observes refcount 2 and correctly
takes the fresh-allocate path instead of corrupting `snap`. Pinned by a
dedicated test. A conditional rebind (`if c { acc = acc.push(x) }`,
exactly what `Array.filter` emits) is fine too: consume and rebind live
in the same branch.

**Verified**: `chars_of` (= `.split("")` + `Array.filter`, previously
dying at 20,000 characters) now handles 100,000 characters at a flat
~2.1MB RSS. 5 new `fbip` tests including the aliasing-safety case; full
workspace suite green (1,771 tests); 98/98 corpus, 13/14 `exec_corpus`,
5/5 `typecheck_corpus` unchanged.

**NOT fixed**: `./sh tokens` on the full 20KB `lexer.plum` still exceeds
1GB. The ceiling moved (~5.7KB -> ~8KB of source) but something remains
superlinear, and it is NOT: array push (fixed), `chars_of` (fixed),
string `concat` folding (measured flat at 50,000 double-concats), or
stack depth (a "stack overflow" reading was an artifact of a `ulimit -v`
cap killing the process before RSS could grow — see below). Unknown.

### Guard lesson, corrected

Running the self-hosted binary unguarded OOM-killed the whole terminal
(and the session in it) five times, because the process shares the
terminal's cgroup and a global OOM tears down the entire scope. Two
successive guard designs were wrong:

- A hand-typed `ulimit` per command: shell state does not persist
  between commands, so it had to be retyped every time, and was dropped
  repeatedly under iteration pressure.
- `ulimit -v` in a wrapper: caps VIRTUAL address space, so it (a) trips
  on harmless large reservations, producing false failures, and (b)
  kills before RSS grows, producing a false "low memory use" reading
  that led directly to a wrong stack-overflow diagnosis AND to disabling
  the guard right before a 44.9GB terminal-killing OOM.

**The working design** (`./sh` in the repo root, wrapping `./sh.real`):
`systemd-run --user --scope -p MemoryMax=1G -p MemorySwapMax=0`. A real
RSS-based cgroup cap — the kill is `CONSTRAINT_MEMCG` scoped to the
process's own transient cgroup, never `CONSTRAINT_NONE`/`global_oom`, so
a runaway cannot reach the terminal. Rebuild with `-o sh.real`, never
`-o sh`.

## Profiling the remaining blowup — self-hosted lexer now lexes itself (2026-08-14)

After the loop-accumulator rule landed, `./sh tokens lexer.plum` still
exceeded 1GB, and three successive attempts to guess the cause had all
been wrong. Switched to measurement.

**The instrument** (kept, it's real tooling): per-allocator counters in
the generated runtime — `plum_alloc`/`plum_alloc_array`/`plum_alloc_str`
/`plum_alloc_closure` each bump a count and a byte total, dumped to fd 2
at exit by a global destructor, gated on `PLUM_ALLOC_STATS` so normal
runs are unaffected and stdout stays clean for golden comparisons. No
external profiler is installed on this machine, and "which allocator is
being hammered" was otherwise pure guesswork.

**What it found, in order.**

1. *The loop-accumulator fix wasn't actually working in the real
   pipeline.* A pure `let mut acc = []; for .. { acc = acc.push(i) }`
   showed 1,008 array allocations for n=1000 and bytes going 4MB → 16MB
   → 64MB across doublings: textbook quadratic. Dumping the generated
   LLVM showed a surviving `@plum_rc_inc` before every push. Cause: the
   `For` arm passed the incoming `live_after` into the body, but a use
   AFTER the loop (`acc.len()` — i.e. essentially every real
   accumulator, since you build a collection to use it) makes that true,
   which re-marked the consumed read as live. The rebind argument says
   the post-loop use reads the value the LAST rebind produced, never the
   consumed one, so the body must be analyzed with `live_after = false`.
   The unit test had passed only because it put nothing after the loop.
   Fixed, and a test pinning the realistic shape added. Result: 1,008
   allocations → **4**, constant from n=1,000 to n=100,000.

2. *`Array.slice` was O(tail), not O(range).* Defined as `Array.take
   (Array.drop(arr, start), end - start)`, and `Array.drop` copies the
   entire remaining tail — so slicing a 5-character identifier out of a
   20KB source char array allocated ~20KB. `lexer.plum` does exactly
   that per identifier/number token, giving O(input²). Rewritten as a
   direct single-pass `array_slice_acc` with the original's exact edge-
   case semantics preserved (including its odd negative-`start`
   behavior, which the take∘drop composition produced and which the
   existing tests pin).

3. *`String.index_of` was quadratic on its own.* `string_index_of_acc`
   called `Array.slice` at EVERY position to compare against the needle.
   Replaced with `string_matches_at`, an in-place elementwise compare —
   zero allocation. This one is called per character for classification
   (`is_digit_char`/`is_ident_start_char` against fixed alphabets), so
   it was a large constant on top of the quadratic.

4. *String join was ~98% of what remained.* With arrays fixed, the
   profile showed string allocation dominating: 2,789MB of string
   allocation for a 69KB input (vs 44MB of array). `string_join_with_
   space` used `Array.fold(.., |acc, s| acc.concat(" ").concat(s))`, and
   the OUTER concat's receiver is a freshly-built temporary — `fbip`'s
   reuse analysis only marks a bare VARIABLE receiver, so that concat
   always allocated-and-copied the whole accumulated output, once per
   token. Split into two single-`.concat()` statements so each has a
   bare `acc` receiver: 2.75GB → 1.63GB, 864ms → 500ms.

**Result — the milestone this whole investigation was chasing.** Every
self-hosted source file now tokenizes AND parses itself, with output
**byte-identical** to the real Rust compiler:

| file | size | `tokens` | `ast` |
|---|---|---|---|
| lexer.plum | 22,797 B | match | match |
| parser.plum | 68,888 B | match | match |
| interp.plum | 41,520 B | match | match |
| types.plum | 10,055 B | match | match |
| context.plum | 8,190 B | match | match |
| infer.plum | 48,664 B | match | match |
| main.plum | 3,897 B | match | match |

Cost: lexer.plum 121MB/165ms; parser.plum 1.63GB/500ms. All 14 Rust
test suites green, 196/196 corpus checks, 13/13 exec_corpus, 5/5
typecheck_corpus rejections.

**Still open, and now precisely located**: string cells have no
`capacity` field, so `StrConcatReuse` reallocs to exact size on every
append — the same bug the array layout change fixed, but for strings.
That's why 69KB of input still costs 1.63GB: memory is still quadratic
in output length, just with a much smaller constant. The fix is the
string-cell counterpart of the array `capacity` word (a wide, layout-
sensitive change touching every string producer in the runtime), and it
is the clear next step for this line of work.

## Closing the string blowup — three more fixes, 72x less memory (2026-08-15)

Continued from the profiling pass above, which had left `parser.plum`
tokenizing itself at 1.63GB. The allocation counters made each step a
measurement rather than a guess.

**1. `StrConcatReuse` reallocated to exact size on every append.** The
array-cell fix had added a stored `capacity` word, but doing the same
for strings would move every string's data offset from 16 to 24 — 76
sites in this backend assume it, and separating the string ones from
the structurally identical `Ctor` ones is exactly the error-prone audit
worth avoiding. Instead capacity is DERIVED from `malloc_usable_size`:
ask the allocator how big the block really is, append in place when it
fits, and otherwise `realloc` to double. Same amortized-O(1) growth as
the array fix, **zero layout change** — `plum_alloc_str` and every
reader are untouched. (`other` can never alias the receiver on the
reuse path: that would be two live references, so the refcount check
would already have taken the fresh branch — which is also why the
`realloc` can't invalidate the copy source.)

**2. Character classification re-split its alphabet on every call.**
`String.index_of` is defined as `string_index_of_acc(chars_of(s),
chars_of(needle), 0)`, so `is_ident_start_char(c)` re-split the whole
52-character alphabet into 52 fresh one-character heap strings *per
source character*. That was ~7 million string allocations while
tokenizing the 69KB parser source — the single largest cost at that
point. Hoisted the split into `Array[String]` GLOBALS (evaluated once)
tested with `Array.contains`, in all four places that did this
(`lexer.plum`'s digit/alpha checks, and `is_capitalized`/`is_capitalized
_name` in `parser.plum`/`interp.plum`/`infer.plum`). String allocations
dropped ~9x.

**3. Two rebinds in one loop body defeated the rebind rule.** A string
join is `for .. { if c { acc = acc.concat(" ") }; acc = acc.concat(s) }`
— and the backward walk saw the SECOND rebind's read as making the
FIRST one live, so the first concat took the allocate-and-copy path and
the whole join stayed quadratic (900MB for a 10,000-element join, 4x
per doubling). The `For` arm's rebind rule only severed the
loop-carried edge; ordinary sequencing needed the same treatment. Now
`Assign`'s own arm analyzes its `value` with `live_after = false`
whenever it rebinds the name being analyzed, since every use in `rest`
reads the NEW binding. Aliasing safety is unchanged and still comes
from the ordinary walk, not a special case: `let snap = acc; acc =
acc.concat(x); use(snap)` still Inc's the read into `snap`, so the
concat's runtime refcount check takes the fresh path rather than
corrupting the alias. Both properties are pinned by tests. The join
benchmark went from 225MB to 260KB at n=5,000 and is now exactly
linear (2.0x bytes per 2x input, measured to n=40,000).

**Result.** Every self-hosted file tokenizes AND parses itself,
byte-identical to the real Rust compiler, now within the ordinary 1GB
guard:

| file | size | peak RSS | wall | before |
|---|---|---|---|---|
| lexer.plum | 25 KB | 4 MB | 49 ms | 224 MB |
| parser.plum | 69 KB | 38 MB | 102 ms | 2,752 MB |
| interp.plum | 42 KB | 4 MB | 80 ms | — |
| infer.plum | 49 KB | 30 MB | 57 ms | — |

7/7 files match on both `tokens` and `ast`. 196/196 corpus checks,
13/13 exec_corpus, 5/5 typecheck_corpus rejections, all 14 Rust suites
green. Peak memory for the largest file fell 72x and wall time 8.5x.

**Method note worth keeping.** Every one of these was found by
measurement and every one contradicted a plausible-sounding guess — the
`ulimit -v` "stack overflow" that was really a capped allocation, the
string-concat hypothesis that measured flat, the loop-accumulator fix
whose unit test passed while the real pipeline still emitted a
per-iteration `Inc`. The allocation counters (`PLUM_ALLOC_STATS=1`) and
reading actual generated LLVM are what turned each one from speculation
into a specific line to change.

## Self-hosted type checker checks its own lexer (2026-08-15)

`./sh check` previously failed on every self-hosted source. Three fixes,
each found by running it and reading the actual error rather than
guessing:

**1. Two-pass context building.** `build_context` resolved each
declaration's field/payload annotations against the context as it
existed BEFORE that declaration was added — so `pub enum ITy { ..
ITFunction(Array[ITy], ITy) .. }` (this checker's own `types.plum`)
failed with "unknown type: ITy" on its own self-reference. Every
self-hosted source hit this (`Ty` in parser.plum, `Value` in
interp.plum, `InterpPiece` in lexer.plum). Now pass 1 registers every
declaration's NAME and generic parameters with empty fields, and pass 2
resolves annotations against that complete set. No fixpoint is needed
because resolution only ever consults names and arity, never field
types. This is the same Phase-1/Phase-2 split the real compiler uses —
and the one `infer.plum` could legitimately SKIP for functions (their
signatures must be fully annotated, so all are known up front); types
get no such escape, since a declaration's shape is knowable only from
the declaration itself.

**2. Builtin declarations and signatures.** `Option[T]`/`Result[T, E]`
live in the real compiler's prelude, not in any file the checker is
handed, so they're seeded into both context passes — their variant tags
then resolve through the same `find_variant` path a user enum's do,
with no special-casing. Prelude FUNCTIONS got the same treatment via a
`builtin_sig` table written as ordinary generic `FnSig`s, so
`infer_fn_call`'s existing instantiate-with-fresh-vars path handles
them unchanged: `Option.unwrap_or[T](Option[T], T) -> T` checks through
exactly the code a user's own generic function does. Only the ~16
builtins the self-hosted sources actually call are listed (counted, not
guessed).

**3. `()` is the Unit value, not a 0-tuple.** The parser emits
`ETuple([])` for it (there is no separate unit-literal node), so
`usage(())` against `let usage (): Unit` failed as "() != Unit".
`infer_tuple` now maps the empty case to `ITUnit`, matching what
`resolve_param_ty` already did for the Unit *pattern*.

**Result**: `./sh check bootstrap/self_host/lexer/lexer.plum` → `ok`.
The self-hosted type checker fully checks its own sibling stage — 25KB
of real Plum, including generic builtin instantiation. Verified not to
be a vacuous pass: injecting a wrong return type yields "function
is_digit_char: declared return type Int doesn't match body type Bool",
and `Array.contains(PLUM_DIGIT_CHARS, 42)` yields "argument 1: Int !=
String" (i.e. `T` correctly instantiated to `String` from the array).
No regressions: 5/5 typecheck_corpus still rejected with unchanged
messages, 13/13 exec_corpus check AND run, 196/196 corpus.

**The remaining blocker is module resolution, and it is the only one
for 5 of 7 files.** `./sh check` reads a single file, but `parser.plum`
references `lexer.Token`, `interp.plum` references `parser.PExpr`, and
`main.plum` calls `lexer.tokenize` — so they fail with "unknown type:
Token" / "unbound variable: lexer". Note `resolve_named` already
strips the module qualifier (it keys off the last path segment), so the
missing piece is purely *loading* the modules a file `use`s and merging
their public declarations, not resolving qualified names. `types.plum`
is the one non-module failure left: a genuine field-access inference
gap ("field access requires a struct value with a statically known
type"), the same constraint the real compiler documents.

## The self-hosted type checker now checks the whole self-hosted compiler (2026-08-15)

`./sh check bootstrap/self_host` → `ok`. All 7 modules, ~250KB of Plum,
type-checked as one program by the type checker written in Plum —
including `typecheck/` checking itself. Four more fixes on top of the
two-pass context work:

**1. Module loading (`check` accepts a directory).** A module can't be
checked alone — `parser.plum` references `lexer.Token`, `interp.plum`
references `parser.PExpr` — so `check` now walks a directory, parses
every `.plum` file under it, and CONCATENATES their items into one
program. Concatenation is the entire mechanism: `resolve_named` already
keys off a path's last segment, so `parser.PExpr` and a bare `PExpr`
resolve identically once both files' declarations are present. (`.`/`..`
are skipped explicitly — `list_dir` returns raw `readdir` entries.)

**2. Module-qualified CALLS.** `lexer.tokenize(src)` parses as a field
access on `lexer`, which isn't a value. Resolution order in `infer_call`
is now: bound local/global (a real value → ordinary method call, so a
local can never be shadowed by a module) → builtin namespace → merged
top-level signature (the module-qualified case) → method call. Reached
only when the qualifier names no value, so it cannot swallow a genuine
record-of-closures field call.

**3. Closure-param-type seeding.** `Array.filter(self_s.entries, |e| ..
e.id ..)` (real code in `types.plum`) inferred the closure BODY before
unifying the closure against `(ElemTy) -> Bool`, leaving `e` an
unresolved variable at `e.id` — and field access genuinely cannot be
deferred, since it needs a concrete struct to look the field up in. Now
`Array.map`/`filter`/`fold` seed a closure LITERAL's parameter types
from the already-known element type before inferring its body. This is
the same inference-ordering problem the real compiler hit twice (see the
language-ergonomics chunk) arriving in the self-hosted checker.

**4. A real false positive from flat merging.** Merging modules into one
namespace made `bind_params` ambiguous — `interp.plum` binds runtime
VALUES, `infer.plum` binds parameter TYPES, same name, different
signatures — so the checker rejected a program the real compiler accepts
(modules scope those names properly). Renamed `infer.plum`'s to
`bind_param_types`, which is the clearer name anyway. **This is a
workaround, not a fix**: the self-hosted checker has a flat top-level
namespace and will reject any valid program with same-named functions in
different modules. Only 4 names collide across the current sources, and
the other 3 (`quote`, `escape_str_chars`, `is_capitalized_name`) are
identically-typed so first-wins is harmless. Proper per-module scoping
is the real fix and is not done.

**Verified not vacuous.** Against a copy of the tree, three injected
errors were each caught: a wrong return type deep in `parser.plum`
("function is_at_eof: declared return type Int doesn't match body type
Bool"), a CROSS-MODULE argument error (`lexer.tokenize(42)` →
"argument 0: Int != String", proving module resolution really does check
across boundaries), and a bad field on a struct declared in another
module ("struct ItemResult has no field named nonexistent_field"). The
unmodified copy passes.

No regressions: 5/5 typecheck_corpus still rejected, 13/13 exec_corpus
check AND run, 196/196 corpus goldens, 14/14 Rust suites.

**Where self-hosting stands.** Of the four stages, the front end is now
genuinely self-applying: the lexer tokenizes itself, the parser parses
itself, and the type checker checks all of it. What remains for true
self-hosting is the back end — the self-hosted INTERPRETER can't yet run
the compiler (it has no module loading of its own, and `interp.plum`'s
own scope cuts still exclude generics), and there is no self-hosted
codegen at all.

### The self-hosted interpreter now runs the whole self-hosted compiler (2026-08-15)

```
./sh run bootstrap/self_host check bootstrap/self_host    # -> ok, 6.3s
```

That command is the self-hosted **interpreter** running the self-hosted
**lexer**, **parser**, and **type checker** over all 7 modules of the
self-hosted compiler — including over the interpreter's own source. All
four stages now self-apply. The previous section listed exactly this as
the remaining gap ("the self-hosted INTERPRETER can't yet run the
compiler"); this closes it.

**What it took.** Five changes, four of which are real bugs that only
self-interpretation could have surfaced.

*Project loading for `run`.* `./sh run <dir>` now takes a directory and
concatenates every `.plum` file's items, exactly as `check` already did
and for the same reason. Everything after the directory becomes the
INTERPRETED program's own `args()` (a new `argv` field on
`ProgramState`, threaded by `build_program_state_argv`) — that is what
lets the inner compiler be handed `check bootstrap/self_host` as its own
command line.

*Prelude builtins.* The real `plumc` prepends a large Plum-source
prelude that this interpreter can't parse yet (it reaches into `extern
"C"`/`unsafe`/explicit generic instantiation). The subset the compiler
actually uses is provided as builtins instead — `chars_of`, `chars_join`,
`read_file`, `list_dir`, `is_directory`, `args`, `panic_raw`, plus the
`Array`/`String`/`Option`/`Result` associated functions. Each is a
one-liner delegating to the REAL host function of the same name, the
same trick `VArray` already used for array primitives: the semantics are
identical by construction rather than by re-derivation. Honestly a shim
— it is not the prelude, it borrows it.

*Module-qualified calls, `lexer.tokenize(src)`.* Resolution order is
bound value → builtin namespace → flat top-level function → value method
call, so a local can never be shadowed by a module and nothing can
redefine `Array`. Same flat-namespace consequence the checker already
documents.

**Bug 1 — assignment through a nested block was silently discarded.**
`eval_stmt_for_env` threaded the environment out of a `for` loop only;
`if`/`match`/bare blocks in statement position evaluated their
assignments and then threw the result away. That gap was already written
down in this codebase as "not exercised by any exec-corpus fixture" —
and self-interpretation exercised it in the first thirty seconds, since
`lexer.tokenize` accumulates inside `if !done { ... }`. It produced ZERO
tokens, silently and with no error. `if`/`match`/block now thread too,
including a block's TAIL expression (an unterminated trailing `match`
parses as a tail, not a statement — which is exactly the shape
`tokenize` uses).

Every threading path funnels through one new helper, `scope_out(inner,
n)`, which cuts the environment back to the enclosing scope's length.
That is not tidiness: `eval_stmts` only ever appends entries or `.set`s
an existing index, so the prefix is the enclosing scope with updated
values — and leaking an inner binding would be a genuine bug, since
`env_assign` scans from the END and would then write a later outer
assignment into the leaked shadow instead of the variable it names.

The remaining gap is stated rather than left to be found: an assignment
inside an expression-POSITION `if`/`match` (`let x = if c { n = n + 1; 2
} else { 3 }`) is still lost, because `eval_expr` returns a value alone.
Closing it means returning an (env, value) pair from every arm — a real
refactor, not needed by anything this compiler contains.

**Bug 2 — `&&`/`||` did not short-circuit.** Both operands were
evaluated before dispatch. Every exec-corpus fixture used `&&` as a pure
boolean combinator, where eager and lazy are indistinguishable; the
lexer uses it as a GUARD (`int_end + 1 < chars.len() &&
is_digit_char(chars[int_end + 1])`), where evaluating the right side
after the left says "don't" is a real out-of-bounds index. A new
`eval_binary_expr` takes the right operand as an unevaluated `PExpr`;
every other operator is genuinely strict and still goes through
`eval_binary`.

**Bug 3 — `==` didn't work on compound values.** Only scalars were
comparable, which the parser needs constantly (`tok == TokEof`).
`values_equal` is now structural over variants, arrays, tuples, and
structs. Closures stay excluded — function equality has no defensible
answer and real Plum doesn't offer one.

**Bug 4 — the for-loop environment grew linearly in iteration count.**
Bindings accumulated, justified as harmless because a later iteration
shadows an earlier one. True, but it made every `env_lookup` in a long
loop scan the whole history. `tokenize`'s loop runs once per source
character, so self-interpreting a 69KB file made that quadratic — the
same accidental O(n²) this project has now hit in four separate places.
`scope_out` fixes it and makes the loop body a real scope at the same
time.

*Checker support for the above:* `.len()` is now overloaded on the
receiver (byte length for `Str`, element count for `Array`). This is the
one place inference branches on an already-resolved type instead of
unifying against one expected shape, because `Str` and `Array[?]` have
no common unifier; an unresolved receiver still takes the array path,
preserving every pre-existing inference exactly. `.split()`,
`String.slice`, and `String.is_empty` were added alongside.

**Verified, and verified not vacuous.**

| check | result |
| --- | --- |
| corpus AST, self-interpreted | 98/98 |
| corpus tokens, self-interpreted | 98/98 |
| exec_corpus `run`, through TWO interpreter levels | 14/14 |
| `run self_host check self_host` | ok, 6.3s |
| corpus native | 196/196 |
| exec_corpus native check + run | 14/14 |
| typecheck_corpus rejections | 5/5 |
| Rust suites | 14/14 |

Injected into a copy of the tree and caught by the SELF-INTERPRETED
checker: a wrong return type in `parser.plum`, and a cross-module
argument error (`lexer.tokenize(42)` → "argument 0: Int != String"). The
unmodified copy passes.

**Where self-hosting stands now.** All four stages self-apply. The one
thing still missing for true self-hosting is a self-hosted CODEGEN —
there is no Plum-written backend at all, so the self-hosted compiler can
check and interpret its own source but cannot produce a binary from it.

### Stage 5: a self-hosted CODE GENERATOR (2026-08-15)

```
bootstrap/shbuild bootstrap/exec_corpus/enums_and_match/main.plum /tmp/prog
/tmp/prog          # -> 12 / 9, byte-identical to `plum build`'s own binary
```

A Plum-written compiler now produces native binaries. `bootstrap/
self_host/codegen/` emits LLVM IR text; `plum compile-ir` hands it to
`clang`. **11 of the 14 runnable exec_corpus fixtures compile and run,
and all 11 produce output byte-identical to the REAL backend's binary
for the same program** — not merely matching `expected.txt`, but
agreeing with the other implementation, which is the stronger claim.

**The decision that shaped it: unboxed, not boxed.** The cheap route was
one uniform tagged representation for every value — no type information
needed anywhere, generics erasing for free, a far smaller emitter. It
was rejected deliberately: boxing costs a tag check on every operation
and, more importantly, produces a backend that would eventually be
thrown away rather than grown. So an `Int` is an `i64` in a register, a
`Float` is a `double`, a `Bool`/`Unit` is an `i1`, and everything else
is a `ptr` to a heap cell.

**What makes unboxed tractable here: Stage 5 never frees.** No
refcounting, no FBIP, no `plum_rc_inc`/`dec`. That is 3,293 lines of
`plum-ir/src/fbip.rs` skipped, and it is a legitimate bootstrap choice —
the workload is "start, compile one program, exit," where the OS
reclaims everything at once. It is the first thing that would have to
change for this backend to compile long-running programs, and it is
stated in `runtime.plum` rather than left to be discovered.

#### Where the types come from

`typecheck/infer.plum` computes the type of every expression and
discards it — nothing writes inferred types back onto the AST, and
`parser.PExpr` has no field to write them into. So the emitter
re-derives what it needs, bottom-up, as it emits.

That is duplication and worth naming as such. Two things keep it small:
it runs AFTER `check_program` on a program already known well-typed, so
it needs no unification, substitutions, fresh variables, or error
recovery — most of what `infer.plum`'s 931 lines are; and it needs only
the REPRESENTATION of a type, not its identity, so `CgTy` has eight
cases where `ITy` is a full type language.

The alternative was threading a typed node through `InferResult`'s 54
construction sites — a rewrite of the one component that currently
type-checks the entire self-hosted compiler. Not worth destabilizing for
a Stage 5 that didn't exist yet. **If Stage 5 grows to need real
inference — closures are exactly where that starts, since an unannotated
`|x| ...` parameter cannot be synthesized bottom-up — the right move is
to make `infer.plum` produce a typed tree, not to grow a second
inference engine in the backend.**

#### Two techniques worth recording

**Every local is an `alloca` slot, never a bare SSA register.** Loaded
on read, stored on write. This means the backend never constructs a phi
node and never reasons about dominance: an `if` that produces a value
stores into one slot from both arms; a `match` does the same across N
arms; `let mut` and `let` need no distinction at all; and a `for` body's
assignment to an outer local is just a store — the exact thing the
self-hosted INTERPRETER needed a whole env-threading mechanism
(`scope_out`) to get right. LLVM's mem2reg turns all of it back into
registers with correct phis, so it costs nothing — **at `-O1` and
above**. That qualifier went unwritten for a long time and it was not
free: nothing in the toolchain passed `clang` an `-O` flag at all, so
every binary this compiler had ever produced was `-O0`, mem2reg never
ran, and the design's central "the spill is free" assumption was simply
never collected on. Every local read was a real load, every write a
real store. Fixing it is one `clang` argument, and the measurement is
in the `OPT_ARTIFACT` doc comment: 2x, across the board.

The one catch is that allocas must be **hoisted to the entry block**.
An `alloca` executed inside a loop body allocates afresh every iteration
and is not reclaimed until the function returns — a real stack leak —
and mem2reg only promotes entry-block allocas anyway. So `Emit` carries
`allocas` as a field separate from `code`, and `cg_fn` splices it in
after `entry:`.

**Enum cells are sized for the enum's widest variant, never for the
variant being built.** That is a correctness requirement, not a
convenience: a `match` arm loads a payload slot *before* it knows the
tag matched, so per-variant sizing would let that load run off the end
of the allocation. Over-allocating means such a load reads garbage it
then discards. It is what allows pattern bindings to be computed
unconditionally, which in turn is what keeps nested patterns from
needing a second layer of control flow.

#### Toolchain additions

- `plum emit-llvm <project> [-o f.ll]` — print the IR `plum build` would
  compile. Written as the reference for what correct IR looks like, and
  useful on its own for debugging generated code. It shares `build_ir`
  with `plum build`, so the two cannot drift.
- `plum compile-ir <f.ll> -o <bin>` — assemble and link. The self-hosted
  compiler needs this because Plum has no process-spawn builtin; even a
  finished self-hosted compiler would delegate this step, so it is a
  division of labor rather than a bootstrapping cheat.
- `bootstrap/shbuild <file.plum> <out>` — the two steps together.

#### The stray trailing `0`, finally chased down

Every compiled Plum program has ended with a spurious `0` line for as
long as this project has had a native backend — "a pre-existing
native-`main()` CLI behavior noticed several times, never chased down."
It was `emit_main`: it echoes the entry function's return value, and
`Unit` shared `Bool`'s `%d\n` print path, so a `Unit`-returning `main`
printed `0`. `Unit` carries no information, so echoing it was pure
noise. A `Unit` entry point now prints nothing, `bootstrap/exec_corpus`
no longer needs `head -n -1` on the native side, and exactly one test
asserted the artifact (it now asserts its absence).

#### Verified

| check | result |
| --- | --- |
| exec_corpus compiled by the self-hosted backend | 11/14 (arrays, closures, `tuples` unsupported) |
| those 11 vs. the REAL backend's binaries | byte-identical, 11/11 |
| `sh run self_host emit-llvm` (codegen under the interpreter) | byte-identical IR, compiles and runs |
| `sh check bootstrap/self_host` (now 9 modules) | ok |
| `sh run self_host check self_host` | ok |
| exec_corpus native check + run | 14/14 |
| Rust suites | 14/14 |

### Stage 5b: arrays and closures (2026-08-15)

**13 of the 14 runnable exec_corpus fixtures now compile and run** with
the self-hosted backend, up from 11.

**Arrays** are `{ i64 len, elem0, ... }` with an 8-byte slot per element
regardless of the element's own width — the same trade the struct layout
makes, space for an offset rule that fits on one line. `push`/`set`
build a new cell and copy rather than mutating, because Plum arrays are
values and this backend has no ownership analysis that could prove an
in-place update safe. That is where the real backend was before FBIP;
the difference is it now has `ArrayPushReuse` and this one does not, so
**a `push` in a loop here is quadratic** — the most likely first
performance cliff if this backend is ever pointed at a big program.

An empty literal `[]` has no element to derive a type from.
`CgArray(CgUnit)` records exactly that, `.len()` works on one, and any
operation that would genuinely need the element type fails with a clear
message. That is the same gap closures have, from the same cause.

**Closures** are `{ ptr code, capture0, ... }`; calling one loads the
code pointer and passes the cell back in as the first argument. Two
decisions worth recording:

*Captures are the whole enclosing environment, not the free variables.*
Computing free variables means a shadowing-aware scan of the body — the
exact analysis that produced a crash-causing bug in `plum-ir/src/
fbip.rs` earlier in this project when it got shadowing wrong. Capturing
everything is trivially correct and costs one word per in-scope local at
the creation site. A deliberate trade of space for the absence of a
subtle analysis.

*`Array.map`/`filter`/`fold` are emitted as inline loops.* They are
prelude functions in the real compiler — ordinary generic Plum — and
this backend has neither a prelude nor generics. Inlining is not a
workaround: it is what a monomorphizing backend would produce for them
anyway, and it puts the array's element type right where the closure
argument needs it.

#### Where bottom-up synthesis runs out, precisely

Closure literals get their parameter types pushed DOWN from context
(`cg_expr_params`), which covers call arguments, struct fields, variant
payloads, and `Array.map`/`filter`/`fold`. It does not cover `let f =
|n| n + 1`, where nothing in the surrounding context says what `n` is —
only unifying the body's use of it does. That is inference, not
synthesis, and `infer.plum` already performs it and discards the result.

So the boundary predicted when Stage 5 started has been reached exactly
where predicted. **The fix is to make `infer.plum` produce a typed tree,
not to grow a second unifier in the backend.** The `closures` fixture is
the one remaining failure and it fails for this reason alone.

#### Two fixtures the REAL backend cannot compile

`arrays` and `closures_in_structs` both compile and run correctly here,
and both are rejected by `plum build`: the real compiler cannot infer
`let empty = []`, and it refuses a closure-typed struct field. Their
`expected.txt` goldens come from `plum run` (the interpreter), and the
self-hosted backend's binaries match those goldens byte for byte. This
is not a claim of superiority — the real backend's rejections are
deliberate — but it does mean the "identical to the real backend" check
covers 11 of the 13, with the other two checked against the interpreter.

#### A real regression: self-interpretation now needs 16GB

`./sh run bootstrap/self_host check bootstrap/self_host` still returns
`ok`, but the memory it needs went from 6–8GB (7 modules) to 12–16GB
(9 modules). Roughly 20% more source, roughly double the memory: the
self-hosted interpreter's memory is **superlinear in the size of the
program it is interpreting**.

`PLUM_ALLOC_STATS=1` puts 3.0GB of the 6.3GB total in array allocation —
the environment, which is threaded by value and copied with
`Array.concat(env, bindings)` at every binding site.

The obvious fix, `.push()` instead of `Array.concat` for the
single-binding case, was tried and **made it worse** — slow enough to
time out. The reason is worth recording: `env` is a function PARAMETER,
and parameters are still not tracked in `plum-ir/src/fbip.rs`'s
`known_heap` (the gap-1 work reverted earlier in this project). So the
`.push()` is not an in-place append at all; it is a copy that ALSO
applies capacity doubling, allocating roughly twice what `Array.concat`
allocated. That is a concrete, measured cost for gap 1 still being open,
and the first time this project has been able to name one.

Fixing it properly means either closing gap 1 or changing the
interpreter away from value-threaded environments. Neither was done
here; the change was reverted.

### The checker now publishes a typed tree (2026-08-16)

**Every runnable exec_corpus fixture — 14 of 14 — now compiles and runs
under the self-hosted backend.** The last one, `closures`, was blocked
on `let f = |n| n + 1`: a closure literal with no expected type, whose
parameter representation is only pinned by UNIFYING the body's use of
it. That is inference, and the backend had none.

The fix was the one predicted when Stage 5 started: `typecheck/
infer.plum` already computed that type and discarded it, so it now
publishes it instead. `typecheck/texpr.plum` defines a `TExpr` — a
`PExpr` with a type on every node — and `infer_expr` builds one as it
goes. `check_program` throws it away; the new `typed_program` keeps it.
**There is still exactly one traversal**: checking a function and
producing its typed body are the same operation, so nothing can drift
between what the checker proved and what the backend compiles.

#### It paid twice, as expected

The backend shrank from 1,717 to 1,557 lines while gaining a feature,
and the deletions are the interesting part — these were not moved, they
ceased to exist:

- the operator result-type table (`cg_binary_result_ty`)
- the "a `match`'s type is its first arm's type" rule, which emitted an
  arm speculatively into a scratch buffer purely to read its type back
- the empty-array `CgArray(CgUnit)` "element type never determined"
  marker, and the double-emission of an array literal's first element
  to discover the element type
- `cg_expr_params`/`cg_call_args_typed`/`cg_param_tys_of` — the whole
  mechanism for pushing expected parameter types down into closure
  literals from call arguments, struct fields, and `Array.map`
- `cg_resolve_ty`/`cg_resolve_named`/`CgTyName` — annotation resolution,
  replaced by reading the checker's own declaration tables through three
  new accessors (`ctx_field_names`/`ctx_field_types`/
  `ctx_variant_payload`)

Each existed only to reconstruct something inference already knew.

#### Calls arrive classified

`parser.PExpr` has one `ECall` covering user functions, prelude
functions, variant constructors, closure values, methods, `Array.map`
and `println`. Telling them apart takes scope information the parser
doesn't have, and the backend used to redo that resolution with its own
copy of the precedence rules — bound local beats builtin namespace beats
top-level function beats method. `infer_call` already did it; now the
typed node records the answer (`TFnCall`, `TClosureCall`,
`TVariantNew`, `TMethodCall`, `TArrayNs`, `TPrintln`).

**The backend can no longer disagree with the checker about what a call
is.** That is a whole class of bug removed rather than tested for, and
it is worth more than the line count.

#### One genuinely new rule: defaulting

`let empty = []` has an element type nothing constrains — no element is
ever stored or read — so it survives inference as a free variable.
Reaching codegen, it is not an error: the program provably does not
depend on which type it is, or unification would have pinned it. So an
unresolved variable DEFAULTS (to `Unit`), which is the standard
resolution for an ambiguous type rather than a fallback for a missing
case.

That is also why this backend compiles `let empty = []` while the real
compiler rejects it as uninferable — a real behavioural difference,
noted rather than smoothed over.

#### Reading types

A type recorded during inference may still hold unresolved variables;
the function's final substitution resolves them. Rather than rewriting
the whole tree once inference finishes — another full mirror of the enum
to maintain — the substitution is applied at the point of USE
(`texpr_ty`), and `CgProgram` carries the current function's
substitution, swapped per function by `cg_fn`.

#### Verified

| check | result |
| --- | --- |
| exec_corpus via the self-hosted backend | **14/14 runnable** (only `tuples`, the documented annotation exception, is left) |
| those vs. the real backend's binaries | 12/12 identical (it can't build `arrays` or `closures_in_structs` at all) |
| `sh check bootstrap/self_host` (10 modules) | ok |
| `sh run self_host check self_host` | ok |
| typecheck_corpus rejections | 5/5 |
| corpus goldens | 98/98 |
| exec_corpus check + interpreter run | 14/14 |
| Rust suites | 14/14 |

**Next: generics.** They need monomorphization — the last major
subsystem the real backend has and this one doesn't.

### Generics, by monomorphization (2026-08-16)

The self-hosted backend compiles generic functions, structs and enums.
Two new exec_corpus fixtures (`generics/`, `generic_types/`) pin it, and
both produce output byte-identical to the real compiler's.

```
define i64 @plum_identity__Int(i64 %p0)
define ptr @plum_identity__Str(ptr %p0)
define i1  @plum_identity__Bool(i1 %p0)
```

Three real functions with three different machine types, from one
declaration.

**Monomorphization is the bill for being UNBOXED.** A boxed backend
needs none of it — every value is a `ptr`, so one copy serves every
instantiation. That tradeoff was weighed when Stage 5 started and
decided in favour of unboxed; this is the cost coming due, and it was
the last major subsystem the real backend had that this one lacked.

#### What made it tractable

The typed tree, again. `infer_fn_call` instantiates a generic signature
with fresh variables and then unifies them away, so by the end of
inference the type arguments at each call site are recoverable but
nowhere recorded. `TFnCall` now carries them. **That is information a
backend cannot reconstruct** — it is the one thing monomorphization
needs and the one thing the old bottom-up synthesis could never have
produced.

**No specialized body is ever built.** Emission walks the same typed
tree with a different generic mapping in `CgProgram`, applied at
type-read time (`cg_concrete`) exactly as the substitution already was.
That is sound only because nothing about a body's SHAPE depends on its
type arguments — true here, and it would stop being true if this backend
ever specialized on values.

#### Reachability falls out for free

A generic function has no single symbol to emit until a call site says
which instantiation it needs, so the worklist has to start from `main`
anyway. Monomorphic functions come along the same way, which means the
backend now emits only what is reachable — dead-code elimination as a
consequence of the design rather than a separate pass.

The worklist carries a fuel counter. It is not decoration:
**polymorphic recursion** (`f[T]` calling `f[Array[T]]`) generates
infinitely many specializations, and monomorphization is undecidable for
it in general. Plum can't express it today; if that changes, this stops
with an explanation instead of exhausting memory.

#### Generic aggregates need no specialization

`struct Box[T]` and `enum Maybe[T]` are emitted once. Every field
occupies the same 8-byte slot whatever `T` is, and a variant's tag
doesn't depend on it either — only the machine type of a field or
payload LOAD varies. So pattern matching instantiates the declared
template against the SCRUTINEE's own type arguments, which meant
patterns had to start carrying the scrutinee's `ITy` rather than its
`CgTy`: `CgStruct("Box")` cannot tell you whether `item` loads as an
`i64` or a `ptr`, and `ITStruct("Box", [Int])` can.

#### A checker fix this forced

`unify` rejected `ITParam` outright as "internal error: unresolved
generic parameter reached unification" — correct while no generic
function body was ever checked, and wrong the moment one was. A generic
parameter is **rigid** inside its own body: `T` unifies with itself and
nothing else. That distinction matters — if `T` could unify with `Int`,
the checker would accept `let f[T] (x: T): T = 1`, which no
instantiation can satisfy.

#### Where this leaves real code

**The self-hosted lexer — 500 lines with enums, arrays, closures and
generics — now type-checks and reaches codegen.** It stops at
`chars_of`: a PRELUDE function this backend's runtime doesn't implement.
The blocker for compiling real Plum is no longer a missing language
feature, it is a finite list of library functions — and most of the real
prelude is ordinary Plum source that this backend could now compile if
it were handed it.

#### Verified

| check | result |
| --- | --- |
| exec_corpus via the self-hosted backend | **16/17** (only `tuples`, the documented exception) |
| those vs. the real backend | 14/14 identical (it can't build `arrays` or `closures_in_structs`) |
| new `generics`/`generic_types` fixtures | checker ok, interpreter ok, backend ok |
| `sh check bootstrap/self_host` | ok |
| typecheck_corpus rejections | 5/5 |
| corpus goldens | 98/98 |
| Rust suites | 14/14 |

### The prelude, in Plum — and the backend compiles the front end (2026-08-16)

```
./sh emit-llvm <lexer+parser project> | plum compile-ir -o parse
./parse bootstrap/corpus/let_defs/associated_function.plum
  -> ((let Point.add ((a:Point) (b:Point)) ->Point a))
```

**The self-hosted lexer and parser, compiled to a native binary by the
self-hosted backend, produce byte-identical ASTs to the real Rust
compiler on all 98 corpus fixtures.** That is ~1,800 lines of real Plum
— enums, arrays, closures, generics, recursion, string handling —
compiled by a compiler written in Plum.

#### The prelude is Plum source, compiled like anything else

`codegen/prelude.plum` carries the prelude as source text and prepends
it to every program, exactly as `crates/plumc`'s `STDLIB_*_SRC`
constants do. **These are compiled by the backend, not special-cased in
it**: `Array.slice[T]` is a genuine generic function that gets
monomorphized per element type, and a prelude written in Plum is a
prelude that proves the backend handles real Plum.

Prepending (not appending) is what makes a user definition of the same
name win — `find_sig` scans in order — which is the real compiler's
behaviour too.

Only what CANNOT be written in Plum lives in the runtime. Today that is
four things:

- `chars_of` — codepoint awareness has to bottom out somewhere. Every
  other string routine (`String.slice`, `String.index_of`,
  `String.parse_int`) is built on it in Plum, which is what makes them
  all multi-byte-safe instead of each needing its own care.
- `panic_raw`, `args` — no Plum expression denotes them.
- `read_file_raw` — returns an ARRAY (empty = failure, one element =
  success) rather than a `Result`. A `Result` is an ordinary enum whose
  tags the backend assigns per program, so hand-written runtime IR
  cannot build one without hard-coding a layout decision made
  elsewhere. The prelude wraps it in Plum, where `Ok`/`Err` mean what
  the program says they mean.

#### Two real bugs this found

**`==` on aggregates compared ADDRESSES.** `icmp eq ptr` — so the
parser's own `tokens[pos] == expected` was always false and every parse
died with "expected 'let'". Found by compiling the parser, not by
reading the code.

Fixed with per-type equality functions, discovered by the same worklist
that discovers specializations and closed over component types (the
`seen` check is what makes a recursive type like `PExpr` terminate: it
needs exactly one function, which calls itself). **Comparing tags alone
would have been enough for the parser** — its expected tokens are all
nullary — **and quietly wrong for `Ident("a") == Ident("b")`**, so it
isn't done that way.

**The checker couldn't resolve source-declared associated functions.**
`Array.reverse(xs)` was only ever looked up in `builtin_sig`, never in
the program's own signatures, so prelude source could declare
`Array.reverse` and no call site could reach it. Now a real declaration
is checked first — the same shadowing order used everywhere else, and
the real compiler's, where the prelude IS a declaration.

Also: `emit-llvm` on a single file used to type-check the user's
program BEFORE the backend prepended the prelude, which checked the
wrong program and rejected every call to a prelude function without a
`builtin_sig` entry. `typed_program` already checks; the duplicate call
is gone.

#### Also added

`emit-llvm` accepts a project DIRECTORY (a real Plum project is several
modules); `Int.to_float()` in checker and backend (`sitofp`); and
`String.parse_float` is written directly rather than delegating to a
JSON number parser as the real prelude does — right there, absurd here.

#### Verified

| check | result |
| --- | --- |
| self-hosted lexer+parser compiled by the self-hosted backend, vs corpus goldens | **98/98** |
| exec_corpus via the self-hosted backend | 16/17 (only `tuples`) |
| those vs. the real backend | 14/14 identical |
| `sh check bootstrap/self_host` | ok |
| typecheck_corpus rejections | 5/5 |
| exec_corpus check + interpreter run | 16/16 |
| corpus goldens (native `sh`) | 98/98 |
| Rust suites | 14/14 |

**What is left.** The backend does not yet compile the compiler's own
`typecheck/` or `codegen/` modules — those need `list_dir`/`is_directory`
and a few more prelude functions, and `interp.plum` needs tuples. The
front end compiling itself is the milestone; the whole compiler
compiling itself is the next one.

### SELF-HOSTING: the fixed point (2026-08-16)

```
./bootstrap/bootstrap-check
  stage 1 -> IR ...
  stage 2 -> IR ...
  FIXED POINT: stage-2 and stage-3 IR are byte-identical (72172 lines)
  stage 2 vs corpus goldens: 98 pass / 0 fail
  stage 2 type-checks the whole compiler: ok
```

**Plum compiles itself.** The self-hosted compiler, compiled by the
self-hosted backend, is the same compiler — proven the standard way:

- **stage 1** — `sh`, the self-hosted compiler built by the *Rust*
  compiler
- **stage 2** — `sh2`, the self-hosted compiler built by **stage 1**
- **stage 3** — the self-hosted compiler built by **stage 2**

Stage 1 and stage 2 are different binaries built by different
compilers, so their agreeing on small inputs proves little. Stage 2 and
stage 3 are built by compilers *themselves written in Plum*, so any
construct the backend miscompiles — one it uses in its own source —
makes stage 3 diverge. **They are byte-identical, and so are the
binaries.** That is a stronger statement than "the tests pass": the
compiler compiled by itself is the same compiler.

Stage 2 also passes all 98 corpus goldens, type-checks the entire
compiler, runs every exec_corpus fixture, and runs the compiler under
its own interpreter — every mode, identical output to stage 1.

`bootstrap/bootstrap-check` runs the whole thing in about four seconds.

#### What the last mile took

**Flat-namespace collision, finally biting.** The prelude's private
helper `array_contains_acc` silently shadowed `interp.plum`'s own
function of that name, and the compiler failed to type-check itself
with "argument 0: T != Value". Prelude internals are now prefixed
`plum__`: a prelude's private helpers must not be able to collide with
user code, while public prelude names deliberately stay unprefixed —
those are the API, and shadowing one is the user's prerogative.

**Directory access** (`list_dir`, `is_directory`) via the same C shims
`dir_shim.c` the real compiler uses — already linked into every binary
`plum compile-ir` produces, so declaring them was all it took. No second
copy of the platform code, and no way for the two backends to disagree
about what `list_dir` means. The `Result` wrappers are written in Plum
in the prelude, for the same reason `read_file` is: runtime IR must
never hard-code a variant tag the backend assigns per program.

**String primitives** `starts_with`/`ends_with`/`contains`/`split` in
the runtime. These stay primitives rather than moving to the prelude
because they are byte-level; written over `chars_of` in Plum they would
become codepoint-level, which for `starts_with`/`ends_with` is an
observably different function. `split` with an empty separator is
defined as `chars_of` — the real compiler defines `chars_of` in terms of
that case of split, so the two agree from opposite directions.

#### What is still not self-hosted

The Rust toolchain is still needed for two things, and neither is a
Plum-language gap:

- **`plum compile-ir`** — assembling and linking the emitted `.ll`.
  Plum has no process-spawn builtin, so the self-hosted compiler cannot
  invoke `clang` itself. Even a finished self-hosted compiler would
  delegate this step.
- The **C shims** (`dir_shim.c` and friends), which are platform glue
  the real compiler also links rather than implements.

Everything between source text and LLVM IR — lexing, parsing, type
checking, monomorphization, code generation, and the prelude — is Plum
compiling Plum.

And the backend still **leaks**: no refcounting, no FBIP. `sh2` is a
correct compiler that never frees a byte. That was a deliberate v1 cut
(see Stage 5's own section) and it is the single biggest thing between
this and a backend anyone would use for a long-running program.

### The self-hosted backend frees memory (2026-08-16)

**`sh2` no longer leaks.** Type-checking the whole compiler:

| | peak RSS | time |
| --- | --- | --- |
| leaking (`cg_rc_enabled = false`) | 6,595 MB | 1.40s |
| reference counted | **63.8 MB** | 2.97s |

**101x less memory, about 2x slower** — and the bootstrap fixed point
still holds, byte-identical. That trade is exactly what precise
reference counting without reuse costs, and it is exactly what Perceus
exists to buy back.

#### Layout

Every heap cell now begins with a reference count, and every offset is
expressed relative to it:

```
Str:     { i64 rc, i64 len, i8 bytes[len], i8 0 }
Array:   { i64 rc, i64 len, elem0, ... }
Struct:  { i64 rc, field0, field1, ... }
Enum:    { i64 rc, i64 tag, payload0, ... }
Closure: { i64 rc, ptr code, capture0, ... }
```

One allocation point (`@plum_alloc`) initialises it to 1.

#### The discipline

1. Every expression evaluates to an OWNED reference (+1).
2. A slot — a `let`, a parameter, a pattern binding — holds an owned
   reference and releases it when the slot dies.
3. Reading a variable produces a NEW owned reference; reads increment.
4. A call CONSUMES its arguments; a return carries its +1 out.
5. An aggregate OWNS what is stored into it.

Every rule is local: no liveness analysis, no reuse. This is precise
reference counting, the thing Perceus is an optimization *of* — the
real backend's FBIP pass would sit on top of exactly this, and the cost
of not having it is redundant inc/dec pairs, not incorrectness.

Supporting machinery: per-type release functions generated by the same
worklist that generates equality functions and specializations, with the
same `seen` guard (a recursive type gets one function that calls
itself); null-initialised slots so release-on-overwrite is safe on the
first write, which is what lets a `let` inside a loop free the previous
iteration's value.

**Where the typed tree pays off.** `plum-ir/src/fbip.rs` cannot track
function parameters at all — its own comment gives the reason, "no type
checker in this IR to prove one is heap-shaped", the documented root
cause of the gap-1 work reverted earlier in this project. Here every
node's type is known, so `cg_is_heap` is total and exact and parameters
are counted like anything else.

#### Three bugs, and what they have in common

**Copied elements had no references of their own.** `push`/`set`
`memcpy` the old elements into a new cell — which copies POINTERS, so
both arrays referenced the same children and both would release them.
Fixed by incrementing the copy's elements.

**Borrowed elements were passed to closures that consume them.**
`Array.map`/`filter`/`fold` loaded an element and handed it straight to
the closure, which under rule 4 releases it — freeing an element the
source array still held. `filter` needs two references when it keeps an
element: one for the predicate to consume, one for the output.

**Pattern bindings were bound unconditionally.** This is the
interesting one. Bindings are computed before any tag is checked, which
is safe for a LOAD — `cg_payload_offset`'s over-allocation rule
guarantees the address is mapped — and *not* safe for an INCREMENT,
because an increment writes through that pointer. `render_token`'s
fifty-arm match reads `TokIdent`'s string payload while looking at a
`TokInt`, so the "pointer" is the integer 1, and the increment writes to
address 1. The same applied to the STORE: a non-matching arm left its
slot holding garbage that the function's exit release then wrote
through. Both moved into the arm's body block, which only runs once the
tag has matched.

All three are the same mistake in different clothes: a design that was
correct while reads were passive became wrong the moment reads acquired
ownership. Over-allocating so that speculative loads are safe was a good
decision; it just does not extend to speculative *writes*.

#### Closures, and where the last 99% went

A closure's captures are invisible from its TYPE — `(Int) -> Int` says
nothing about what was captured — so nothing generated per-type can walk
them. Only the literal's own creation site knows, so that is where the
walker is generated, and every closure cell carries a pointer to it:

```
Closure: { i64 rc, ptr code, ptr release, capture0, ... }
```

The real backend spends this same word for this same reason. `CgSlot`
had to start carrying each slot's full `ITy` alongside its
representation, because a release function is named after the full type
(`@plum_rel_Box_Int`) and a `CgTy` has already thrown the argument away.

That took peak RSS from 6,595 MB to 760 MB. **The remaining 92% was
somewhere much less interesting**: rule 4 says a call consumes its
arguments, but that rule is about PLUM functions, which release their
parameter slots on the way out. The runtime's own functions are
hand-written IR that does no counting, so they BORROW — and every
`.concat()`, `.to_string()`, `.len()`, `println()`, `==` and
`chars_of()` was dropping two references on the floor. The compiler
concatenates strings constantly. Releasing those arguments at the call
site took 760 MB to 63.8 MB.

Worth naming as a lesson: the interesting-looking leak (closures, needing
a new word in the layout and a new kind of generated function) was worth
8.5x. The boring one — a convention mismatch at the boundary between
counted and uncounted code — was worth another 12x on top.

`cg_rc_enabled` still gates every inc, dec, null-init and slot release.
Flipping it to `false` emits the leaking version, which is a one-line
way to tell a counting bug apart from any other kind.

#### A note on what caught what

The exec_corpus never noticed any of this. During the layout change two
offset edits silently failed to apply and 16/17 fixtures still passed;
`bootstrap-check` caught it immediately. Every one of the three
ownership bugs was found the same way — by compiling the compiler, whose
own source exercises wide matches, nested enums and array plumbing far
harder than any fixture does. **The fixed point is not a milestone, it
is the test.**

#### Borrowing, and a measurement that redirected the work

The obvious next step after precise counting is removing redundant
inc/dec pairs. The biggest class is BORROWS: reading a variable,
using it, releasing it again — when the slot holds a reference for the
whole of a straight-line operation, so the object cannot be freed
underneath it. `cg_borrow` skips both the increment and the release
when the operand is a plain variable read, which covers `xs[i]`,
`p.field`, `s.len()`, `println(msg)`, `a.concat(b)`, `a == b` and
`match tok { .. }`.

That is the first step of what FBIP does properly: Perceus decides it
with a liveness analysis (`plum-ir/src/fbip.rs`'s `mark_last_uses`),
this decides it syntactically — weaker, but needs no analysis and covers
the overwhelmingly common shapes.

**It removed 30% of the reference-counting operations and bought 3% of
wall clock:**

| | RC ops | time |
| --- | --- | --- |
| leaking | 0 | 1.32s |
| counted | 23,155 | 2.88s |
| counted + borrows | 16,288 | 2.80s |

That is the useful result, and it is not the one expected. **The
counting is not what costs 2x — the freeing is.** The leaking version
never calls `free` at all, and that is the entire difference. So
eliminating more inc/dec pairs, including via a real last-use analysis,
has low expected value here.

The win that is left is FBIP's OTHER half: **reuse**. When a cell dies
at a point where a cell of the same shape is about to be allocated,
Perceus writes into the corpse instead of calling `free` and then
`malloc`. That removes the traffic this measurement says is dominant,
rather than the traffic it says is cheap. It also needs last-use
analysis as a prerequisite — so the analysis is still worth building,
just for reuse rather than for skipping increments.

### Profiling the backend, and what it found (2026-08-16)

The borrow measurement said the counting was not the cost. Rather than
guess again, the backend grew allocation counters — `PLUM_RT_STATS=1`,
the same instrument the real backend has as `PLUM_ALLOC_STATS`, and
added here *before* guessing rather than after.

The first reading:

```
[plum-rt] alloc n=21351500 bytes=6779281738 | str=18681380 concat=219002 array=1055684
```

**21.4 million allocations, and 87% of them were `plum_str_new`.** Two
causes, one obvious and one not.

**String literals allocated on every evaluation.** A literal emitted raw
bytes to be copied into a fresh cell each time it was reached. They are
now complete STATIC cells — refcount, length and bytes — used directly,
with a refcount of `-1` marking them IMMORTAL: `plum_rc_inc` and every
release skip a negative count. A sentinel rather than a large positive
number, because "negative means static" cannot be reached by counting.
Worth 2.3 million allocations, and less than expected.

**The checker was inlining every top-level 0-parameter binding.** This
was the real one. `infer_ident` returned the global's BODY in place of
the reference, so `PLUM_DIGIT_CHARS = chars_of(PLUM_DIGITS)` in the
lexer was re-split into 10 fresh string cells *for every character
classified*. The typed tree now records a `TGlobalRef`, and the backend
gives each global a module-level slot filled once by
`@plum_init_globals` — which is exactly what the real backend does, and
what this one had no equivalent of.

```
before: alloc n=19018023  str=16348552
after:  alloc n= 2797519  str=  503962
```

**A 32x reduction in string allocations from one checker change.** It is
also the second time in this project that character-classification
globals have been the hot spot — the same finding, arrived at
independently, in two different implementations.

`Array.concat` was also given a single-allocation implementation (the
prelude's is `array_concat_acc(a.push(b[i]), b, i + 1)`, one push per
element, each copying the whole array — O(n²) bytes). The call site picks
between two runtime variants by the instantiated type argument, since a
result holding heap elements needs its own reference to each and one
holding `Int`s must not touch them. That kind of type-directed choice is
something only a backend with the typed tree can make.

#### Reuse, without a last-use analysis

The remaining bytes were one shape: `acc = acc.push(x)` in an
accumulator loop, where every push copied the whole array — the same
accidental O(n²) this project has now hit in five separate places.

Reuse needs to know the receiver is dead, and in general that needs a
last-use analysis: a dynamic `rc == 1` check is NOT sufficient, because
under borrowing the slot's own reference IS the 1, so a unique count
does not mean the value is unobserved.

But there is one shape where deadness is SYNTACTIC: a self-rebinding
assignment. In `acc = acc.push(x)` the slot is overwritten with the
result, so nothing can observe a mutation of the old value. That makes
it sound to hand the slot's reference straight into the operation and
let it grow the array in place when the cell turns out to be uniquely
referenced and malloc already gave it the room — `malloc_usable_size`
again, no capacity word.

Two details decide whether this is a win or a disaster:

- **The copy path must release the old array.** Its release function is
  passed IN, because the runtime cannot name it — it depends on the
  element type. Forgetting it leaked every intermediate and cost 3.1 GB
  of peak RSS, which is how it was found.
- **The copy path must over-allocate.** Growing by doubling is what
  makes the next pushes hit the in-place path and the whole
  accumulation amortized O(1), rather than accidentally so. The slack
  is invisible: `len` is still the true element count.

The same treatment applies to `acc = acc.concat(x)` for strings.

#### Where it stands

| | allocated | peak RSS | time |
| --- | --- | --- | --- |
| leaking | 6.78 GB | 7,155 MB | 1.41s |
| counted, unoptimised | 6.78 GB | 65 MB | 3.07s |
| **counted + all of the above** | **94.7 MB** | **47.9 MB** | **0.24s** |

**Six times faster than the leaking build it started from, on 1/150th
the memory.** The counting was never the cost; the O(n²) copying was,
and it was there in the leaking build too — reference counting is what
made it visible and what made fixing it possible, because reuse needs to
know a value is dying.

A general last-use analysis is still the right long-term answer — it
would catch the accumulator patterns that are NOT self-rebinding, like
`f(acc.push(x))` in a tail-recursive loop, which is how the parser is
written. That is now a smaller and much better-understood piece of work
than it looked.

### The fixed point is now a test (2026-08-16)

`crates/plumc/tests/bootstrap_fixed_point.rs` runs the whole bootstrap
chain — stage 1 built by the Rust compiler, stage 2 built by stage 1,
stage 3 built by stage 2 — and asserts the stage-2 and stage-3 IR are
byte-identical. It also asserts stage 2 is a *working* compiler (a
fixed point that produces a broken compiler is still a fixed point), by
having it parse a corpus fixture and type-check the whole compiler.

This was overdue. The project's strongest invariant had no automated
guard, and the evidence that it needs one is not hypothetical:

- During the refcount **layout** change, two offset edits silently
  failed to apply. 16 of 17 exec_corpus fixtures still passed.
  `bootstrap-check` caught it immediately.
- Every one of the **three ownership bugs** in the reference-counting
  work showed up here first. None was visible in the corpus.

It costs about 65 seconds, most of it building stage 1, which roughly
doubles `cargo test --release`. That is a fair price for the one test
that can tell you the compiler compiled by itself is the same compiler.

It deliberately does not go through `./sh`: that wrapper caps an
interactive runaway's memory, and a test should fail by asserting rather
than by being killed.

### Where the self-hosted backend actually lands

| | time to type-check the whole compiler |
| --- | --- |
| `sh` — built by the Rust compiler, full FBIP | 0.168s |
| `sh_fin` — built by the self-hosted backend | 0.229s |

**Within 1.36x of the real compiler**, from a backend that leaks nothing,
has no liveness analysis, and whose entire reuse story is one
syntactic special case. The average allocation is now 34 bytes — the
O(n²) copying that dominated everything is gone.

A general last-use analysis remains the right long-term answer, and
would catch the accumulators that are not self-rebinding (`f(acc.push(x))`
in a tail-recursive loop, which is how the parser is written). But the
measured upside is now small, and saying so is more useful than building
it: the backend is fast enough that the next real work is elsewhere.

**That last sentence was wrong, by an order of magnitude.** The shape named
here was right; the size was not. Measured on 2026-08-17: 194 MB versus
5.2 MB of peak RSS on a 20,000-item accumulation, and 1458.7 MB versus
113.6 MB for this compiler emitting its own IR. See "Move-on-last-read in
the self-hosted backend" below. The error came from measuring peak RSS on
a workload where the self-rebinding special case already covered the
dominant path, rather than measuring bytes allocated.

### Tuple type annotations (2026-08-16)

Plum had tuple VALUES from the start and no way to write their type. A
parenthesized list of two or more types with no arrow was a parse
error — `Parser::parse_type` rejected it rather than silently treating
it as something it wasn't, which was the right call while there was
nothing to build.

That gap was invisible to the real type checker, which infers
everything, and fatal to the self-hosted one, which requires every
top-level signature to be annotated. `bootstrap/exec_corpus/tuples/` was
literally unrepresentable there:

```
let swap (t) = match t { (a, b) => (b, a) }
        ^ no way to annotate this
```

`Type::Tuple` now exists, and `(A, B)` parses. The decision table is
small and each entry matches an existing rule rather than inventing one:

| written | means | why |
| --- | --- | --- |
| `(A, B) -> C` | function type | unchanged |
| `(T)` | `T` | grouping, matching the value-level rule exactly |
| `()` | `Unit` | the unit VALUE's type — not an empty tuple, which would be distinct and useless |
| `(A, B)` | tuple type | the new case |

The fixture is annotated now, and **17/17 exec_corpus fixtures
type-check and run** — the documented exception is gone.

**Neither backend compiles tuple values.** `plum build` rejects a
signature involving one ("codegen only supports Int/Float/Bool/Unit/Str/
Array[T]/Task[T] or a non-generic struct/enum"), and so does the
self-hosted backend. Tuples remain interpreter-only; this change is the
syntax half, and saying so is more useful than implying the feature is
finished.

Both parsers agree on the new syntax — the self-hosted parser renders
`(tup Int String)` identically, verified across all 99 corpus fixtures.

### `match` exhaustiveness in the self-hosted checker (2026-08-16)

The self-hosted checker now rejects a `match` on an enum that leaves a
variant uncovered, with the same message the real compiler produces:

```
match is not exhaustive — missing variant(s): Triangle
```

**It implements the real compiler's rule deliberately, rather than a
stricter or cleverer one.** Two checkers that disagree about which
programs are valid is a worse outcome than either rule on its own, and
the real compiler's rule is already reasoned through in its own source:

- Only an ENUM scrutinee is checked. A struct or tuple has a single
  shape, trivially covered by any arm that type-checks against it; an
  `Int` or `String` scrutinee has no finite set of values to enumerate.
- A trailing catch-all — wildcard or bare binding — exempts the match,
  because it accepts anything by construction.
- Only TOP-LEVEL tags count. `Some(Ok(x))` covers `Some`, not "`Some`
  whose payload is `Ok`".
- An or-pattern covers every tag its alternatives name.
- A GUARDED arm still counts as covering its tag.

The last two points make the check **incomplete rather than unsound**:
it never rejects a match that genuinely covers everything, and it can
accept one that still fails at runtime on a nested shape or a failed
guard. That is the direction this project consistently chooses, and it
is the same reasoning `movecheck.rs`'s permissiveness records.

The self-hosted compiler's own sources needed no changes — every `match`
in them was already exhaustive, which is a mildly reassuring thing to
learn from turning a check on.

`bootstrap/typecheck_corpus/non_exhaustive_match/` pins the agreement
between the two checkers, not just the rejection.

### Real module scoping in the self-hosted checker (2026-08-16)

Two modules can now declare the same function name. That sounds small;
it was the project's longest-running papered-over limitation.

Before, `check`/`run`/`emit-llvm` concatenated every module's items into
one flat program and `find_sig` scanned it in order, first-wins. The
consequences were not theoretical:

- `interp.plum` and `typecheck/infer.plum` both wanted a `bind_params`
  — one binding runtime VALUES, one binding parameter TYPES. The
  checker resolved both to whichever came first and rejected the
  compiler's own source. Fixed at the time by renaming one, and
  recorded as a workaround.
- The prelude's private helper `array_contains_acc` silently shadowed
  `interp.plum`'s own function of that name, and the compiler failed to
  type-check itself with "argument 0: T != Value".
- Global slots needed a dedupe pass because two modules both declared
  `UPPERCASE_CHARS`.

#### The rule

Every item now carries the MODULE it was declared in — the name of the
directory holding its file, or `""` for a file at the project root. The
parser writes `""` and knows nothing about it; the project loader stamps
the real name on afterwards, which keeps module identity out of the
grammar where it does not belong.

**Functions and globals are scoped.** An unqualified name resolves in
its own module, then in the root module (where the prelude lives), and
never in a sibling. Crossing a boundary means naming it —
`lexer.tokenize` resolves IN `lexer`, rather than looking `tokenize` up
in one flat table as it used to.

**Types and variant tags remain flat, deliberately.** `parser.PExpr` and
a bare `PExpr` resolve identically, and `parser.plum` matches on
`TokLet` — a tag declared in `lexer` — without qualifying it. Scoping
those would be a language change rather than a checker change, and the
sources depend on the current rule. DESIGN.md's module-system section
already documents the flat tag namespace as a deliberate boundary.

#### The backend had to keep up

A `TFnCall` now records the module the checker RESOLVED to, not just the
name. That is not recoverable afterwards: an early attempt had the
backend look the module up by name, which picked whichever module came
first — reintroducing exactly the ambiguity the change removes. It
emitted one `helper` for a program with two.

Symbols are module-qualified (`@plum_alpha_helper`, `@plum_beta_helper`),
as are global slots.

#### Verified

A two-module program where both modules declare `helper` and
`describe` with DIFFERENT types compiles and runs correctly, producing
four distinct symbols and output identical to the real compiler. Both
checkers reject reaching into a sibling module unqualified.

`bind_param_types` is `bind_params` again — the workaround is undone,
and the compiler type-checks itself with both modules using the name.

### Bounds checks, and tuple values in the self-hosted backend (2026-08-16)

**Array indexing is bounds-checked.** It was an unchecked read before —
a deliberate v1 cut, and a genuinely unsafe one that got worse when
reference counting arrived: an out-of-range read returned whatever
followed the cell, and that value was then INCREMENTED, writing through
a pointer made of arbitrary bytes. The check is a compare and a
never-taken branch, and the failure message is the real backend's
verbatim ("array index out of bounds"), so a program that indexes past
the end behaves identically under both. Negative indices are caught too.

**Tuples are compiled.** A tuple is an anonymous struct — `{ i64 rc,
elem0, elem1, ... }`, laid out by the same `cg_field_offset` a named
struct's fields use. Construction, positional destructuring, structural
equality and release all reuse the struct paths rather than growing a
second copy of the same loops: a tuple pattern is handed its element
INDEX as a field name and goes through `cg_pat_fields` unchanged.

**The self-hosted backend now compiles all 17 exec_corpus fixtures.**

#### The real backend still cannot, and the reason is interesting

`plum build` rejects a signature involving a tuple. The obstacle is
documented at length in `codegen_cli.rs` already: `lower.rs` tags every
tuple by ARITY alone (`tuple_tag(2)` → `"2Tuple"`), and `tag_fields` is
a flat map from tag to field types — so `(Int, String)` and
`(Bool, Bool)` would need one tag to carry two different layouts
simultaneously. Fixing it means type-specialized tuple tags threaded
through `plum_types::Infer` and `lower.rs`, which that comment calls
"real, cross-crate work".

**The self-hosted backend never had this problem, and it is worth saying
why.** It has no tag table for tuples to collide in: a tuple's element
types come from the TYPE at each use site, read off the typed tree.
`ITTuple([Int, Str])` and `ITTuple([Bool, Bool])` are simply different
types, and `cg_of_ity` turns each into its own `CgTuple`. The tag-table
design made tuples hard; the typed tree made them fall out.

So the bootstrap backend now compiles three things the real one can't:
`let empty = []` (uninferable there), a closure-typed struct field, and
tuples. Not a claim of superiority — the first two are deliberate
rejections — but the third is a genuine capability the older design
made expensive.

### Tuples in the real backend: type-specialized tags (2026-08-16)

`plum build` compiles tuples now. The obstacle was recorded in
`codegen_cli.rs` long before this — `lower.rs` tagged every tuple by
ARITY alone (`tuple_tag(2)` → `"2Tuple"`), and codegen's `tag_fields` is
a flat map from tag to field types, so `(Int, String)` and
`(Bool, Bool)` would have needed one entry to describe two layouts. That
comment also named the fix and called it "real, cross-crate work":
thread a span-keyed side channel from inference into lowering, mirroring
`resolve_empty_array_elem_types`.

That is what this is. `Infer` records every tuple expression's and tuple
pattern's element types by span; `resolve_tuple_elem_types` resolves
them against the final substitution (including the template fallback for
a tuple written inside a still-generic body); `lower.rs` folds them into
the tag. `(Int, String)` and `(Bool, Bool)` are simply different tags
now.

Construction and destructuring must agree on the spelling, so both go
through one shared `specialized_tuple_tag` — a naming mismatch between
them would be a silent miscompile, not a build error.

#### The regression this caused, and the fix

`channel[T]()` evaluates to a `(Sender[T], Receiver[T])`, but codegen
BUILDS that tuple itself (`ir::Expr::Channel`, which carries no type)
and registers its own `tag_fields` entry under the legacy arity tag.
Specializing the *destructuring pattern* gave it a tag the construction
never produced — and the match silently found no arm. The channel test
program compiled, ran, exited 0, and printed nothing.

Channels therefore keep the legacy tag, tested for by their element
types being `Sender`/`Receiver`. That also leaves their existing
one-element-type-per-program limitation exactly as it was, rather than
half-lifting it.

**Superseded the next day** — the full lift landed by giving
`ir::Expr::Channel` the tag itself, so the construction site stops being
the one place that can't know it. See "Channels of more than one element
type".

#### Where tuples stand

Both backends compile them. `bootstrap/exec_corpus/tuples` builds and
runs under `plum build` and under the self-hosted backend, with
identical output, and four new codegen tests pin the cases that were
impossible before — including two same-arity tuples with different
element types in one program, and a tuple as a declared parameter and
return type.

The two backends now agree on 15 of 17 fixtures; the remaining two are
`plum build`'s own rejections (`let empty = []` is uninferable there,
and it refuses a closure-typed struct field). Both were fixed the next
day — see below — and the backends agree on all 17.

## Dead-function elimination, and a gate that never gated (2026-08-16)

The two remaining `plum build` rejections were assumed to be deliberate
design limits. Exactly one of them was.

**`let empty = []`.** Rejected with "cannot determine the element type
of the empty array literal — it's never used anywhere that would pin its
element type to something concrete". An accurate description of the
situation, wrongly treated as a failure. Every operation that could
observe an element — `push`, indexing, `map`/`filter`/`fold`, iteration,
comparison against a non-empty array, concatenation — unifies the
element variable with something during type-checking. So a variable
still free once the whole program is solved proves that no element ever
enters or leaves that array, and every choice of element type is
observationally identical: `len()` is 0 and `to_string()` is `"[]"`
whatever we pick. `resolve_empty_array_elem_types` defaults it to `Unit`
now.

The asymmetry with its two siblings is load-bearing and worth stating:
`resolve_closure_types` and `resolve_tuple_elem_types` still ERROR on an
unpinned component, because a closure's parameter or a tuple's element
genuinely is consumed somewhere, and picking a type for it would change
what the program computes.

**The closure-typed struct field.** This one was not a design limit at
all — it was a bug that had been shipping since concurrency landed, and
its own doc comment described the correct behavior the whole time.

`check_no_closure_or_task_fields` is a whole-program rejection gated on
"does this program use `spawn` or a channel anywhere", and it promises
that "a program that never actually spawns anything is completely
unaffected". But the gate is computed by walking the functions that
reach codegen, and `monomorphize::plan` deliberately seeds EVERY
non-generic function into the program unconditionally (it must —
`MonoPlan::functions` fully replaces the lowered function list). Only
GENERIC prelude functions were ever dropped. The prelude's
`http_serve_loop` is not generic, and it contains a `spawn`.

So every program ever compiled set `needs_spawn_runtime`, the gate was
permanently open, and the restriction applied universally. A hello-world
program was emitting 256 functions, including the entire HTTP server and
a `pthread_create`.

The fix is a real one rather than a special case for prelude code:
`plum_ir::prune::prune_unreachable`, run by `plumc` between
monomorphization and codegen, drops every function no root can reach.
Hello-world goes from 256 emitted functions to 52 (all runtime helpers,
no Plum functions), the gate means what it always claimed, and a program
that genuinely calls `http_serve_loop` still pulls the `spawn` in and is
still correctly subject to the check.

Two things the pass has to get right:

- **Conservative in the retaining direction.** The only thing that makes
  a function live is its name appearing syntactically in a live body,
  with no scope analysis — a local shadowing a top-level function's name
  keeps that function alive. Wasteful, never wrong. Erring the other way
  drops a function codegen still calls: a link failure at best. The
  `Expr` walk is exhaustive with no `_` arm so a new variant carrying a
  sub-expression fails to compile rather than silently making what it
  references look dead.
- **`plum test` has more than one entry point.** `run_tests_native`
  compiles the shared IR body once and appends a separate `emit_main`
  per discovered test; each test is an entry point reachable from `main`
  in no way at all. Its doc comment explicitly relied on "the `entry_fn`
  passed in doesn't affect WHICH functions end up in the body" — an
  invariant this pass invalidates. Test names are passed as extra
  reachability roots (`compile_program_to_ir_roots`). This was caught by
  three failing tests, not by reading.

The pass runs only on the codegen path. `plum-interp` can be asked to
invoke any top-level function by name, so it has no single entry point
to root a walk at.

## Channels of more than one element type (2026-08-16)

`channel[Int]()` and `channel[String]()` could not appear in the same
program. The compiler said so loudly:

> codegen does not yet support more than one distinct `channel[T]()`
> element type in the same program — tuple tagging isn't
> type-specialized per element type yet

That message named its own blocker, and the blocker was gone. Tuple
tags became type-specialized the day before; the message was stale.

`channel[T]()` evaluates to a `(Sender[T], Receiver[T])` tuple, and tuple
tags used to be arity-only, so both channels wanted the single flat
`tag_fields["2Tuple"]` entry to describe two different layouts. That is
not a cosmetic collision — `.recv()`'s `word_to_value` conversion depends
entirely on the Receiver's declared inner `CgType` being correct, so
mis-tagging the second element type is a memory-safety bug. Hence the
rejection rather than a silent miscompile.

When ordinary tuples were specialized, channels were deliberately left on
the legacy arity tag rather than half-lifted, and the reason is the
interesting part: **this tuple has no construction site in the source.**
Codegen synthesizes it, from `ir::Expr::Channel`, which carried no type
at all. Specializing the destructuring pattern alone gave it a tag the
construction never produced — the match then found no arm, and the
program compiled, ran, exited 0 and printed nothing. That is the failure
mode this whole area keeps producing, and it is why the fix is shaped the
way it is.

So `ir::Expr::Channel` now carries `tag: String`. Not the element type —
`T` stays as erased as ever, and the interpreter ignores the field
entirely. It carries *the tuple's tag*, computed by the same
`lower::specialized_tuple_tag` the destructuring pattern calls, from the
same two end types, which `Infer`'s `channel[T]()` arm now records by span
like any other tuple's. Three call sites, one function, equal inputs: the
construction, the pattern, and `register_channel_tag`'s `tag_fields` entry
cannot disagree about the name. Making a mismatch unrepresentable is the
only defense that works here, because a mismatch produces no error.

`register_channel_tag` registers one entry per distinct `T` and the
rejection is gone. Verified with four element types at once (`Int`,
`String`, `Bool`, and a struct), across real spawned threads, with the
native build and the interpreter agreeing.

## `Ref[T]` in native codegen (2026-08-16)

The last interpreter-only language feature. `ref(v)`/`.get()`/`.set(v)`
worked under `plum run` and had no native representation at all, so
`examples/shared_mutability` was the one example that could not be
built. Both backends now agree on it, output for output.

### Representation: closer to `Array` than to `Task`

The interpreter implements `Ref` as `Rc<RefCell<Value>>`, deliberately
outside its own toy refcounted heap. That decision doesn't port — native
codegen has no `Rc` to borrow. The question was which existing shape
`Ref` belongs to.

It is `{ i64 refcount, i64 value }` — a real, Plum-managed, refcounted
cell, so `dec_fn_for` returns a real release function for it, unlike
`Task`/`Sender`/`Receiver`/`CStr`, which all return `None` because they
have no refcount word to touch. The right analogue is `Array`: one
`@plum_rc_dec_ref_<mangled>` per distinct inner `CgType`, discovered as
codegen walks each body, exactly the way array element types already
are.

That also means **no tag**, and therefore none of the type-specialized
tagging work that tuples and channels each needed. A `Ref`'s layout is
pinned at compile time by `CgType::Ref(inner)`, so nothing ever recovers
it from a runtime tag word, so there is no flat-`tag_fields` collision
to escape. Better still, the inner type is available at the one place it
is needed: `ref(v)` learns it from `v` itself. No span-keyed side channel
through `Infer`, no new `ir::Expr` field. The construction site is the
one place the type was already in hand — the exact opposite of
`channel[T]()`, whose tuple has no construction site in the source at
all.

### What FBIP must not do

`Ref` must ALWAYS mutate in place and ALWAYS stay visible through every
alias. "Maybe reuse, maybe copy depending on refcount" is exactly
backwards for it, so `.set()` has no refcount branch of any kind.

DESIGN.md decided in 2026-07 that `fbip` gets only exhaustive-match
passthrough for `RefNew`/`RefGet`/`RefSet` — no `is_syntactically_heap`
case, `mark_reuse` never touches them. That still holds, and it is
load-bearing rather than incidental: `is_syntactically_heap` feeds
`known_heap`, which is what makes a binding a reuse candidate. Adding
`RefNew` to it to get automatic release would also have made every `Ref`
eligible for reuse-in-place — and a `match` could then try to reuse a
`Ref` cell as a `Ctor` cell, which have different layouts.

### The consequence, stated plainly

Because `fbip` never tracks these nodes, it never emits a `Dec` for them.
So:

- A `Ref` cell is never released by scope exit. **Measured: 63MB for
  2,000,000 `ref()` calls**, ~32 bytes each, unbounded. **Fixed on
  2026-08-17 by `plum_ir::refdrop` — see below.**
- `.get()` increments the value it hands back, and nothing balances it.
  Still true.

The increment is not optional. Without it,
`let p = r.get(); r.set(other)` leaves `p` dangling — a use-after-free.
With it, the cost is a leak. Leaking is the correct side to err on, and
it is consistent with what `Ref` already accepted: DESIGN.md's "Cycle
collection" section chose Swift's answer (no collector), and a cell in a
reference cycle never reaches refcount 0 either.

What DOES work is the release that matters most in practice: `.set()`
releases the value it overwrites. Measured — 2,000,000 `.set()` calls
each allocating a fresh `Point`, flat at 5.0MB peak RSS, and under ASan
a 50,000-iteration version leaks 100 bytes in 5 allocations (constant,
not proportional). No use-after-free, no double-free.

Ordering is what makes that safe: load the old word, store the new one,
release the old **after**. Release-before-store would free the value in
between and store a dangling pointer whenever the two alias —
`r.set(r.get())` being the obvious case, pinned by a test.

### Why releasing the cell is not a one-line fix (attempted 2026-08-16)

The obvious fix — add `RefNew` to `is_syntactically_heap` so
`insert_refcount_ops` protects and releases `Ref` bindings — was tried,
measured, and reverted. It is worth recording why, because the change
LOOKS correct and even compiles and passes casually.

The predicate split it needs is real and fine on its own. `Ref` must be
refcounted (or it leaks) but must never be a reuse-in-place candidate
(reuse is backwards for it, and a `Ref` cell has no tag word for a match
arm to overwrite as a `Ctor`). Those pull in opposite directions, but
`mark_reuse`'s stated invariant is a SUBSET relation — reuse may only
fire for names `insert_refcount_ops` protected — so widening only the
refcount predicate keeps it intact. Two predicates, `is_syntactically_
heap` and an `is_reusable_heap` that subtracts `Ref`, is the right shape.

`RefGet` cannot join it either way: its result has the INNER type, which
may be a scalar, and this IR has no type information to tell the cases
apart — `RcAnnotated` on a scalar is a hard codegen error, so a
`Ref[Int]`'s ordinary `let n = r.get()` would stop compiling. Only
`RefNew` is type-INDEPENDENT enough to add.

The actual blocker is deeper, and it is Perceus's own model. **The last
use of a value is assumed to CONSUME it**, so no trailing `Dec` is ever
emitted — ownership simply moves into whatever the last use was. That
holds for a `Ctor` field, a call argument, a return. It does NOT hold for
`RefGet`/`RefSet`, which only BORROW their base: the reference is read
and then dropped on the floor.

The measured result of adding the arm was `step()` emitting two
`@plum_rc_inc` calls and zero decrements — strictly worse than before,
since the leak remained and pointless increments joined it.

Emitting a scope-end release instead is not expressible in this IR:
`RcAnnotated { op: Dec, target, rest }` runs its decrement BEFORE `rest`,
and there is no "decrement after this expression produces its value"
node. It could be simulated by rewriting `Let { r, RefNew(v), body }`
into a temp binding — bind `body`'s result, decrement `r`, return the
temp — but that is a use-after-free for a shape that is perfectly legal
and works today:

```
let make_cell (n: Int): Ref[Int] = { let r = ref(n); r }
```

Here the binding's own value escapes as the body's result, so decrementing
at scope end frees the cell the caller just received. Guarding it needs a
tail-position analysis this pass doesn't have.

So the honest summary is that `Ref` needs a notion of BORROWED use
positions, not a wider heap predicate.

### `plum_ir::refdrop` — the borrow-aware pass (2026-08-17)

Built the next day. **63MB -> 5.0MB** for the 2,000,000-`ref()`
benchmark, and flat: the release is per-scope, not amortized.

It is a SEPARATE pass rather than a change to `fbip`, which is what
makes it safe to add at all. A `Ref` binding provably never enters
`fbip`'s `known_heap` — `is_syntactically_heap` has no `RefNew` arm, and
its `Var(n)` case only qualifies if `n` is already in the set — so
`fbip` ignores exactly the bindings this pass owns, and this pass ignores
everything else. That also preserves for free the property `Ref` needs
most: `mark_reuse` is gated on the same set, so a `Ref` can never become
a reuse-in-place candidate. No predicate split turned out to be necessary.

Verified inert for everything else, not just argued: a program with no
`ref` emits **byte-identical** IR before and after the pass existed.

The rule, for a variable bound to a `Ref` cell:

1. Every CONSUMING use gets an `Inc`.
2. Exactly one `Dec` at the end of the binding's scope.
3. BORROWING uses get nothing.

Borrowing uses need no increment because the binding holds the original
reference alive until (2) runs, so any read through the pointer inside
that scope is safe by construction. The borrow positions are
`RefGet`/`RefSet`'s base, and `Binary`'s operands — `Ref` equality is a
raw pointer compare, taking no ownership. A closure body is not
descended into at all: codegen already balances captures itself
(`codegen_closure_literal` increments each heap-shaped capture,
`closure_release$*` decrements it), so marking there would
double-increment.

**Rule 1 is what makes rule 2 unconditionally safe**, and that is the
whole design. The earlier attempt died on `let r = ref(n); r` — the
binding's own value escaping as the body's result. Here that bare `Var`
in return position is a consuming use, so it is incremented, and the
scope-end decrement releases only the binding's own reference. Escaping
through a `Ctor` field works for the identical reason. Both are pinned by
tests, as is a three-deep alias chain (`let b = a; let c = b`), where
each alias is a consuming use with its own scope-end release, so N names
for one cell net out exactly.

Scope-end decrement is expressed by binding the body's result to a
synthetic temporary (`refdrop$N` — `$` is unavailable to Plum
identifiers), since `RcAnnotated`'s decrement runs BEFORE its `rest` and
there is no "decrement after this produces its value" node. One
consequence worth naming: the body stops being in tail position, so a
call at the end of it is no longer a `musttail` candidate. That is a
correctness requirement rather than a lost optimization — a function
holding a `Ref` cell has cleanup to run after the call returns, so it
genuinely cannot tail-call its frame away.

Shadowing is handled precisely rather than over-approximated (which for
increment insertion would mean leaks, not just imprecision): every binder
— `Let`, `For`, match-arm bindings, closure params — removes its own name
from the tracked set for the scope it governs.

Confirmed under ASan across the aliasing, escaping, struct-field,
shadowing, nesting, and loop cases: **no use-after-free and no
double-free**, leaks only at the pre-existing baseline (a trivial program
with no `Ref` at all already leaks its own top-level locals).

What this does NOT fix is `.get()`'s unbalanced increment on
heap-shaped contents. That one needs the inner TYPE, which this IR does
not carry — the same reason `RefGet` cannot join `is_syntactically_heap`.
It is bounded by the number of `.get()` calls on a `Ref` whose contents
are heap-shaped, not by cell count, and `.set()`'s own release of what it
overwrites is unaffected (still flat at 5MB over 2M writes).

### Thread boundary

`Ref` is rejected from crossing `spawn`/`channel`, matching the
interpreter's `to_portable` exactly, and it is the sharpest case in that
family because BOTH available crossing mechanisms are wrong rather than
just one: a deep copy silently splits one shared cell into two
independent ones — precisely the semantics `Ref` exists to provide, so a
silent and invisible bug — while the verbatim pointer copy that
`Sender`/`Receiver` legitimately get would leave two threads racing on a
non-atomic refcount. Real cross-thread shared mutation needs atomics and
a lock, still deliberately deferred.

Both holes are closed: a directly `Ref`-typed capture by
`crosses_thread_boundary`, and a `Ref` hidden inside a struct field by
the whole-program `check_no_closure_or_task_fields` — which only started
meaning anything the same day, once dead-function elimination stopped
the prelude's unreachable `spawn` from holding its gate permanently open.

The spawn-rejection message now explains the reason specific to the type
it rejected. It previously told every case that "a task handle is tied to
the thread that created it, so there's nothing meaningful to deep-copy",
which for a `Ref` is not merely unhelpful but false — deep-copying a
`Ref` would be entirely meaningful, and wrong.

### `==` is identity

Two distinct cells holding equal contents are NOT `==`. That is what
makes `Ref` useful for aliasing at all, so `eq_fn_for` returns `None`
(there is no structural comparison to call) and equality lowers to a raw
pointer compare — the direct analogue of the interpreter's `Rc::ptr_eq`.
`.to_string()` is likewise unsupported: printing contents would render
two genuinely distinct cells identically.

## Nothing was ever freed (2026-08-17)

Chasing the remaining `Ref` leak turned up something much larger. The
`.get()` leak was a symptom; the disease was that **the native backend
never released anything at all.**

`fbip`'s own comment stated it, and had for a long time:

> `RcOp::Dec` is only ever emitted by this pass's `Let` arm's "name never
> referenced at all" case, gated on `used == false`

So a heap binding was released only if it was never used. Every value a
program actually *used* leaked. Measured, in ordinary code, per 1M
iterations of a loop:

| shape | before | after |
| --- | --- | --- |
| `let p = Point { .. }; match p { .. }` | 47.9 MB | **5.2 MB** |
| `let a = [i, i, i, i]; a.len()` | 63.3 MB | **5.2 MB** |
| `let s = Circle(i); match s { .. }` | 32.6 MB | **5.2 MB** |
| `let s = a.concat(b); s.len()` | 139.5 MB | **5.2 MB** |

Linear before (13.7 / 47.9 / 185.1 MB at 250k / 1M / 4M), flat after
(5.2 MB at 4M). This contradicted DESIGN.md's own memory model, which has
specified "at a variable's final use, insert a `drop`" from the start.
What had been carrying the project was reuse-in-place — a different
mechanism entirely.

### The cause is borrow vs. consume, again

Perceus assumes **the last use CONSUMES**, so it emits no trailing
decrement: ownership moves into whatever the last use was. True for a
`Ctor` field, a call argument, a return. False for a `match` scrutinee,
`.len()`, or `.concat()`'s operands, which only READ. The reference is
read and dropped on the floor.

That is the same insight as `Ref`'s (`plum_ir::refdrop`, the day before),
which is what made this one findable.

`all_uses_are_borrows` identifies bindings whose every use is a read, and
`drop_at_scope_end` releases them. The whitelist of borrow positions is
narrow and the DEFAULT IS OWNED, so any position not listed leaves the
binding on the pre-existing path untouched. Widening it can turn a leak
into a release but never turns a working program into a double free —
the risk is one-directional, which is what makes it safe to grow one
position at a time with a measurement behind each.

`allocates_fresh_heap` covers the string/array-producing operations,
consulted at that ONE site rather than added to `is_syntactically_heap`
— which feeds `mark_last_uses` and `mark_reuse` too, so widening it would
have been a broad behavioural change of exactly the kind whose earlier
attempt is recorded above as found unsafe and reverted.

### Four things this got wrong first, each caught by a measurement

**1. `Index` is not a borrow.** `codegen_index` hands back the element
word with no increment, so `a[0]` returns a pointer the array still owns.
Releasing the array dangles it. Found as a segfault in the string and
JSON tests. Whether it is safe depends on the ELEMENT type, which this IR
does not carry, so it stays owned.

**2. Match arms only incremented `CgType::Heap` fields.** `Str`, `Array`,
`Closure` and `Ref` fields were silently omitted — harmless for as long
as nothing ever released a scrutinee, a dangling pointer the moment
something did. Now gated on `dec_fn_for(..).is_some()`, i.e. "this shape
has a refcount word". Not a new leak: it TRANSFERS one reference from
the scrutinee to the binding, and the scrutinee's release decrements it
right back.

**3. Removing the increments broke reuse-in-place.** This was the sharp
one. Those `Inc`s are not merely release bookkeeping — **reuse has no
static check at all, only a runtime `rc == 1` test**, so a non-last use's
increment is the only thing stopping `StrConcatReuse` from destructively
overwriting a string still needed afterwards. Without it,
`let s = "ab"; let t = s.concat("cd"); s.len() + t.len()` returned 8
instead of 6. The release is therefore restricted to bindings with
exactly ONE use, where there is no increment to remove. Multi-use
bindings keep today's behaviour byte for byte, and every measured case is
single-use anyway.

**4. Reuse and release both claim the same reference.** Doing both is a
double free. Reuse WINS wherever it fires, since it avoids the allocation
as well as releasing the cell. Resolved by inserting the release
optimistically in `insert_refcount_ops` and RETRACTING it in
`mark_reuse`, by the code that just decided to reuse — rather than
duplicating reuse's conditions into the earlier pass, where a predicate
that drifted apart would produce a double free, the worst available
failure mode.

There was also a plain bug in my own helper: `for_each_child` had a `_`
catch-all and skipped `StrConcat`, so `incs_name` missed an increment
nested inside one and misread a two-use binding as single-use. That is
what produced (3). It is exhaustive with no `_` arm now, so a missing arm
fails to compile.

### Verified

520 plumc tests + 301 in plum-ir, all 15 suites, fixed point
byte-identical, corpus 99/99, exec_corpus 18/18, and **zero ASan errors**
(no use-after-free, no double-free) across the whole corpus plus the
concurrency and shared-mutability examples. The self-hosted compiler is
unchanged in both memory and speed (254 MB / 0.191 s to check itself,
identical before and after) — its hot paths are dominated by multi-use
bindings and reuse, which this deliberately does not touch.

### What is still open

**Unbound temporaries.** `let s = "a".concat("b")` leaks the two literals:
they are never bound to anything, so there is no binding to hang a
release on. **Fixed the same day by `plum_ir::anf` — see below.**

**Multi-use bindings**, deliberately, until reuse-in-place stops relying
on a bare runtime refcount check for its safety.

**Owned parameters and match-extracted bindings**, which need heap-ness
information the IR does not carry — the same blocker recorded in the
"gap 1" entry above. **Both were addressed on 2026-08-17** — see
"Releasing match-extracted bindings" and "Owned-returning calls" below.
Owned parameters as a CONVENTION remain deliberately untouched; what
landed instead identifies the functions that do not need one.

## A-normalisation: naming the intermediates (2026-08-17)

The scope-end release only reaches a value with a NAME, since the release
attaches to a `Let`'s scope. An unnamed intermediate leaked regardless,
and those are at least as common as the bound case:

| 1M-iteration loop | before | after |
| --- | --- | --- |
| `"abcdefgh".concat("ijklmnop").len()` | 139.2 MB | **5.1 MB** |
| `Point { x: i, y: i }.x` | 47.5 MB | **5.2 MB** |
| `i.to_string().len()` | 34.0 MB | **5.2 MB** |
| `match mk(i) { .. }` (a call result) | 48.1 MB | 48.0 MB |

`i.to_string()` is as ordinary as code gets. `plum_ir::anf` binds
qualifying intermediates to `anf$N` temporaries, which routes them all
through machinery that already works instead of adding a second release
mechanism.

### What qualifies, and why the rule is narrow

Only an expression that is BOTH a syntactically fresh allocation AND has
nothing but atoms for children.

The second half is a soundness requirement. Hoisting moves evaluation
EARLIER — before any sibling to its left — so it is only safe for an
expression whose sole effect is allocating. `f() + Point { x: g() }.x`
would run `g()` before `f()`. Children are processed first, so a nested
fresh allocation becomes a `Var` before its parent is considered, which
is what lets `"a".concat("b")` hoist all three of its allocations rather
than none.

**A `Call` is never hoisted**, which is why the fourth row above is
unchanged. (Lifted later the same day for the subset that provably hands
back a new reference — see "Owned-returning calls" below.) This backend's callees do not release their parameters, so a
function may return one of them and the caller holds no extra reference —
treating a call result as owned would be a use-after-free for
`let pass (p) = p`. That shape is pinned by a test precisely because it
would fail loudly if the rule were ever loosened without an
owned-parameter convention first.

Every deferred or conditional slot — an `If` branch, a `Match` arm, a
loop body, a closure body, a `Let`'s own body — is flattened as its own
region rather than hoisted out of. Hoisting from a match arm would
evaluate it whether or not the arm was taken; hoisting from a loop body
would evaluate it once instead of per iteration, defeating the entire
point.

### A latent codegen bug this exposed

`i.to_string()` in a million-iteration loop **overflowed the stack** once
the release landed. The cause was pre-existing and unrelated:
`ToString`'s `snprintf` scratch buffer was an `alloca` emitted INSIDE the
loop body. LLVM only guarantees a static alloca in the ENTRY block is
allocated once per call; anywhere else it is a fresh stack allocation
every time control reaches it. LLVM had been hoisting it opportunistically
and stopped once the loop body gained one more call — the baseline
survived 20M iterations, which is why this had never been noticed.

`Emitter::fresh_entry_alloca` now collects allocas separately and
`body_lines` splices them in after the `entry:` label. Flat at 5.2MB
through 20M iterations, and a test asserts the placement directly rather
than only its symptom.

### The cost, stated plainly

The self-hosted compiler gets **~8% slower** to type-check itself
(0.2106s → 0.2279s, best of 15) and its peak grows slightly
(254.5 → 271.7 MB); `emit-llvm` moves the other way
(4717 → 4539 MB, 1.73 → 1.85s). The extra `Let` bindings and release
calls are not free, and the compiler's own hot paths are dominated by
multi-use bindings and call results that none of this releases. Turning
unbounded growth into flat memory for ordinary loops is worth 8% on one
workload, but it is a real trade rather than a free win.

### Verified

527 plumc tests + 310 in plum-ir, all 15 suites, fixed point
byte-identical, corpus 99/99, exec_corpus 18/18, zero ASan errors across
the corpus and the concurrency/shared-mutability examples.

## Releasing match-extracted bindings (2026-08-17)

The third and last place a heap value could be owned and never released.
A match arm's binding is ALREADY an owned reference — codegen increments
every refcounted field as it extracts it, transferring one reference from
the scrutinee to the binding — and nothing ever gave it back:

| 1M-iteration loop, arm binds a... | before | after |
| --- | --- | --- |
| `String` field | 32.5 MB | **5.2 MB** |
| `Array[Int]` field | 63.1 MB | **5.2 MB** |
| nested struct field | 32.6 MB | **5.2 MB** |

### How the heap-ness blocker got around

This is the gap that had been recorded three times as blocked on "the IR
carries no types". It still doesn't — but it turns out it doesn't need to.

`plumc` already knows every tag's field types (`tag_fields`, complete once
monomorphization's entries are merged in), so the judgement is made THERE
and handed to the pass as a `tag -> Vec<bool>` table. Crucially the bools
come from `plum_codegen::is_refcounted`, which is the very function
codegen's own extraction increment is gated on — added as a public
one-liner over `dec_fn_for` specifically so the two cannot be derived
separately. Deriving that judgement twice is exactly how an increment and
a release come to disagree, and here disagreeing means a leak or a
dangling pointer.

So no IR change, no threading through `transform`'s seventy recursion
sites: a separate pass (`fbip::release_match_bindings`) run after
`optimize`, at the point in `plumc` where the table is complete.

### The rule, and the four things it declines to do

Same as the `Let` case: exactly ONE use, in a borrow position. Beyond
that, four explicit refusals, each with a test:

- **An escaping binding** (the arm's own result, or stored into a `Ctor`)
  keeps its reference. Supporting that escape is the entire reason the
  extraction increment exists, so breaking it would be worse than the leak.
- **A multi-use binding** is left alone, because the increments it would
  need are what keep reuse-in-place honest.
- **A scalar field** is skipped — `RcAnnotated` on an `Int` is a hard
  codegen error, not a no-op.
- **A catch-all arm's binding** is never released. It binds the WHOLE
  scrutinee rather than a field and codegen does not increment it, so it is
  a pure borrow; releasing it would free the scrutinee from under its
  owner. Such an arm has no `tag_heap` entry at all, which is what makes
  this safe by construction rather than by remembering.

### Verified

532 plumc tests + 318 in plum-ir (8 new unit + 5 new end-to-end), all 15
suites green on the first run, fixed point byte-identical, corpus 99/99,
exec_corpus 18/18, zero ASan errors across the corpus and the
concurrency/shared-mutability examples.

The self-hosted compiler's own numbers are unchanged by this increment
(271.7 MB, 0.192s to check itself) — its matches mostly bind values that
escape or are used more than once, which this deliberately leaves alone.

## Owned-returning calls (2026-08-17)

The last shape still leaking, and the one that looked like it needed a
calling-convention change:

| 1M-iteration loop | before | after |
| --- | --- | --- |
| `match mk(i) { .. }`, `mk` returns a `Point` | 48.1 MB | **5.2 MB** |

A call result could not be released because this backend's callees do not
release their parameters, so a function may RETURN one and the caller then
holds no extra reference. Releasing the result of `let pass (p) = p` frees
the caller's own value.

### What was NOT done, and why

The textbook answer is to make parameters owned — callees release their
own parameters, callers transfer ownership at the call — after which every
return is uniformly owned. That is a genuine convention change touching
every function, its failure mode is a use-after-free rather than a leak,
and DESIGN.md already records one attempt in this territory ("gap 1") as
found unsafe and reverted.

So the convention stayed put and the question was inverted: instead of
making every function's return owned, identify the functions whose return
already is. `anf::owned_returning` is a least fixpoint over
"my tail is a fresh allocation, or a call to something already in the
set". **It adds no increment anywhere and moves no convention** — a
function either provably hands back a new reference or it does not, and
"does not" is the default.

Least fixpoint from the empty set, deliberately: mutual recursion simply
fails to qualify rather than being assumed safe. An optimistic
greatest-fixpoint is the usual formulation and would qualify more
functions, but the constructor-style shape this targets converges on the
first pass anyway, so the extra reach buys little against a
use-after-free.

Two refusals are worth naming because both look allowable:

- **A bare `Var` in return position**, even a match-extracted binding that
  codegen genuinely did increment. Telling one apart from a parameter needs
  types the IR does not carry.
- **A scalar return.** Not an oversight — a scalar has nothing to release,
  and marking such a function owned would invite a decrement on an `Int`.

`ANF` hoists a qualifying call (with atom arguments, same ordering
requirement as everything else it hoists) and emits the release itself,
using `fbip`'s OWN `all_uses_are_borrows` rather than a copy of it. `fbip`
will not add a second release, because `allocates_fresh_heap` does not
recognize a `Call` and cannot — that judgement needs the whole-program
analysis.

The `pass(mk(i))` shape is pinned by a test precisely because it would
fail as a DOUBLE FREE, not a leak, if the analysis ever qualified it.

### Where the memory work as a whole landed

Every ordinary loop-shaped allocation pattern measured over these chunks
is now flat at ~5.2 MB, from 32–139 MB and growing linearly:

| shape | before | after |
| --- | --- | --- |
| `let p = Ctor; match p` | 47.9 MB | 5.2 MB |
| `let a = [..]; a.len()` | 63.3 MB | 5.2 MB |
| `let s = a.concat(b); s.len()` | 139.5 MB | 5.2 MB |
| `"a".concat("b").len()` (unnamed) | 139.2 MB | 5.1 MB |
| `Ctor { .. }.field` (unnamed) | 47.5 MB | 5.2 MB |
| `i.to_string().len()` | 34.0 MB | 5.2 MB |
| arm binds a `String`/`Array`/struct field | 32.5/63.1/32.6 MB | 5.2 MB |
| `match mk(i) { .. }` | 48.1 MB | 5.2 MB |
| `ref(v)` per iteration | 63.0 MB | 5.0 MB |

**The self-hosted compiler is essentially unmoved**: 254.6 → 271.6 MB and
0.1753 → 0.1870 s to check itself, and 4717 → 4532 MB to emit its own IR.
That is worth stating plainly rather than burying. The compiler's own
footprint is not made of the shapes this work fixes — it is dominated by
multi-use bindings (deliberately untouched, since their increments are
what keep reuse-in-place honest), by functions that return parameters or
extracted fields (which do not qualify as owned-returning), and very
likely by genuinely live data. **Finding out which** is the obvious next
question, and `PLUM_ALLOC_STATS` already exists to answer it. It was asked
and answered immediately — see "Where the compiler's memory actually
goes" below. The guess in this paragraph was wrong.

### Verified

537 plumc tests + 329 in plum-ir (12 new unit + 5 new end-to-end), all 15
suites green on the first run, fixed point byte-identical, corpus 99/99,
exec_corpus 18/18, zero ASan errors across the corpus, the
concurrency/shared-mutability examples, and a dedicated ownership fixture
exercising parameter-returning, branch-returning, and chained constructor
functions against the interpreter.

## Where the compiler's memory actually goes (2026-08-17)

Several chunks of release work left the self-hosted compiler unmoved, so
the next step was to stop guessing and read `PLUM_ALLOC_STATS`.

`emit-llvm` over the whole compiler, bytes allocated:

| bucket | allocations | bytes |
| --- | --- | --- |
| str | 21,955,285 | **2286 MB** |
| array | 1,945,667 | **2170 MB** |
| ctor | 2,222,808 | 59 MB |
| closure | 1,495,855 | 37 MB |

Total ≈ 4552 MB against a 4532 MB peak RSS — so essentially NOTHING is
freed, and the footprint is copy garbage rather than live data. (`check`
is the same shape, smaller: 184 MB allocated, 271 MB peak.)

### The cause, confirmed by direct measurement

Two ways of accumulating the same 20,000-character string:

| accumulator held as... | allocations | bytes |
| --- | --- | --- |
| a PARAMETER (tail-recursive `go(acc.concat("x"), n - 1)`) | 40,005 | **200.7 MB** |
| a LOCAL rebinding (`let mut acc` in a `for` loop) | 20,005 | **0.36 MB** |

**557x more bytes**, and 200.7 MB is exactly the sum of 1..20000 — textbook
O(n^2) copying. The local version gets reuse-in-place; the parameter
version never does.

`mark_reuse` only ever targets a name present in `known_heap`, and a
PARAMETER is never in it. The self-hosted compiler is written in
tail-recursive accumulator style throughout, so this is its 4.5 GB.

This also settles a question DESIGN.md had already recorded and got wrong.
The "Where the self-hosted backend actually lands" section concluded that
a general last-use analysis "would catch the accumulators that are not
self-rebinding (`f(acc.push(x))` in a tail-recursive loop, which is how
the parser is written)" but that "the measured upside is now small". The
shape it named was right; the size was not, by two orders of magnitude.
The difference is that the earlier measurement was of peak RSS on a
workload where reuse already handled the dominant path, not of bytes
allocated.

### A sound rule for parameter reuse

Parameter reuse was attempted once and reverted (see "Gap 1" above): two
simultaneous uses of the same unprotected parameter could each observe
refcount 1 and both destructively reuse the same cell — `s.concat(rep(s,
n - 1))`, a real segfault. The fix at the time was to gate reuse on
`known_heap`, which excluded parameters wholesale.

What makes reuse safe is the CALLER's increment: `mark_last_uses` already
increments a tracked argument the caller still needs, so the callee
observes refcount > 1 and the runtime check declines to reuse. That
protection is real but incomplete — it does not cover a callee that uses
its own parameter again AFTER the reuse site, which is exactly the
reverted crash.

A local, checkable condition closes that hole: allow reuse on a parameter
only when the parameter is used **at most once on any path** through the
function body, counting `If`/`Match` branches as alternatives rather than
additively. Then:

- `go (acc) (n) = if n == 0 { acc } else { go(acc.concat("x"), n - 1) }` —
  one use per branch, so max 1. ELIGIBLE, and it is the shape that matters.
- `rep (s) (n) = s.concat(rep(s, n - 1))` — two uses on the same path.
  REJECTED, which is precisely the shape that crashed.

Implemented immediately after — see below.

## Reuse-in-place on parameters (2026-08-17)

The first change in this whole sequence that actually moved the compiler.

| the compiler emitting its own IR | before | after |
| --- | --- | --- |
| peak RSS | 4717 MB | **2389 MB** |
| wall time | 1.4457 s | **0.8934 s** |
| array bytes allocated | 2170 MB | **51 MB** |

And on the isolated shape — a 20,000-character tail-recursive
accumulation: **200.7 MB -> 0.36 MB**, byte-for-byte identical to the same
thing written as a local rebinding (20,005 allocations, 360,102 bytes).

### The rule

Two conditions, and each closes one of the two ways reuse can be unsafe.
Reuse's only guard is a runtime `rc == 1` check, so it is safe exactly when
nothing else needs the cell at that moment.

**1. The parameter is used at most once on any PATH** — `If`/`Match`
branches counted as alternatives rather than additively. This rules out
the callee corrupting a cell it still needs. The branch/alternative
distinction is the entire point: it admits `go (acc) (n) = if n == 0
{ acc } else { go(acc.concat("x"), n - 1) }` (one use per branch) while
rejecting `rep (s) (n) = s.concat(rep(s, n - 1))` (two uses on one path) —
and that second shape is the exact segfault DESIGN.md's "Gap 1" records as
the reason the previous attempt was reverted.

A loop body or a closure body counts as two uses regardless of what is
inside it. One syntactic use is not one dynamic use: either can run the
reuse site again, and after the first time the cell is gone.

**2. Every call site passes a provably uniquely-owned value** — a
syntactically fresh allocation, or a `*Reuse` node. This rules out the
CALLER being corrupted. For a tracked argument the existing machinery
already handles it (`mark_last_uses` increments an argument the caller
still needs, so the callee observes `rc > 1`), but for an UNTRACKED one —
itself a parameter, a call result, a match binding — no increment exists
and the runtime check offers nothing. `{ let r = f(q); q.len() }` with `q`
a parameter is that hole, and requiring a non-`Var` argument closes it.

A function whose name is ever mentioned other than as a direct callee has
call sites this cannot enumerate, so all of its parameters are ineligible.

### What it does NOT do

No increment is added anywhere and no calling convention changes. The
eligible parameters seed `mark_reuse`'s `known_heap` and only
`mark_reuse`'s; `insert_refcount_ops` is untouched. This does deliberately
relax the invariant `mark_reuse_scoped`'s doc comment describes — that
reuse fires only for names `insert_refcount_ops` protected — and the two
conditions above are what stand in for that protection.

Computed BEFORE `anf`, which hoists a fresh-allocation argument into a
temporary and would leave a bare `Var` where condition 2 needs to see the
allocation. Not a soundness hole either way (an ANF temporary is a
`Let`-bound fresh allocation, hence tracked and single-use, so it is safe
by the tracked-argument case), but computing it first is what keeps the
accumulator shape recognizable.

### What is left: struct-field accumulators

Strings barely moved: 2286 MB -> 2154 MB allocated, still 21.8M
allocations. Measured rather than guessed at, and the answer is sharp — the
same 20,000-item accumulation, two ways:

| accumulator held as... | allocations | bytes |
| --- | --- | --- |
| a bare PARAMETER (`acc.concat(x)`) | 20,005 | 0.36 MB |
| a STRUCT FIELD (`Emit { code: r.code.concat(x), .. }`) | 40,005 | 200.7 MB |

The second is `codegen.plum`'s `Emit` exactly, and it is still fully
O(n^2). Two independent things block it:

1. `r.code` is a MATCH-EXTRACTED binding, and codegen increments a
   refcounted field as it extracts it — so `rc == 2` at the concat and
   reuse correctly declines. `r` is still alive and genuinely holds that
   string.
2. Extracted bindings are not in `known_heap`, so `mark_reuse` would not
   target one anyway.

The `Emit` CELL is not reused either: `r` is read six times in that one
expression (`r.code`, `r.allocas`, ... each lowering to its own `Match`),
so `max_uses_on_path` saturates at 2 and the parameter is ineligible. The
20,001 ctor allocations in the measurement above confirm it.

### The mechanism this needs: consuming pattern match

When the scrutinee is provably dead after the match, extraction should
MOVE its fields — no increment — rather than borrow them. Then `code` has
`rc == 1` and `StrConcatReuse` grows it in place: O(n) instead of O(n^2).
Worth roughly 2154 MB -> ~100 MB of the compiler's string traffic.

Not attempted here, and the reason is specific rather than general
caution: moving fields out means the scrutinee's cell must be freed
WITHOUT releasing its fields, which collides directly with `CtorReuse`'s
existing "release the old fields, then overwrite in place" path. That
interaction needs a design pass, not an increment.

A cheaper, strictly smaller variant is also available and worth noting:
relaxing condition 1 from "at most once on any path" to a true last-use
test would let `CtorReuse` fire for the `Emit` cell itself, since the
construction happens after every field read. That is sound but only worth
the ~59 MB of ctor traffic, not the 2154 MB, so it is not the interesting
half.

`check` is unchanged in memory (254.7 -> 251.5 MB) and still carries the
release work's ~6% time cost (0.1774 -> 0.1886 s).

### Verified

548 plumc tests + 339 in plum-ir (10 new unit + 6 new end-to-end), all 15
suites green on the first run, fixed point byte-identical, corpus 99/99,
exec_corpus 18/18, and zero ASan errors across the corpus, the
concurrency/shared-mutability examples, and dedicated fixtures for the
`rep`, `hold`, and tracked-caller shapes — each of which is a
use-after-free rather than a leak if the rule is wrong. All of those also
agree with the interpreter output for output.

## Toward consuming pattern match: three fixes, and why the compiler did not move (2026-08-17)

Going after the struct-field accumulator turned up two regressions I had
introduced earlier the same day, one hole in my own analysis, and a
correction to the plan itself. The compiler's numbers are unchanged, and
the reason is now exact rather than suspected.

### The plan was wrong, and one experiment said so

The write-up above proposed coalescing the six field reads as the enabling
step. Hand-writing the single-match form first took two minutes and
settled it:

| shape | bytes |
| --- | --- |
| six field reads (`Emit { code: r.code.concat(x), .. }`) | 200.7 MB |
| ONE match (`match r { Emit(code, n) => Emit { .. } }`) | 200.7 MB |

Identical. Coalescing alone changes nothing, because `code` is still
incremented on extraction (`rc == 2`) and `r` is still never released.
Building the pass first would have been a day's work for no effect.

### Fix 1: an unused extracted field was still incremented

`let second (p: Pair): Int = p.n` incremented the `String` field it never
touches. That increment had nothing to balance it —
`release_match_bindings` deliberately requires a USE before it will
release — so it leaked the field and, through it, the scrutinee. Introduced
by widening the extraction increment from `CgType::Heap` to every
refcounted shape; visible directly in the emitted IR.

Now gated on the arm actually mentioning the binding.
`expr_mentions_var` is coarse and shadowing-unaware, which is the safe
direction: over-reporting costs an increment, never a dangling pointer.

### Fix 2: `anf` had silently disabled `CtorReuse`

`mark_reuse` rewrites a `Ctor` into a `CtorReuse` only when the `Ctor` is
the match arm's body. `anf` hoists a `Ctor`'s fields into `Let`
temporaries, so the arm body became `Let { anf$1, .., Ctor { .. } }` and
the pattern stopped matching — **reuse-in-place quietly stopped firing for
every such arm**, with no failing test, because the interpreter's path has
no `anf` stage and the unit tests build IR directly.

`rewrite_tail_ctor` now looks through leading `Let`/`RcAnnotated` chains.
Measured on the accumulator: **20,001 ctor allocations -> 1.**

### Fix 3: uniqueness is transitive, and escapes could launder it

`reusable_params`' condition 2 required a syntactically fresh argument at
every call site, which rejected `push(r, "x")` — where `r` is the caller's
own parameter at its last use, and therefore safe. It is now a least
fixpoint over "unique on entry": an argument may be a bare `Var` naming
the ENCLOSING function's parameter, provided that parameter is itself
unique on entry AND this is its last use.

The transitivity is load-bearing. Without it `g (p) = f(p)` would
establish `f`'s parameter as unique on the strength of `g`'s, while
`h (q) = { let r = g(q); q.len() }` still needed the value.

Writing that fixpoint exposed a hole of its own: an ESCAPED function's
parameters were being treated as unique (no enumerable call sites meant
"no call site violates it"), and that propagated outward to establish
uniqueness for other functions that nothing guaranteed. Escaped functions
are now excluded from the fixpoint entirely. Two of my own safety tests
also had to be rewritten — they asserted that any bare `Var` argument
disqualifies, which was the old rule, not the actual hazard.

### Why the compiler still did not move

Byte-identical allocation stats. The reason is one line of `codegen.plum`:

```
Emit { code: r.code.concat(line), allocas: r.allocas, releases: r.releases, ... }
```

The `Emit` construction is OUTSIDE all six field-read matches, so
`rewrite_tail_ctor` never sees it and `CtorReuse` cannot attach to
anything. Coalescing IS needed after all — not to fix the string, but to
put the `Ctor` inside a match arm so cell reuse can fire at all.

So the remaining chain is two steps, in order:

1. **Coalesce sibling field-read matches** on the same scrutinee into one.
   Worth the ~59 MB of ctor traffic by itself, and a prerequisite for (2).
2. **Move fields instead of incrementing them** when the arm's body became
   a `CtorReuse` of the scrutinee — the cell is being consumed anyway, so
   the fields are morally ours. That is what gets `rc == 1` at the concat
   and turns the 2154 MB into roughly 100 MB.

Step 2 has a genuine subtlety worth recording before anyone starts it:
`CtorReuse` decides between reuse and fresh allocation with a RUNTIME
`rc == 1` check, but the extraction increment happens earlier. Moving
unconditionally would be wrong on the fresh-allocation branch, so either
the increment becomes conditional on the same test, or the test moves
earlier. Both are real changes to that node's shape.

### Verified

548 plumc tests + 344 in plum-ir (4 new), all 15 suites, fixed point
byte-identical, corpus 99/99, exec_corpus 18/18, zero ASan errors across
the corpus, the examples, and the `rep`/`hold`/ownership fixtures.

## Consuming pattern match: what landed, and one pass built and removed (2026-08-17)

Releasing a matched scrutinee works, and is worth 200x on the shape it
targets. Merging field reads to reach the compiler's own version of that
shape was built, measured, found to be a net negative, and removed.

### What landed: `consume_matched_scrutinees`

Codegen increments a refcounted field as it extracts it, transferring one
reference from the scrutinee to the binding — so while the scrutinee
lives, that field's count is at least 2 and nothing can reuse it. Releasing
the container right after extraction drops it to 1, and the field becomes
genuinely ours.

| 20,000-item struct-field accumulation | before | after |
| --- | --- | --- |
| `Emit { code: r.code.concat(x), .. }` | 200.7 MB | **0.36 MB** |

No new IR node and no codegen change. `RcAnnotated { Dec, .. }` runs its
decrement BEFORE `rest`, and extraction happens before the arm body, so
wrapping the body is exactly "after extraction, before use". There is no
conditional logic either: if the count is greater than 1 the decrement
leaves it alive, the fields stay at 2, and everything falls back to
copying — correct, just not fast.

Eligibility covers PARAMETERS (via `reusable_params` — releasing one and
reusing one carry the identical hazard) and LOCALS bound to a uniquely
owned value and matched at most once. Locals matter more in practice: the
compiler's emitters are written `let r = cg_expr(..); .. r.code ..`.

Reuse-in-place stands down for a scrutinee an arm releases: both consume
the same reference, and releasing wins because it unlocks in-place growth
of the FIELD rather than saving only the container's own cell.

### Three bugs found on the way, two of them mine

**An unused extracted field was still incremented.** `let second (p: Pair):
Int = p.n` incremented the `String` field it never touches, with nothing to
balance it. Mine, from widening the extraction increment to every
refcounted shape.

**`anf` had silently disabled `CtorReuse`.** It hoists a `Ctor`'s fields
into `Let` temporaries, so a match arm body stopped being a bare `Ctor` and
`mark_reuse`'s pattern stopped matching. Reuse quietly stopped firing for
every such arm, with no failing test — the interpreter's path has no `anf`
stage and the unit tests build IR directly. Also mine. Fixed by looking
through `Let`/`RcAnnotated` chains: 20,001 ctor allocations -> 1.

**`expr_mentions_var` ignored `reuse_of`.** A `*Reuse` node consumes the
cell it names, which is as real a reference as reading it. Once codegen
began skipping the extraction increment for an unmentioned binding, a
`StrConcat` that `mark_reuse` had rewritten into a `StrConcatReuse` made
its own operand look unused — the increment vanished, the scrutinee's
release freed the cell, and the reuse read freed memory. A segfault.

### A release must never cost a tail call

`drop_at_scope_end` binds the body's result to a temporary, which puts the
body in a `Let`'s VALUE position and takes `musttail` away from whatever
ended it. Guaranteed tail calls are a language promise, not an
optimization.

The self-hosted lexer is one tail-recursive call per token; once enough of
its bindings became releasable it overflowed the stack on its own source.
Every release site now declines when the scope ends in a call. The cost is
a leak, which is what the code did before any of this existed. The corpus
never caught it — those programs do not recurse deeply enough — so there is
a dedicated 300,000-deep test now.

### The pass that was removed

Merging repeated field reads was the named prerequisite: `p.x` lowers to a
whole `Match`, so `Emit { code: r.code.concat(line), allocas: r.allocas, .. }`
reads `r` seven times, which makes it look multi-use to every analysis and
leaves the construction outside every match.

It was built, and it worked — 1352 merges across the compiler, and the
compiler's exact accumulator shape went 200.7 MB -> 0.36 MB in isolation.
It was removed anyway, for two measured reasons:

1. **No effect on the compiler.** Allocation stats byte-identical, in every
   bucket.
2. **It broke the compiler.** Hoisting one destructuring to dominate a whole
   scope extends every field's live range across the function, and the
   frames grew enough to overflow the stack — `emit-llvm` segfaulted even
   with 512 MB of stack, so not merely depth.

Shipping it would have been shipping a regression for no measured gain.
Recorded here so the idea is not rebuilt from the same reasoning: the
obstacle is not that the merge is hard, it is that whole-scope
destructuring trades allocation for stack, and on this workload that trade
is bad. A version restricted to unconditionally-evaluated positions avoids
the live-range blowup but cannot reach the compiler's shape, where the
reads sit in separate statements and inside `match` arms.

### Verified

550 plumc tests (3 new) + 353 in plum-ir (10 new), all 15 suites, fixed
point byte-identical, corpus 99/99, exec_corpus 18/18, zero ASan errors,
and the compiler builds and checks itself. Its allocation numbers are
unchanged — the shape this targets is reachable in ordinary Plum code but
not, without merging, in the compiler's own source.

## Value-position `Assign` (2026-08-17)

The last construct DESIGN.md listed as reaching codegen's
"does not yet support this construct" catch-all:

```
twice({ sum = sum + 1; sum })      // as a call argument
let y = { sum = sum + 1; sum };    // as a Let's value
1 + { sum = sum + 1; sum }         // as an operand
```

Assignment is a STATEMENT in this backend. `codegen_expr`'s `Assign` arm
threads an updated `Env` into whatever follows, because a `let mut`
variable is an SSA register rather than a stack slot — and `codegen_value`
returns a register and a type, with no way to hand an environment back to
its caller.

Rather than teach it to, `plum_ir::liftassign` moves the assignment to
where the existing machinery already handles it:

```text
N(.., Assign { n, v, rest }, ..)  =>  Assign { n, v, N(.., rest, ..) }
```

### Order is the whole problem

The rewrite moves the assignment EARLIER — ahead of everything the node
evaluated before that slot. Where a preceding slot does not commute with
it, that slot is bound to a temporary first, which pins it where it stood:

```text
sum + { sum = sum + 10; sum }   =>   let t = sum; { sum = sum + 10; t + sum }
```

With `sum` at 1 that is 1 + 11 = 12. Lifting directly would have given
22, and a test pins the difference. Same for a preceding CALL, which may
read or write the variable: bound first, so it still runs first. Two
assignments in one expression each lift in turn, and
`{ d = d + 1; d } + { d = d + 1; d }` is 3 — not 4, not 2.

Only slots evaluated UNCONDITIONALLY are eligible: an `If`'s condition and
a loop's BOUNDS qualify; its branches and body do not. Lifting from a
branch would perform an assignment the program does not; lifting from a
loop body would perform it once instead of per iteration. Both have tests.

### The catch-all is now unreachable from Plum source

No writable Plum program is known to reach it. It is kept and still
tested: `plum_codegen`'s own
`unsupported_construct_is_a_clear_error_not_a_panic` builds IR directly, so
it bypasses this pass and exercises the error path.

The `plumc`-level test that used to cover this had been repointed twice as
its subject kept getting implemented — first off a Unicode string op, then
off `ref(1)`, and now off value-position `Assign`. It asserts the construct
WORKS now, and says so.

### Verified

560 plumc tests (7 new) + 363 in plum-ir (10 new), all 15 suites, fixed
point byte-identical, corpus 99/99, exec_corpus 18/18, zero ASan errors,
and every new case checked against the interpreter output for output.

## Move-on-last-read in the self-hosted backend (2026-08-17)

The two backends had diverged badly, and measuring it was the whole reason
this got found. Same program, a 20,000-item string accumulation:

| backend | bytes allocated | peak RSS |
| --- | --- | --- |
| real (`plum build`) | 0.36 MB | 5.2 MB |
| self-hosted (`./sh emit-llvm`) | 200.4 MB | 194.0 MB |

557x on allocation. A week of memory work had gone into the real backend
and none of it existed here.

### The diagnosis was the opposite of what was expected

The self-hosted backend is not leaking. It does PRECISE reference counting
on the typed tree, and its own note explains why it can: every node's type
is known, so `cg_is_heap` is total and exact, and parameters are counted
like anything else. That is a better foundation than `plum-ir/fbip`, which
cannot see types at all.

What it lacked was REUSE. `cg_borrow` on an identifier is already a true
borrow — no increment — so `acc.concat(x)` already saw refcount 1. But
`cg_concat` called the copying `@plum_str_concat`, and the reusing path was
reachable only through `cg_is_self_method`'s literal `x = x.concat(..)`
shape. An accumulator threaded through a call never matched.

### The change

A slot read AT MOST ONCE on any path hands its reference over instead of
lending it: load it, store null back, no increment. `cg_concat` then calls
`@plum_str_concat_reuse`, which CONSUMES its receiver (its copy branch
releases it, its in-place branch returns it) and is runtime-guarded on
`rc == 1` plus `malloc_usable_size` — so using it is never wrong, merely
useless when the count is higher.

Storing null is what removes the need for any path analysis. The slot's
release at function exit still runs, and every `plum_rel_*` is null-safe by
construction (`cg_null_init` already depended on that), so it becomes a
no-op on exactly the paths where the reference left.

**No whole-program reasoning is needed**, unlike the real backend's
equivalent. Rule 4 of this backend's discipline says a call CONSUMES its
arguments, so a parameter slot owns its value outright and no caller is
still holding it. That is the owned-parameter convention `plum-ir`
deliberately does not have — and it is why the same idea took a
least-fixpoint uniqueness analysis there and takes none here.

Reads are counted per PATH, with `If`/`Match` branches as alternatives.
That is what admits the accumulator: `if n == 0 { acc } else {
build(acc.concat("ab"), n - 1) }` reads `acc` once in each branch, and only
one branch runs. A loop body or closure body counts as two reads — one
syntactic read there is not one dynamic read. Assigning through the name,
or a pattern that rebinds it, also disqualifies.

### The same treatment for `.push`

`@plum_array_push_grow` has the identical contract to
`@plum_str_concat_reuse` — rc-guarded, consumes its receiver, and DOUBLES
on the copy branch — and `cg_array_push` had the identical problem: it
allocated a fresh array and copied on every push unless the code matched
`acc = acc.push(x)` literally. The parser accumulates `Array[Token]`
through a tail-recursive call, which never did.

With a moved receiver, a 20,000-element accumulation takes **14 array
allocations** instead of 20,000, and 0.53 MB instead of ~1.6 GB of copies.

### Results

| | before | + concat | + push |
| --- | --- | --- | --- |
| compiler emitting its own IR, peak RSS | 1458.1 MB | 114.0 MB | **118.4 MB** |
| ...wall time | 1.98 s | 1.85 s | **0.94 s** |
| ...bytes allocated | 4564 MB | 3902 MB | **1565 MB** |
| ...array allocations | 2,040,355 | — | **621,176** |
| 20,000-item string accumulation, peak RSS | 194.0 MB | **5.2 MB** | 5.2 MB |
| compiler CHECKING itself, peak RSS | 49.4 MB | 50.5 MB | 52.7 MB |

**12.3x less memory and 2.1x faster** on the compiler's own IR emission.
Worth separating the two: `concat` bought the memory, `push` bought the
speed, and neither alone gets both.

5.2 MB on the microbenchmark matches the real backend exactly.

The `check` path is essentially unmoved (49.4 -> 52.7 MB), and that is
worth stating rather than hiding in an average: it is not
accumulator-dominated, so this does nothing for it.

### Verified

Fixed point byte-identical (119,107 lines), corpus 99/99, exec_corpus 17/18
under BOTH the self-hosted interpreter and the self-hosted backend (the
18th is `refs`, the documented `Ref[T]` scope gap), 560 plumc tests, all 15
suites, and zero ASan errors across the self-hosted backend's own output
for every corpus fixture.

A new `accumulator/` fixture pins every half of the rule for both strings
and arrays: the moved case; `twice_str`/`twice_arr`, whose parameter is
read twice on one path and therefore must NOT be moved (the second read
would find a nulled slot); and `branchy`, read once per branch and
therefore movable despite two syntactic reads. All three implementations —
the real backend, the self-hosted interpreter, and the self-hosted backend
— agree on it, and it is ASan-clean under the self-hosted backend.

## The self-hosted backend overtakes the real one (2026-08-17)

Compiling the same source, the two backends now produce this:

| workload | rust-built | self-hosted-built |
| --- | --- | --- |
| `check` the whole compiler | 0.184 s / 256.7 MB | 0.268 s / **52.6 MB** |
| `emit-llvm` the whole compiler | 0.805 s / 2445.4 MB | 0.946 s / **118.2 MB** |

**4.9x less memory on `check` and 20.7x less on `emit-llvm`**, at 1.2-1.5x
the wall time. A day earlier the self-hosted backend was 37x WORSE on the
accumulator microbenchmark; it is now decisively ahead on the compiler's
own workloads.

This is architectural rather than incidental, and the reason is the one
both backends' comments have been pointing at from opposite directions.
`plum-ir/fbip` cannot see types — its own comment says "no type checker in
this IR to prove one is heap-shaped" — so it cannot track parameters, which
is what forced every workaround in the real backend: the reverted "gap 1"
attempt, the least-fixpoint uniqueness analysis, the owned-returning
analysis, and finally the merging pass that had to be removed. The
self-hosted backend has the typed tree, so `cg_is_heap` is total, parameters
are counted like anything else, and move-on-last-read needed no
whole-program reasoning at all.

The real backend's remaining 2445 MB is precisely the struct-field
accumulator it cannot reach: `Emit { code: r.code.concat(x), .. }`, where
the fix needs consuming destructuring, and the only route to that measured
as a net negative (see "Consuming pattern match" above).

The remaining gap is TIME, not memory, and it is the smaller one. Worth
saying plainly: nothing here shows the self-hosted backend generates faster
code — it does not, yet.

## Where the compile time actually went (2026-08-17)

`plum emit-llvm` on this compiler's own source took **68.6s**. It now
takes **9.7s**, and every step of getting there was a measurement
contradicting a guess.

Nothing could say which pass was responsible, so the first change was
`PLUM_PASS_TIMES=1` — a per-phase stopwatch that works from any entry
point (`build`, `emit-llvm`, a test). The answer was not close:

```
  0.007s  front-end rewrites
 68.123s  type inference          <- 99.3%
  0.176s  monomorphize::plan
  0.151s  fbip::optimize_program
  0.075s  emit_program
   ...    everything else < 0.005s
```

The guess had been the whole-program fixpoint passes — `reusable_params`,
ANF, `prune`, `refdrop`. Together they are **0.19s**. Two orders of
magnitude off. The backend, the part with all the interesting analysis
in it, was never the problem.

**`TypeEnv::extend` was O(environment).** It was a `Vec` used as a
persistent value, so binding one name cloned every `String` and
`Scheme` in scope. Inference calls it once per parameter, `let`, match
binding and lambda argument: 629,231 calls copying **257,126,855**
entries, average environment 409. Making it a shared-tail `Rc` cons
list makes `extend` O(1) — one node, one refcount bump — with
identical value semantics, since nothing was ever mutated. The lookups
already wanted innermost-first order (`.iter().rev().find(..)`), so the
chain reads more naturally than the `Vec` did. **68.6s -> 15.4s.**

**Then `apply_subst`, and a wrong turn worth recording.** It rebuilt
every binding in scope on each refinement: 33,699 calls, 42,754,972
entries. The obvious fix — carry a PENDING substitution on the
environment and apply it lazily at lookup, O(substitution) instead of
O(environment) — was implemented, produced byte-identical IR, and was
**slower**: 15.4s -> 19.5s. The pending substitution accumulates every
binding, so `compose` grew from 18M entries copied to **163M** (average
map 1535). Cheap work done 42M times beat expensive work done 33k
times. It was reverted.

What the counters actually showed: of those 42.7M entries, only
**325,831 — 0.76% — change**. Applying a substitution to a type is
cheap; ALLOCATING a new binding for the 99.24% that come back
identical is not. So the rewrite stays eager but SHARES: rebuild
bottom-up, and when a node's own scheme and its whole tail are both
untouched, hand back the original `Rc`. 92.9% of nodes are now shared
rather than reallocated. **15.4s -> 9.7s.**

A free-var min/max range check was also measured before being built,
and would have skipped only 41.6% — worth knowing, since it is the
first idea that comes to mind and it is the wrong one.

### The compiler was not reproducible

Verifying "this refactor changed nothing" ought to be a matter of
diffing the emitted IR. That was not available: the same binary emitted
**261,046 differing lines** on two consecutive runs. Two independent
causes, both `HashMap` iteration order (randomised per process):

  * `monomorphize::plan` seeded its worklist unordered, so function
    emission order varied — and closures are numbered from one global
    counter in emission order, so they were renamed.
  * `codegen::merge_envs` allocated SSA registers while iterating an
    `Env`, permuting register numbers at branch merges.

The output was always CORRECT — two differently-numbered builds of the
self-hosted compiler emit byte-identical output, the IR handed to
codegen was already deterministic, and `reusable_params` fingerprints
identically across runs, so no analysis was order-dependent. What it
cost was reproducible builds, caching, bisectable codegen regressions,
and the ability to check a refactor by diffing. The last one is not
hypothetical: it is exactly the check the `TypeEnv` work needed and
could not use until this was fixed. Both inference changes above are
now verified byte-identical against a baseline built from the previous
commit.

The regression test must run the compiler in SEPARATE PROCESSES.
Rust seeds each `HashMap` once per process, so two compiles inside one
process iterate identically and pass regardless of how order-dependent
the compiler is — the first version of this test did exactly that and
passed with both fixes reverted. It also needs a fixture with enough
branch-divergent bindings to actually permute; the existing examples
were too small to notice.

## The 20x gap was a missing check, not a faster design (2026-08-18)

After the inference work above, the Rust compiler emitted IR for this
compiler's source in 9.6s. The SELF-HOSTED compiler did the same job in
**0.48s** — 20x faster, same input, same output. Two ways that could
go: the Rust one still had structural waste, or the self-hosted one was
skipping work it should be doing. It was the second.

`bootstrap/self_host/typecheck/infer.plum` has exactly four `tyenv_*`
functions — `empty`, `extend`, `lookup`, `lookup_acc`. There is no
environment refinement at all. The Rust engine's `TypeEnv::apply_subst`
runs 33,699 times over 42,754,972 entries; the self-hosted checker
simply never does it. That absence IS the 20x.

Is that refinement load-bearing? Disabling it and running the suite
answers precisely: **one** test fails, `if_condition_constraint_on_an_
existing_binding_propagates_into_sibling_branches`. One case, already
documented at the point it was fixed.

So the self-hosted checker was tested on that case, and got it wrong:

```plum
let f = |n| if n == 0 { !n } else { true };
```

`n == 0` pins n to Int; `!n` needs Bool. The Rust compiler rejects it
with a span. The self-hosted `check` printed **`ok`**. Codegen then
emitted `xor i1` against an `i64`, and the only thing that ever
objected was `clang` — with no source location. A `check` that answers
"ok" for an ill-typed program is failing at the one thing it exists to
do.

Worth being precise about severity: no wrong BINARY was produced here,
because LLVM's own type checker caught the i1/i64 confusion. Whether a
confusion between two same-width representations could slip through
silently was not established either way — it is plausible and
unproven, so it is not claimed.

The fix follows the self-hosted architecture rather than importing the
Rust one. `infer_ident` (and the `EIdent` case of `infer_call`) now
returns `subst_apply(acc, ty)` instead of the raw stored type — the
substitution is ALREADY threaded through inference, so resolving at
lookup costs O(substitution) at the handful of points that read a
name, instead of O(environment) at every refinement. The environment
walk stays gone.

Verified: both failing programs now rejected, all 7 rejection fixtures
rejected, 18/19 `exec_corpus` still `ok` (the exception is the
pre-existing `Ref[T]` gap, confirmed identical on the old binary), the
compiler still checks its own source, and the BOOTSTRAP FIXED POINT
still holds byte-for-byte. Cost: 0.503s -> 0.596s.

The general lesson is worth keeping: when two implementations of the
same thing differ by 20x, "the faster one is better designed" is a
hypothesis, not a conclusion. Here the faster one was faster because it
was doing less, and some of what it skipped was necessary.

### Skipping the environment suffix (2026-08-18)

Node sharing above stopped `apply_subst` from ALLOCATING for the 93% of
bindings a substitution leaves alone, but it still WALKED all of them:
42.7M node visits. Measuring the ceiling first — make `apply_subst` a
no-op and time it — put type inference at **2.0s** against 9.2s, so
about 7s was on the table and worth going after.

The obvious fix is what the self-hosted checker does: resolve names
against the substitution at lookup and drop environment refinement
entirely. That does not port cheaply here. `infer_expr` RETURNS a
`Subst` rather than taking one; callers propagate knowledge by doing
`env.apply_subst(&s)` before inferring the next subexpression. Refining
the environment IS this engine's threading mechanism, so removing it
means threading an accumulator through the whole recursion — a
signature change across a 8,000-line file, for a win that could be had
another way.

The cheaper route uses a property the cons list already has. Type
variables are handed out in increasing order and bindings push onto the
HEAD, so the chain is ordered newest-first and variable ids decrease
monotonically with depth. The deep bulk of the environment is every
top-level signature in the program — which is why the average
environment is 1269 entries — and all of it predates any substitution
generated while inferring a function body.

So each `EnvNode` caches `tail_max`: the largest variable id in its own
scheme or anywhere in its tail, maintained in O(1) per `extend` as the
max of the new scheme and the tail's cached value. `apply_subst` takes
the substitution's lowest key, and the walk stops dead at the first
node whose whole suffix falls below it. Sound by construction: skipping
requires every variable in the suffix to be below every key in the
substitution, so none can be in its domain. `tail_max` is RECOMPUTED
rather than inherited when a node is rebuilt, since substituting can
replace a variable with a type mentioning higher-numbered ones.

**9.7s -> 4.2s**, IR byte-identical, against a floor of ~2.4s.

The whole arc, on this compiler's own source:

```
  68.6s   where it started
  15.4s   cons-list `extend`      (O(env) -> O(1) per binding)
   9.7s   node sharing            (stop reallocating the 93% that don't change)
   4.2s   suffix pruning          (stop WALKING them either)
  ~2.4s   floor, if the walk vanished entirely
```

A note on process, since it nearly went wrong: the first IR comparison
after this change came out DIFFERENT, and the difference was reported
before it was understood. The baseline was stale — it had been emitted
before `bootstrap/self_host/typecheck/infer.plum` was patched for the
soundness fix above, so the INPUT SOURCE had changed underneath it.
Against a same-source baseline the output is byte-identical. Byte
comparison is only as good as the discipline about what is being
compared to what.

### `Ref[T]` in the self-hosted backend — the last exec_corpus gap (2026-08-18)

`bootstrap/exec_corpus` was 18/19 for a long time: `refs` was the one
fixture the self-hosted compiler could not check, let alone run
("unbound function: ref"). It is now **19/19**.

`ITRef(ITy)` threads through unify/subst/occurs/render; `CgRef(CgTy)`
through the codegen type layer. `ref(v)` gets its own `TRefNew` node —
it is a CONSTRUCTOR, not a receiver-shaped call, and a new `TNode`
variant only costs 7 sites. `.get()`/`.set(v)` reuse `TMethodCall`,
split from `Array.set` purely by ARITY, exactly the way inference
splits them.

Three places where the existing design already had the right answer:

  * **Layout.** The cell is `{ i64 rc, value }` with the value in its
    NATURAL machine type at offset 8 — the same convention array
    elements and struct fields already use. No i64 punning, and no new
    runtime helper. A `@plum_alloc_ref` had already been written
    (mirroring the Rust backend, which does pun through i64) before
    this was noticed; it was deleted rather than kept.
  * **Release.** `cg_emit_struct_rel(t, [inner])`. A `Ref` cell is
    byte-for-byte a ONE-FIELD struct, so the struct path is not merely
    similar to what `Ref` needs, it is exactly it.
  * **`.get()`.** Borrow the cell, load, retain, drop the base — the
    same shape as `cg_field_read`, for the same reason: the caller
    receives an owned value while the cell keeps its own reference.

Equality is IDENTITY, not structural: two cells holding equal values
are still two cells. That is the entire point of an explicit
shared-mutable reference, and it is what the fixture pins down
(`a == b` true, `a == c` false with identical contents).

**Positional struct patterns came along for the ride.** The fixture
also needs `match a.get() { Point(x, y) => .. }` — a struct
destructured positionally — which neither the self-hosted checker nor
its codegen supported, and which has nothing to do with `Ref`. The
parser cannot tell `Point(x, y)` apart from a variant pattern; the NAME
decides. Both layers now fall back to the struct path when the tag
resolves to a struct instead of a variant, pairing positions with
declared field names — the same reuse the tuple case already makes by
pairing with indices.

**Memory, checked under ASan rather than assumed.** No use-after-free,
no double-free, no overflow in any of the 19 fixtures. 16 of the 19 DO
leak at exit — including `recursion_factorial` (32 bytes) and
`arithmetic` (160), which never touch a `Ref` — so exit-time leaking is
a pre-existing, backend-wide property, not something this introduced.
The Rust backend leaks on the same fixtures too, and MORE on this one
(291 bytes in 17 allocations, against the self-hosted 96 in 3). Not a
`Ref` bug — and investigated immediately after, see below.

Fixed point still holds byte-for-byte; 7/7 typecheck_corpus still
rejected; Rust suite 1934/0.

### The exit-time leaks were two real bugs, not "locals aren't released" (2026-08-18)

16 of 19 `exec_corpus` fixtures leaked at exit. The obvious explanation
— that `main`'s locals are simply never released — was wrong, and
reading the generated IR is what said so. For `println(1.to_string())`
the release is right there:

```llvm
  %t0 = call ptr @plum_int_to_string(i64 1)
  %t1 = call i1 @plum_println(ptr %t0)
  call void @plum_rel_str(ptr %t0)
```

The reference counting was never the problem. Two unrelated causes
were.

**1. Leaked scratch buffers in the runtime.** `plum_int_to_string`
mallocs a 32-byte buffer, formats into it, and hands it to
`plum_str_new` — which COPIES the bytes into a fresh cell. The buffer
is dead on return and was never freed: 32 bytes per `.to_string()`,
which is exactly what ASan reported, one allocation per call.
`plum_float_to_string` had the same shape (64 bytes), and `read_file`
the worst version of it — it leaked THE WHOLE FILE on every read. Three
`@free` calls. The Rust backend uses an `alloca` for the same job and
so never had this bug.

**2. `Array.map`/`filter`/`fold` released neither operand.** Both the
source array and the closure arrive OWNED — `cg_expr`, not
`cg_borrow` — so both are the call's to release, and neither was. This
is why it hid so well: every single-construct test came back clean,
because an array used once has its reference consumed elsewhere. It
only appears when the same array is mapped AND filtered, which is why
`arrays` was the worst fixture in the corpus at 896 bytes.

Result: **16 leaking -> 2**, no use-after-free/double-free/overflow
anywhere, 19/19 still correct, fixed point still byte-identical, 7/7
`typecheck_corpus` still rejected.

The two survivors — `closures_in_structs` (24 bytes) and `generics`
(72) — looked like the closure-capture gap this backend documented as
architectural. They were not; see the next section. **The corpus is now
19/19 leak-free.**

The method that found both: ASan as the oracle, then MINIMISE. Every
one-construct fixture was clean, which is itself the clue — it said the
bug lived in a combination, not a construct.

### The closure "capture leak" was one missing decrement (2026-08-18)

The last two leaking fixtures were attributed to a documented
architectural limitation: captures are incremented when stored and
never released, "because a closure's captures are not visible from its
type and its release cannot walk them", so fixing it would mean adding
a release-function pointer per closure cell the way the real backend
does.

Every part of that was false about this backend's own code. A closure
cell ALREADY carries a release-function pointer at offset 16.
`<fn>_rel` is ALREADY emitted beside each lifted function — the one
place the capture list is known — and walks exactly the captures that
literal took. `@plum_rel_closure` ALREADY loads that pointer and calls
it before freeing. The machinery was complete and correct.

The bug was one missing decrement. `cg_call_closure_value` receives its
callee OWNED — `cg_expr` increments when it reads a variable — but
invoking a closure does not consume it, so the reference taken in order
to make the call was never given back. Visible directly in the emitted
IR for a function taking a closure parameter: it arrives at rc=1, the
read increments it to 2, scope-end releases it back to 1, and there it
stays.

```llvm
  %t54 = load ptr, ptr %s2
  call void @plum_rc_inc(ptr %t54)      ; read: rc 1 -> 2
  ...
  %s2_end = load ptr, ptr %s2
  call void @plum_rel_fnInt_Int_to_Int(ptr %s2_end)   ; rc 2 -> 1, leaked
```

One `cg_dec_cg` after the call. **`exec_corpus` is now 19/19 correct
AND 19/19 leak-free, with no use-after-free, double-free or overflow
anywhere.** Fixed point still byte-identical, 7/7 `typecheck_corpus`
still rejected, Rust suite 1934/0.

The `ITFunction` release path was also changed to delegate to
`@plum_rel_closure` rather than open-code its own dec-and-free, which
would skip that release pointer. That is independently correct but
fixed nothing on its own — it was tried FIRST, as the obvious
candidate, and changed no measurement at all. The emitted IR is what
identified the real cause, for the third leak in a row.

Worth stating as a pattern, since it recurred all through this work: a
comment asserting something is impossible is a claim about code, and
claims about code can be checked. This one had been true of an earlier
design and was never revisited when the release-pointer machinery
landed.

## `build` in the self-hosted compiler, and the road to deprecating the Rust one (2026-08-18)

Brad's goal: retire the Rust backend once the self-hosted compiler is
solid. That reframes the leak work above — `plum-ir`'s untracked-
temporary leaks are the documented consequence of `fbip` having no type
information, and fixing THAT is precisely the effort deprecation throws
away. Recorded as a deliberate won't-fix instead: the Rust backend
leaks on 19/19 `exec_corpus` fixtures (16,977 bytes total, 13,270 of it
in `accumulator`), against the self-hosted backend's 19/19 clean.

### The gap that actually blocked deprecation

The self-hosted compiler could `check`, `run` and `emit-llvm` — but not
produce a BINARY. It could compile itself to IR and not to an
executable, so it could not replace the compiler that builds it. That
one gap blocked deprecation outright; every other gap merely limits
what programs it accepts.

`build` = `emit-llvm` + `clang`, and it now bootstraps:

```
shbld build bootstrap/self_host -o g2     # built by the Rust compiler
g2    build bootstrap/self_host -o g3     # built by ITSELF
g2.ll == g3.ll                            # byte-identical
```

`g3` then builds and runs all 19 `exec_corpus` fixtures correctly and
rejects all 7 `typecheck_corpus` ones. (The BINARIES differ — embedded
paths and build ids — which is why the IR is the invariant here, the
same choice `bootstrap-check` already made.)

Two new runtime primitives, both through the established three-layer
pattern (a `prelude.plum` stub for the checker's signature, interception
in `cg_runtime_fn`, real IR in `runtime.plum`): `write_file`, and the
handle-based process set. The C shims were already linked into every
self-hosted binary, so `process_run` needed only a `declare`.

### Two mistakes, both from inventing an API instead of matching one

`main.plum` is compiled by BOTH compilers, so anything it calls must
mean the same thing in each. A simplified `run_process (..) -> Int`
failed immediately for that reason. Rewritten to the real
`Result[ProcessResult, String]`.

Subtler: the self-BUILT compiler passed `clang` a `-o` with no value,
while the Rust-built one worked from identical source. The join was at
fault. `process_run` points `argv[1]` at the WHOLE joined string, so a
LEADING separator makes `argv[1]` empty and pushes the last real
argument past `argc`, silently dropping it. The real prelude's
`join_args_acc` special-cases `i == 0`; an `Array.fold` that prefixes a
separator to every element is NOT equivalent. The first diagnostic
compared the same hand-written join expression under both compilers,
found it identical, and proved nothing — it never exercised the real
prelude's helper at all. Reading `process_shim.c`'s parsing loop is
what found it.

### `build` does not type-check, on purpose

It calls `cg_emit_program` without `check_program`, matching
`emit-llvm`. The backend prepends the prelude and checks the COMBINED
program; a separate check would check the wrong one — the user's items
without the prelude — and reject every prelude call lacking a
`builtin_sig` entry.

That difference bit immediately: `build` worked while `check
bootstrap/self_host` failed with "unbound function: write_file", and
the bootstrap fixed-point test caught it. Fixed by giving `write_file`/
`run_process` `builtin_sig` entries beside `read_file`, and adding
`ProcessResult` to `builtin_context` — `main.plum` reads `.exit_code`
and `.stderr`, so checking this compiler's own source needs the field
layout, not just the name. That list's own comment invites exactly this
("extending this list is the right move when a specific missing
declaration blocks a specific real program").

### What remains before the Rust backend can go

**Run `./bootstrap/example-sweep`.** That is the answer, not a list kept
here.

The hand-maintained table this section used to hold was wrong three
times running, always understating what was left. It listed four rows
while six of nine `examples/` failed; it claimed `check` rejects
contracts, which had not been true for a long time; and it never
mentioned the stdlib, which turned out to be the largest gap by far (18
of the real compiler's 78 associated functions). A list you have to
remember to update is not a source of truth. The sweep builds and runs
every example through BOTH compilers and diffs the output, so it cannot
drift.

It also finds what a table cannot. `adts_and_matching` began COMPILING
after the stdlib batch and printed the wrong answer — the self-hosted
backend was silently ignoring match arm guards (see below). No amount of
list-keeping surfaces that; only running the program does.

Known-and-deliberate exclusions live in the script itself, next to the
reason (`asteroids` opens a window), rather than in prose here.

### `.to_string()` on aggregates (2026-08-18)

The largest remaining language gap, and the one most likely to stop a
real program. Structs, enums (payload or not), and arrays now render,
matching the real compiler byte for byte:

```
Outer { name: "o", items: [1, 2], inner: Inner { v: 9 } }
[Inner { v: 1 }, Inner { v: 2 }]
Pair(3, "x")
Nil
[[1], [2, 3]]
```

Including the quoting rule: a `String` nested inside an aggregate
renders QUOTED, while `"hi".to_string()` at top level does not. That is
a debug rendering, where an unquoted empty string would be invisible.

A renderer per type, emitted from the SAME reachable-type set the
release functions use rather than from the call sites that print. A
struct's renderer calls its fields' renderers, so the set has to be
closed under nesting, and the release set already is — reusing it is
less machinery than a second discovery traversal, at the cost of
emitting a few renderers nothing calls.

**A correction to the gap table above**: tuple `.to_string()` was
listed as a self-hosted gap. It is not — the REAL compiler rejects it
too (`.to_string() not yet supported for Tuple([..])`). That was
parity, not a gap, and the table said otherwise because it was built by
grepping this backend's own error strings instead of by comparing the
two compilers. Corrected by running both.

**One deliberate divergence.** The real compiler REJECTS `.to_string()`
on an aggregate containing a closure. This one renders `<closure>`.
More permissive, and the alternative is worse: renderers are emitted
for every reachable heap type, so a struct with a closure field would
otherwise emit a call to a renderer that is never defined and fail at
LINK time — for a program that never printed the thing. A closure has
no text form in either compiler; the only question is whether that
costs you a link error. `<ref>`, `<tuple>` and `<?>` exist for the same
reason.

**Three self-inflicted bugs, all caught by running rather than
reasoning**, worth recording because two share a cause:

  * A SEGFAULT from seeding the accumulator with `null` on the claim —
    written into a comment — that `plum_str_concat` treats null as
    empty. It does not; it dereferences both operands immediately. The
    first literal is now itself the accumulator (static, so the
    releases are no-ops). Asserting the false thing in a comment was
    the worse half of that mistake.
  * A LINK failure on `closures_in_structs`: a call to
    `@plum_str_fnInt_Int_to_Int`, never emitted.
  * A TYPE error on `arrays`: the empty literal `[]` has element type
    `Unit`, so the renderer called `@plum_str_Unit(ptr %ev)` on an
    `i1`.

The last two are one cause: `cg_render_val`'s catch-all assumed every
remaining type was a renderable aggregate. It is now total.

19/19 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 7/7 `typecheck_corpus` rejected, Rust suite 1934/0.

### Destructuring `let` (2026-08-18)

`let (a, b) = ..`, `let P { x: px, y: py } = ..`, and nested forms like
`let ((m, n), o) = ..` now compile, matching the real compiler exactly.

The implementation is small because a destructuring `let` IS an
irrefutable one-arm match. `cg_pattern` already binds every name in a
tuple/struct pattern, takes a reference per binding, and reports the
slots it created — so this reuses it rather than growing a second
binding path. The `test` it returns is discarded: the checker has
already established the pattern is irrefutable, which is exactly what
makes this a `let` and not a `match`. The one rule to respect is
ordering — `code` (the field loads) before `binds` (the stores and
increments) — which the match-arm emitter documents.

Bindings borrow from the scrutinee and take their own reference, so the
scrutinee's own reference is released once they have. ASan-clean, which
is the check that matters here: one release too few leaks and one too
many is a use-after-free.

**This closes the last construct where `check` said `ok` and codegen
then refused.** Every remaining gap is now rejected by the CHECKER,
with a source location — which was the point of prioritising it. That
failure shape (accept, then fail late without a span) is the same one
behind the stale-binding soundness bug earlier in this file, and it is
now absent from the language surface.

19/19 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 7/7 `typecheck_corpus` rejected, Rust suite 1934/0.

### `test` in the self-hosted compiler (2026-08-18)

Matches the real harness's output and exit code:

```
running 3 tests
test test_one_ok ... ok
test test_boom ... FAILED
test test_after_boom ... ok

failures:

---- test_boom ----
array index out of bounds

test result: FAILED. 2 passed; 1 failed        (exit 1)
```

**Each test runs in its OWN PROCESS**, and that is the design, not an
implementation detail. `panic_raw` aborts rather than returning a
catchable error, so a single-process harness stops at the first failure
and cannot report the rest — which is exactly why the real compiler's
NATIVE path is built the same way (`testing.rs`: "there is no way to
run more than one test per compiled PROCESS and still observe every
test's own outcome"). `test_after_boom` running after `test_boom` died
is the proof it works. The child is this same binary re-invoked through
`/proc/self/exe` — `args()` does not include the program's own path —
and `run_process`'s captured output becomes the failure report,
printed under the test's name rather than interleaved with progress.

The parent type-checks ONCE; children skip it, since they load the same
program and re-checking per test would dominate the run.

**Discovery needs more than the name prefix.** A `test_`-prefixed
helper that takes real arguments is not a test, and calling it with
none reads out of bounds instead of reporting a failure. This was not
hypothetical for five minutes: this compiler's own `cmd_test`/
`cmd_test_one` were originally named `test_*`, and pointing `test` at
`bootstrap/self_host` "discovered" and crashed on both. Note the real
compiler's `discover_tests` filters on the name alone and would
mis-discover the same way.

The obvious filter is wrong too. `Array.is_empty(def.params)` reported
**0 tests** for genuine test files, because `let f ()` parses as ONE
parameter holding the empty-tuple pattern, not as zero — the unit-
argument convention, visible in `dump-ast` as `(let f (((tuple))) ...)`.
`takes_no_args` accepts either shape.

19/19 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 7/7 `typecheck_corpus` rejected, Rust suite 1934/0.

### `|>` in the self-hosted compiler (2026-08-18)

Desugared in the CHECKER and the INTERPRETER, not in the parser:
`render_expr` must keep printing `(|> x f)` to match
`bootstrap/corpus/expressions/pipe_*.expected`, so the pipe node has to
survive into the parse tree. That is the same reason the real parser
keeps `BinaryOp::Pipe` and desugars separately in `infer.rs` and
`lower.rs` — one shared `parser.desugar_pipe` serves both self-hosted
consumers, and codegen never sees a pipe at all because the checker
rewrites it before building a `TExpr`.

Semantics mirror `infer.rs::infer_pipe` exactly: `x |> f` is `f(x)`,
`x |> f(a, b)` APPENDS — `f(a, b, x)` — and a single `_` argument
claims the position instead (`x |> f(a, _)` is `f(a, x)`). Two
placeholders are an error rather than a silent choice. `_` needs no
grammar support: `parse_argument` already turns a bare `_` into the
identity closure `|_| _`, and pipe gives that one shape a meaning
inside a piped call's argument list and nowhere else.

### Refutable nested patterns — two different miscompilations (2026-08-18)

Found while adding `|>`: the checker arm for it is
`EBinary(BPipe, lhs, rhs)`, a variant pattern nested inside another,
and the self-hosted compiler built from that source began treating
EVERY binary operator as a pipe. `n * 2` became `2(n)`.

**The Rust backend never tested the inner tag.**
`wrap_nested_destructures` compiles each nested pattern into a
single-arm `Match` — a shape with no way to fail — which is right for a
tuple or a struct (one possible tag) and wrong for an enum variant.
`ENode(OAdd, 1)` ran the `ENode(OMul, a)` arm's body. Silently: the
LLVM backend printed a wrong answer with no diagnostic, and the
interpreter raised a bare "no match arm for tag OAdd" that reads like an
exhaustiveness bug in the user's program. A nested LITERAL pattern was
already rejected with a clear "not yet implemented" error; the variant
case fell through to being treated as a binding instead.

The fix reuses two things that already existed. Refutability is decided
by `ctx.variants` (`nested_pattern_is_refutable`), which also tells a
positional STRUCT pattern from a variant one — both parse as
`Pattern::Variant`, only a real variant's tag is in that map. The test
itself becomes a synthesized arm GUARD (`nested_tag_test`), because a
guard's documented semantics are already exactly what a failed nested
tag needs: "skipped as though its tag hadn't matched at all," on to the
next arm. The guard is an ordinary `Match` used as a Bool, with
`DEFAULT_ARM_TAG` supplying the false branch, so no new IR node or
`MatchArm` field was needed. Sub-patterns recurse, conjoined with `&&`.
The existing "no guard on an arm with a nested pattern" restriction
stays: it exists because a USER guard would reference names that only
exist deeper in the destructure chain, and a synthesized one references
only the synthetic top-level bindings that are already in scope.

**The self-hosted backend tested the tag but dereferenced first.** Its
patterns are deliberately flat — `PatEmit` computes an `i1` with no
control flow, documented as safe because `cg_payload_offset`
over-allocates every cell, so loading another variant's payload word is
always in bounds. True for the load; not true for what a nested
sub-pattern does next, which is follow that word as a POINTER to read an
inner tag. `Option[Option[Int]]` matched against `None` produced correct
output normally and a SEGV under ASan — the giveaway was `0xbe` bytes
in the faulting register, ASan's fill pattern for allocated-but-
uninitialized memory.

Fixed without giving up flatness: a dereferencing sub-pattern is handed
`select i1 <tags matched so far>, ptr <real payload>, ptr @plum_pat_safe`.
`@plum_pat_safe` is a global whose word 1 is zero — the tag for an
enum, the length for a string or array, so an inner test simply fails —
and whose every remaining word points back at itself, which is what
makes ONE fixed cell stand in for a payload chain of unbounded depth. It
is `global`, not `constant`: a stray write must corrupt a dead word
rather than fault on a read-only page. Binding is unaffected, because
`select` yields the real payload on the matching path and `binds` only
ever runs there.

Both regression tests assert on the ANSWER, not the IR. This bug was
invisible to every other check: the IR looked reasonable, the type
checker was happy, and the corpus goldens (which render the parse tree)
could not see it at all. `bootstrap/exec_corpus/nested_patterns` covers
the self-hosted half, ASan-clean rather than merely correct.

21/21 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 99/99 parser goldens, 7/7 `typecheck_corpus` rejected,
Rust suite 1934/0.

### Refutable patterns where nothing can fall through (2026-08-18)

The other half of the nested-pattern family, found by checking the
positions the `match` fix did not touch:

| position | refutable nested pattern |
|---|---|
| `match` arm | synthesized guard (above) |
| `let` destructuring | already rejected, in the checker |
| `for` pattern | already rejected |
| **function parameter** | **segfaulted** |

`let f (W { op: OMul, n }: W): Int = n`, called with an `OAdd`, ran the
body against another variant's data — SEGV under the LLVM backend,
"no match arm for tag" under the interpreter, which reads like the
user's own bug. A guard cannot help here: a parameter has no next arm
to fall through to, which is exactly what makes REJECTING it the only
correct answer.

`wrap_destructure` is the single funnel for every irrefutable
destructuring position (function parameters via `lower_params`, and
`select`'s receive binding), so the check lives there, and
`refutable_tag` names the offending tag in the message rather than
just asserting something can fail.

One rough edge left, pre-existing and not specific to this check:
reached through `monomorphize.rs`, every lowering error is stringified
(`.map_err(|e| e.to_string())`) and loses its span, so `plum build`
reports this without a source location while `plum run` shows the
underlying line. Worth fixing at the `monomorphize` boundary, for every
lowering error at once, rather than here.

### `require`/`ensure` in the self-hosted compiler (2026-08-18)

Two prelude one-liners, no backend work — the same implementation the
real compiler uses:

```plum
let __contract_require (cond: Bool) (msg: String): Unit =
    if cond { () } else { panic_raw(msg) }
```

The parser already desugars every clause into a call to one of these
before the checker or the backend sees the body, so the whole feature
is a prelude declaration plus a `builtin_sig` entry (for `check`, which
runs without the prelude). The self-hosted checker had been
special-casing both names to produce a `TUnsupported` node; deleting
that special case is what turned them into ordinary calls.

Violations now match the real compiler exactly, on both paths:
`precondition failed: withdrawal amount must be positive`, exit 1.

**`println` of a non-String** was the actual thing blocking
`examples/contracts`, not contracts at all — the example prints
`account.balance`, and the self-hosted backend accepted only a String.
It renders through the same per-type machinery `.to_string()` already
used, so the two cannot disagree. The paths differ only in ownership:
a String argument is borrowed, a rendered one is a fresh cell released
as soon as `@plum_println` returns.

`examples/contracts` now builds and runs identically under both
compilers.

22/22 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 99/99 parser goldens, 7/7 rejections, Rust suite
1938/0.

### Equality that would have to compare a function (2026-08-19)

Listed as a self-hosted CODEGEN gap. It was not: the REAL compiler had
it too, and the shape of the failure was the interesting part.

`f == g` on two closures type-checked in both compilers and then failed
in a backend — "cannot compare Closure(0) and Closure(1)" at
interpreter runtime, "Eq is not supported for Closure([Int], Int)
operands" from codegen. Neither carries a source location, which is the
same complaint that made this a listed gap in the first place.

One step further out the two backends did not merely both fail, they
DISAGREED. A struct with a closure-typed field was a runtime error
under the interpreter and printed `true` under the LLVM backend — for
two *different* closures, because `@plum_struct_eq` never meaningfully
compared that field. A silently wrong answer, which is worse than
either error.

So the fix is a type error, in both compilers, for `==`/`!=` on any
type that CONTAINS a function — directly, through a struct field, a
variant payload, a tuple element, or a type argument (which is what
covers `Array[(Int) -> Int]`, whose element type is nobody's declared
field). Two functions have no equality worth defining: structural
equality of code is not something Plum can offer, and pointer identity
would make `(|n| n + 1) == (|n| n + 1)` depend on whether the optimizer
happened to share the two closures. The `Eq` BOUND already reasoned
this way — `satisfies_bound` excludes what codegen cannot compare — so
this extends an existing rule to concrete types rather than inventing
one.

The walk (`first_function_within`, mirrored in both checkers) guards
recursive declarations by NAME. That can only ever under-report, never
over-report: a false negative leaves the old behaviour exactly as it
was, while a false positive would reject a legitimate comparison. A
regression test compares an `enum List { Cons(Int, List), End }` for
exactly that reason — it hangs rather than fails if the guard breaks.

One bug caught while writing the self-hosted half, worth recording
because it is a trap the Rust version dodged by accident: `Array` is
BUILTIN, so `ctx_field_types(ctx, "Array")` is an ERROR ("unknown
struct: Array"), not an empty answer — the self-hosted compiler stopped
type-checking itself. Rust's `struct_fields_for` returns an `Option`
and simply answered `None`. An undeclared aggregate's element type is
always a type ARGUMENT, already walked.

22/22 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 99/99 parser goldens, 9/9 `typecheck_corpus` rejected,
Rust suite 1941/0.

### The example sweep, and the three bugs it found (2026-08-19)

`bootstrap/example-sweep` replaced the hand-maintained gap table. Its
first run reported 7 of 9 examples failing where the table listed four
rows total — and the causes were mostly things the table never
mentioned.

**Stdlib parity was the big one.** The self-hosted prelude declared 18
associated functions; the real compiler has 78. Adding `Option`/`Result`
combinators, `Array` queries (`find`, `find_index`, `first`, `last`,
`take`, `drop`, `all`, `index_of`, `sum_*`) and the `Int`/`Float`
numerics fixed three examples outright. They are ordinary Plum, mirrored
from `crates/plumc/src/lib.rs`. Each also needs a `builtin_sig` entry:
`check` runs WITHOUT the prelude, so a function that exists only in
`prelude.plum` is invisible to it. `Map` and `Set` are still entirely
absent — they are recursive generic enums and want their own batch.

**Match arm guards were ignored by the self-hosted backend.** The
checker types a guard (`infer_match`) and every other codegen pass walks
it (`cg_rel_in`, `cg_eq_in`, `cg_reads_max`), but `cg_match_arms` — the
one place that turns an arm into blocks — never read it, so a guarded
arm ran unconditionally. `adts_and_matching` reported a rectangle as
"(a square!)". A guard now gets its own block between the tag test and
the body, and a false guard branches to the next arm exactly as a failed
tag test does. The arm's bindings move into that guard block, because
the guard reads them; a guard that then fails leaves an extra reference
in a slot the function's own exit release already covers — delayed, not
leaked, and the corpus is ASan-clean.

This is the third silent wrong-answer bug in a week (after nested
pattern tags and struct-with-closure equality), and the pattern is
consistent: every one of them type-checked, emitted plausible IR, and
was only visible by running a real program and looking at the output.

**A closure argument was inferred in isolation.** `Array.find(xs, |it|
it.price > 0)` failed with "field access requires a struct value with a
statically known type, found T1", even though `xs: Array[Item]` pinned
the element type one argument earlier — the closure body was checked
before the argument was ever unified with the parameter. The fix is the
generalization the real checker already made for the same reason:
`infer_closure_seeded` existed but only `Array.map`/`filter`/`fold` used
it, so every OTHER callback-taking function, stdlib or user-defined, hit
this. `infer_call_args` now seeds any closure argument from its
parameter type. The self-hosted checker is now slightly AHEAD of the
real one here: `examples/option_result` documents an annotation as
"required", and this checker no longer needs it.

Sweep: 5 of 9 matching, from 2. Remaining: `asteroids` (an ALL-CAPS
constant in an `if` condition parses as a struct literal — the
`no_struct_literal` suppression `parser.plum` documents as a v1 cut),
`concurrency` (channels, a documented scope cut), `currying` (partial
application unsupported in the self-hosted checker), `json_and_files`
(the JSON stdlib, ~200 lines of prelude Plum).

23/23 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 99/99 parser goldens, 9/9 `typecheck_corpus` rejected,
Rust suite 1941/0.

### The JSON stdlib, and a leak in every closure (2026-08-19)

`json_and_files` was the third example the sweep still failed on. The
JSON library is ~245 lines of ordinary Plum in the real compiler's
prelude, so it was mirrored mechanically — extracted from
`STDLIB_JSON_SRC`, un-escaped, re-escaped for embedding, and inserted
character-for-character apart from dropping its own `chars_of` (the
self-hosted runtime already provides that one; it is the single thing
this prelude cannot write in Plum). Copying rather than rewriting is
the point: the two compilers agree on number formatting, escape
handling and parse errors by construction rather than by inspection.
`check` also needs `JsonValue`/`JsonEntry` seeded into
`builtin_context`, for the same reason `Option`/`Result`/
`ProcessResult` are there — it runs without the prelude.

Every name was checked against the compiler's own source first. The
prelude is prepended to every program INCLUDING the compiler itself,
and this bootstrap has already been bitten once by an unprefixed
prelude helper shadowing a module's own function.

**Then the corpus fixture leaked, and the example did not.** Chasing
that found something much larger than JSON: a lifted closure emitted
neither its body's slot releases nor any release for its own
PARAMETERS, where a named function emits both (`cg_fn` concatenates
`body.releases` and `entry.releases` before `ret`; the closure path
concatenated neither).

The parameter half is what bites. A caller hands a closure an owned
value — `Array.map` explicitly increments each element first, "the
closure consumes what it is given" — and nothing ever gave it back. So
`Array.map`/`filter`/`fold` over an array of HEAP elements leaked one
cell PER ELEMENT. That is why `json_stringify` leaked in proportion to
the document it was printing, and why the leak was invisible for
`Array[Int]`: an `Int` element's increment is a no-op, so every earlier
corpus fixture happened to use exactly the case that could not show it.

Captures are deliberately NOT released there: they belong to the
closure CELL, and `<fn>_rel` walks them when the cell dies. Releasing
them per CALL would free them out from under the next invocation.

Worth noting how this was found. The output diff that started it was a
red herring — ASan's leak check `_exit`s and discards buffered stdout,
so a leaking program looks like a program printing nothing. One bug
wearing two costumes.

Sweep: 6 of 9 matching. 24/24 `exec_corpus` correct and leak-free,
self-build fixed point byte-identical, 99/99 parser goldens, 9/9
`typecheck_corpus` rejected, Rust suite 1941/0.

### Currying in the self-hosted compiler (2026-08-19)

The residual value IS a closure — over synthetic parameters, with a
fully-applied call as its body — so the backend needed no new concept
whatsoever. That is the same conclusion the real compiler reached (see
"Currying (partial application)" above), reached one pass earlier here:
`plum_ir::lower` consults a span-keyed `partial_calls` map to rewrite
the node later, while this checker emits the `TClosure` directly,
because it produces a typed tree rather than a note for a subsequent
pass to act on. No side-channel, no new IR, no codegen change.

**Both call shapes needed it, which the first attempt missed.**
`infer_fn_call` covers `scale(2)` — a named function under-applied. But
`clamp3(0)(10)(-5)` under-applies a closure VALUE at the second step,
and that goes through `infer_closure_call`, which failed with "function
arity mismatch". Currying that works only for the first application in
a chain is not currying; the residual is now built in both places, with
a `TClosureCall` body where the callee is a value rather than a symbol.
The corpus fixture caught this — the hand-written smoke test happened
to stop at one level of chaining.

Three things that must still be errors, and are:

| written | result |
|---|---|
| `scale()` | arity error — the deliberate 0-of-N gate |
| `scale(1, 2, 3)` | arity error — over-application |
| `scale("x")` | `argument 0: String != Int`, not a misleading arity error |

The first is pinned by `typecheck_corpus/zero_arg_is_not_partial`,
because it is a CHOICE rather than a consequence: nothing in the
implementation would stop `args.is_empty()` from taking the partial
path.

One inherited caveat, stated rather than discovered later: the supplied
argument expressions end up inside the closure body, so they are
re-evaluated per call of the residual rather than once at
partial-application time. The real compiler's rewrite does exactly the
same, and expressions of this kind are pure, so the two are
observationally identical.

Sweep: 7 of 9 matching. 25/25 `exec_corpus` correct and leak-free,
self-build fixed point byte-identical, 99/99 parser goldens, 10/10
`typecheck_corpus` rejected, Rust suite 1941/0.

### The struct-literal/block ambiguity, and what asteroids really needs (2026-08-19)

`parser.plum`'s top comment listed `no_struct_literal` suppression as a
deliberate v1 cut: "none of the 98 corpus fixtures exercise an `if`/
`match` with a struct-literal condition... revisit if a fixture ever
needs it." A real program needed it, and the shape is not exotic at all:

```plum
if pos.x > SCREEN_WIDTH_F { pos.x - SCREEN_WIDTH_F }
```

`SCREEN_WIDTH_F` is capitalized, so a capitalized-path-followed-by-`{`
was read as a struct literal and the `if` BODY was parsed as field
initialisers. **Any all-caps constant in an `if`/`match`/`for` head hits
this** — the error it produced ("a nested field-update path needs an
explicit ': value'") points nowhere near the cause.

The real parser carries `no_struct_literal` as mutable parser state.
This one is a pure function of `(tokens, pos)`, so the flag is threaded
as a parameter down the whole precedence chain — seventeen functions —
to its single consumer, `parse_path_shaped_expr`. Suppression stops at
any BRACKET: inside `(..)`, `[..]`, an index or an argument list the
ambiguity cannot arise, and those positions re-enter through the plain
`parse_expr`, which pins the flag back to false. That is the same
re-entry the real parser spells `parse_expr_allowing_struct_literal`.

**Currying quietly changed what a threading mistake looks like.** Two
call sites missed the new argument, and instead of an arity error the
compiler reported `parse_pipe`'s BODY as having type `Function([Var],
ExprResult)` — the under-applied call had become a closure. Still
caught, one step removed from the cause. That is the cost side of the
`f(a)(b) === f(a, b)` equivalence, and worth knowing about rather than
rediscovering.

**asteroids is not close.** With the parser fixed it now reaches the
type checker and stops at `Float.sqrt`/`Float.pow`/`Float.random` —
which look like a stdlib gap but are not: the real prelude defines them
as `unsafe { sqrt(x) }`, and the file itself declares `extern "C"` and
uses twelve `unsafe` blocks to drive raylib. It is an FFI program. The
self-hosted backend has no `extern`/`unsafe` support at all, which puts
asteroids in the same category as `concurrency`: a genuine scope cut,
not a near miss.

Both are now annotated IN the sweep, next to their reason. They still
report FAIL and still count as failing — silencing them is exactly what
turned the old gap table into fiction. The annotation only saves the
reader from re-deriving why.

Sweep: 7 of 9 matching, 2 failing, both annotated. 25/25 `exec_corpus`
correct and leak-free, self-build fixed point byte-identical, 101/101
parser goldens (two new: `if_capitalized_condition`,
`match_capitalized_scrutinee`), 10/10 `typecheck_corpus` rejected, Rust
suite 1941/0.

### AST positions, and type errors that say where (2026-08-19)

The second half of the spans work. Parse errors got locations first
(token offsets plus the index every parse function already threads);
TYPE errors needed positions on the AST itself, because the checker
walks a tree, not a token stream.

**The constraint that shaped the design**: the checker runs on a MERGED
program — every module's items concatenated into one flat list (see
`collect_project`) — so by the time an error is reported there is no
ambient "current file" to attribute it to. A position therefore has to
carry its FILE with it. `ItemNode` gained `path` and `start`; `PBlock`
gained `starts`, one character offset per statement plus a trailing one
for the tail expression. The source is re-read from disk to render,
which only ever happens on the failure path — the compiler is about to
abort anyway.

**Granularity is a statement, not an expression, and that was a
choice.** The real compiler carets the exact subexpression:

```
real:  4 | let c (p: P): Int = p.nope
         |                     ^^^^^^
self:  4 | let c (p: P): Int = p.nope
         | ^
```

Expression precision would mean a position on all twenty-odd `PExpr`
variants — 181 match sites and 113 construction sites across the
parser, interpreter, checker and backend. Statement granularity cost
roughly a dozen edits, because `ItemNode`/`PBlock` are constructed in
exactly 12 places (all in the parser) and read everywhere else by FIELD
NAME, so adding fields broke nothing. Two large mechanical rewrites
earlier the same day had each introduced a bug the compiler then caught
(a duplicated parameter, a mis-scoped in-chain detection), which is a
fair prior on how a 294-site version would have gone.

The thing that actually matters — turning "somewhere in your project"
into "this line" — is bought at statement level. Going from there to the
exact column is a refinement that can be made later without redoing any
of this: `PBlock.starts` and `ItemNode.start` stay exactly as they are,
and only the leaf nodes gain positions.

Both parallel arrays (`LexedSource.starts`, `PBlock.starts`) follow the
same rule for the same reason: the thing being annotated is matched on
in dozens of places that have no use for a position, and none of them
should have to unwrap a wrapper to keep working.

A -1 offset means "unknown" throughout — a synthesized block (contract
desugaring), or a parse with no source context set. It falls back to the
enclosing item's position, or to the bare message, so nothing depends on
positions having been recorded.

25/25 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 101/101 parser goldens (positions are invisible to
`render_program`, by design), 10/10 `typecheck_corpus` rejected — all
ten now reporting a file and line — sweep 7/9, Rust suite 1941/0.
Self-compilation 0.86s.

### Map and Set, and the two features they exposed (2026-08-19)

Mirrored from `STDLIB_COLLECTIONS_SRC` the same way JSON was. `Map` is a
real hash map, which makes one detail load-bearing that a smoke test
would never catch: the bucket index is `String.hash(..) % bucket_count`,
so `Map.keys` and `Map.values` enumerate in HASH order. The two
compilers agree on a map's printed contents only if their hashes agree
exactly — including the final `& i64::MAX`, which clears the sign bit so
`% bucket_count` can never index negatively. A first attempt left the
mask off and every hash differed, caught immediately because
`String.hash("")` is the offset basis only if you forget it.

`String.hash` itself is a new runtime primitive (FNV-1a, the same
algorithm both real backends implement independently).

**Two gaps fell out of the mirror, neither specific to collections.**

`for x in xs` over an ARRAY. The self-hosted checker accepted only a
literal range, so `for bucket in m.buckets` was rejected — but so is any
ordinary program that iterates an array, which is most of them. It is
now `TForArray`, its own typed node rather than a flag on `TFor`: a
range has two bounds and an array has one iterand, they share no
operands and no codegen, and folding them together would be a tag
pretending to be a shape. Ownership follows `cg_array_map`'s rule — the
array is an owned temporary released after the loop; each element is
borrowed for one iteration, incremented on the way in and released on
the way out.

`xs.remove(i)`. Used exactly once in the prelude, by `Map.remove`, and
absent from this compiler entirely. The runtime does the copy; codegen
does the counting (every surviving element gains a reference, then the
source is released, which drops the removed element's last one).

**The spans work paid for itself here.** The failure was
`unknown struct: Array` on every program, with no location because it
came from the prelude — a string inside the compiler with no file to
point at. Four rounds of guessing at call sites got nowhere, and a
binary search over the prelude cost eleven seconds per step. What
actually found it: reconstructing the prelude as a real FILE and running
`./sh check` on it, which printed

```
error: unknown struct: Array
  --> .../main.plum:327:17
327 |     if i >= 0 { Map { buckets: m.buckets.set(idx, bucket.remove(i)), ... } }
```

— one command, exact line. That is the difference the previous commit
bought, demonstrated on the first real bug after it landed.

Two diagnostics were improved while hunting: the three
`unknown struct` context accessors now say which one asked, and a type
error inside a prelude function reports which function it was checking
(a prelude item has no file, so the function name is all it has).

Stdlib parity is 65 of 78. What remains splits cleanly: `Array.sort_*`/
`zip` and `String.lines`/`trim_*` are pure Plum; `Float.sqrt`/`pow`/
`floor`/`ceil`/`round` are `unsafe { }` calls into libm in the real
prelude, and `Float.random*` needs the `random_raw` primitive — so those
seven want runtime primitives, not prelude source.

27/27 `exec_corpus` correct and leak-free (two new fixtures:
`collections`, `for_array`), self-build fixed point byte-identical,
101/101 parser goldens, 10/10 `typecheck_corpus` rejected, sweep 7/9,
Rust suite 1941/0.

### The pure-Plum half of the remaining stdlib (2026-08-19)

`Array.sort_by`/`sort_int`/`sort_float`, `Array.zip`, `String.lines`/
`trim_start`/`trim_end` — mirrored from the real prelude, which is the
whole point: insertion sort is STABLE, and two implementations only
agree on equal keys if they are the same implementation.

`Array.sort_string` was deliberately LEFT OUT despite being pure Plum
in the real prelude, because its comparison goes through `a.runes()`
and this compiler has no such primitive. It belongs with `Float.sqrt`
and friends — the group needing runtime support, not prelude source —
and putting it here would have meant inventing a different string
ordering, which is exactly the kind of quiet divergence the mirroring
approach exists to prevent.

`Zipped` is a named struct rather than a tuple, as in the real prelude,
so it needed seeding into `builtin_context` alongside `Option`/`Result`/
`JsonValue` for `check` to know it.

Stdlib parity is now 70 of 78. The eight left all need runtime
primitives: `Float.sqrt`/`pow`/`floor`/`ceil`/`round` are `unsafe { }`
libm calls, `Float.random`/`random_range` need `random_raw`, and
`Array.sort_string` needs `.runes()`. Nothing pure-Plum remains.

**A tooling bug worth recording**: the corpus run appeared to fail on 18
of 28 fixtures with "CLANG FAIL", which looked like a codegen
regression from this change. It was `/tmp` filling up — the local
validation script created a temp directory per run and never removed
one, across roughly twenty runs on a RAM-backed tmpfs. The script now
traps EXIT. Worth noting because the failure mode impersonated a real
regression convincingly, and the actual error was two layers down in
`ld`, not in anything the compiler emitted.

28/28 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 101/101 parser goldens, 10/10 `typecheck_corpus`
rejected, sweep 7/9, Rust suite 1941/0.

### The last eight stdlib functions — 78/78 (2026-08-19)

All eight needed RUNTIME support rather than prelude source, which is
why they were held back from the pure-Plum batch.

`Float.sqrt`/`pow`/`floor`/`ceil`/`round` are `unsafe { sqrt(x) }` in
the real prelude. This backend has no `extern "C"`/`unsafe` support at
all, so they are runtime primitives reaching the same libm functions —
same results, different route. That is a real divergence in MECHANISM
worth naming: if `extern`/`unsafe` ever lands here (it is what
`examples/asteroids` needs), these should move back to being ordinary
prelude source.

`Float.random`/`random_range` follow the real NATIVE backend: libc
`rand()` over `RAND_MAX + 1`, seeded once from `time(0)` in the entry
prologue rather than per call. No output parity is possible or
attempted — the real INTERPRETER uses splitmix64 while both native
backends use `rand()`, so the two real backends already disagree on the
numbers. Only the distribution is shared.

`Array.sort_string` was the interesting one. The real prelude compares
`a.runes()` rune by rune, and this compiler has no decoder. Rather than
build one, the comparison is byte-lexicographic via `memcmp` — which
for UTF-8 IS codepoint order, a designed property of the encoding. So
the orderings agree exactly, with no decoder involved. The fixture
sorts `"Apple"`/`"app"`/`"apple"` specifically, because a
case-insensitive or length-first ordering would place those
differently.

One duplicate `declare i32 @memcmp` slipped in — `plum_str_eq` already
declared it — and LLVM rejected the module outright. Caught on the
first compile.

**Stdlib parity is 78 of 78.** Every associated function the real
compiler's prelude defines now exists in the self-hosted one.

29/29 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 101/101 parser goldens, 10/10 `typecheck_corpus`
rejected, sweep 7/9, Rust suite 1941/0.

### FFI: `extern "C"`, `unsafe`, and `CStr` (2026-08-19)

The self-hosted compiler can now call C. `examples/asteroids` — a
raylib game, twelve `unsafe` blocks, sixteen extern declarations —
builds from self-hosted IR and links against raylib.

The parser already handled `extern` blocks and `unsafe`; everything
else was new:

- **`ITCStr`/`CgCStr`** — a raw `char*`, deliberately NOT an alias for
  `String`, so the conversion has to be written. `.as_cstr()` borrows a
  Plum string's bytes (the runtime always NUL-terminates them, so it is
  a pointer adjustment, not a copy); `.as_string()` COPIES back, because
  whatever C returned may be static, stack, or freed on the next call.
  A `CStr` is not heap-shaped: nothing increments or releases one.
- **Extern signatures** enter the ordinary signature table with an
  `is_extern` marker, so ordinary call inference resolves them. No
  currying for an extern — there is no closure to build.
- **The `unsafe` gate** mirrors `plum_types::Infer::in_unsafe`,
  including being saved and restored around the block rather than
  simply set: a closure defined inside `unsafe` must not carry the
  permission to wherever it is later called.
- **Codegen** emits one `declare` per extern (unconditionally — a
  declaration costs nothing and externs have no bodies for the
  reachability worklist to reason about) and a direct call. C does not
  participate in reference counting, so arguments are borrowed.

**Three more gaps surfaced on the way, each found by asteroids failing
one step further along.** None were FFI:

1. `f()` where `f` was declared `let f (): T`. That declaration has ONE
   parameter — the empty-tuple pattern — so the call needs one argument,
   and the real compiler synthesizes it. This compiler's own source
   writes `f(())` throughout, which is exactly why the gap survived this
   long: nothing it compiled had ever used the sugar.
2. **Struct-literal spread** (`Game { score: 0, ..g }`). Carried to the
   backend rather than desugared in the checker, because the copied
   fields must be INCREMENTED as they are stored — desugaring to
   `TFieldGet` yields borrowed values where `cg_store_fields` requires
   owned ones, and every refcount would come out one short.
3. **Nested field update** (`Game { ship.rotation: r, ..g }`), expanded
   to `Game { ship: Ship { rotation: r, ..g.ship }, ..g }`. The
   expansion needs the INNER struct's name, which only the context
   knows — so it lives in the checker, exactly as the real compiler's
   `nested_struct_update` runs after declarations are collected.

Two duplicate `declare`s (`strlen`, and `memcmp` in the previous batch)
were rejected outright by LLVM. Cheap to fix, and worth noting that
adding a libc declaration means checking the runtime does not already
have it.

The sweep now LINKS an example it cannot run: "both compilers produce a
binary" is most of what running asteroids would have told us, and it
catches a codegen regression that emitting alone would not. Per-example
link flags live in the script, next to the reason.

**Sweep: 8 of 9.** Only `concurrency` remains.

30/30 `exec_corpus` correct and leak-free (new `ffi` fixture, with its
own C shim), self-build fixed point byte-identical, 101/101 parser
goldens, 11/11 `typecheck_corpus` rejected (new: the unsafe gate),
Rust suite 1941/0.

### Concurrency, and the sweep reaching 9 of 9 (2026-08-19)

`spawn`, `.join()`, `channel[T]()`, `.send()`, `.recv()` — the last
sweep row. **Every `examples/` project now builds under both compilers**
and produces identical output (asteroids build-only: it opens a window).

Threads and channels come from `native_stdlib/thread_shim.c` rather
than from hand-emitted pthread IR. The real backend does emit its own
(`emit_channel_runtime`); this backend reaches the same primitives
through a shim, the same split it already uses for directories and
processes. A mutex/condvar queue is a lot of fiddly text to get right in
LLVM IR and none of it is Plum-specific. The shim compiles clean under
`-Wall -Wextra`, uses the `while`-not-`if` predicate loop that spurious
wakeups require, and signals while holding the lock.

Two design choices worth stating:

**`spawn { body }` becomes a zero-parameter CLOSURE in the checker.** A
spawn body captures its enclosing locals exactly as a closure does, so
reusing the closure path means the capture machinery, the lifting and
the release function were all already written. The backend adds only a
per-site entry function matching `void *(*)(void *)`, which invokes the
closure and boxes the result. Boxing rather than stuffing the value into
the pointer: a `Float` does not fit in a pointer on every target, and
one boxed word is the same shape for every element type.

**`Task`/`Sender`/`Receiver` are bare handles**, `Int`-shaped, with no
cell and nothing to release. Both channel ends are the SAME handle
underneath — only their types differ, which is what makes `.send` on a
`Receiver` a type error rather than a runtime one.

`channel[T]()` is also the one place explicit type arguments mean
anything, in either compiler — recognised by shape, matching the real
compiler's own `GenericInst`-callee match.

**One honest exception**: `exec_corpus/concurrency` is the corpus's only
expected leak. A task nobody joins is never freed — its handle struct,
boxed result and closure all outlive the program — and that is true of
BOTH compilers; the real one leaks 734 bytes to this backend's 608 on
exactly that program. Fixing it needs the Plum side to signal that a
`Task` died unjoined, which a bare handle cannot do. It is listed in the
validation script with that reason, not silenced.

Two validation-harness bugs were fixed while landing this, both of which
had impersonated real failures: the leak check and the output comparison
now run as separate passes, because LeakSanitizer `_exit()`s after
reporting and discards buffered stdout — a leaking program looks like
one that printed nothing.

**Sweep: 9 of 9.** 31/31 `exec_corpus` correct and leak-free, self-build
fixed point byte-identical, 101/101 parser goldens, 11/11
`typecheck_corpus` rejected, Rust suite 1941/0.

### A language server, in Plum (2026-08-19)

`./sh lsp` speaks LSP over stdin/stdout and publishes real diagnostics.
`bootstrap/lsp-smoke` drives a full session — initialize, open a file
with a known error, assert the diagnostic lands on the right line, shut
down.

**Two prerequisites I had called blockers turned out not to be.**

"No async" was wrong: a language server is a request loop, and a
synchronous one is a correct server. The debouncing the real LSP does
exists to fix a race between overlapping re-checks that a
single-threaded loop cannot have. What was actually missing was
BLOCKING STDIN, which is a shim function, not an architecture.

"Needs expression spans" was half wrong. Hover and go-to-definition are
position QUERIES and do need them. DIAGNOSTICS need only the position of
the error, which statement granularity already gives — and diagnostics
are the half that makes an editor useful.

**Diagnostics come from a subprocess.** `fail_tc` reports by aborting,
so an in-process check would take the server down on the first type
error a user typed, which is every keystroke of a half-written program.
The child is this binary re-invoked as `check` — the same
`/proc/self/exe` trick `test` uses — and its own diagnostic text,
already carrying `path:line:col`, is parsed straight back. The format is
coupled on purpose: one renderer (`lexer.render_at`), one parser.

**Two portability constraints shaped the code**, both found by the real
compiler rejecting the first version:

1. An `extern "C"` block only works in the ROOT module. The real
   compiler prefixes a module's item names, so an extern declared in
   `lsp/lsp.plum` is looked up as `lsp.stdout_write` and never found.
   So `lsp.plum` is pure protocol logic — framing, JSON, diagnostic
   parsing — and `main.plum` owns everything that talks. Better
   separation than I would have chosen unprompted.
2. An extern's `CStr` RETURN cannot be passed back to another extern:
   the real compiler materializes it as a Plum string. So the shim OWNS
   its buffers and reuses them, rather than handing malloc'd memory to a
   caller that has no `free`. Caller-owned buffers would have leaked one
   allocation per message in a process meant to run for hours.

Deliberately not in v1, stated rather than discovered: unsaved buffers.
`didChange` does not re-check, because the checker reads from disk and
reporting disk state against an edited buffer means reporting the wrong
lines. Saving re-checks.

A third duplicate `declare` collision (after `memcmp` and `strlen`):
the runtime's stdin declares collided with the extern block's once both
existed. The pattern is now clear enough to state as a rule — a libc or
shim symbol belongs in exactly one of the runtime's declare list or a
user-level `extern` block, never both.

31/31 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 101/101 parser goldens, 11/11 `typecheck_corpus`
rejected, sweep 9/9, Rust suite 1941/0.

### Hover and go-to-definition (2026-08-19)

Both work. `./sh lsp` now advertises `hoverProvider` and
`definitionProvider`; hovering `scale` shows `let scale (p: Point) (f:
Int): Point`, and go-to-definition jumps to its declaration.
`bootstrap/lsp-smoke` asserts both.

**They are NAME-based, which is the honest headline.** The identifier
under the cursor is looked up among the project's top-level definitions
BY NAME. So a local variable that shadows a top-level name resolves to
the top-level one, and a local with no top-level namesake resolves to
nothing at all.

That was a deliberate trade, made with the numbers in hand. Precise
hover needs a position on every expression, which this compiler records
per STATEMENT: 181 match sites and 113 constructions across the parser,
interpreter, checker and backend, against roughly zero for the
name-based version. And jumping to the function or type under the cursor
is what the overwhelming majority of real go-to-definition use IS. The
precise version stays additive on top of this — nothing here has to be
undone to get it.

The mechanism is the lexer, not the AST. Token offsets already exist
(they are what gave parse errors their locations), and an identifier
token's length is its own text — so "what identifier is at line 4,
column 28" is answerable without any AST position at all. That is the
second time the spans work has paid for something it was not built for.

`defs <project>` is a new subcommand: every top-level definition as
JSON. A SUBCOMMAND rather than in-process work, for the same reason
`check` is one — the parser reports by aborting, and a file with a
syntax error is most files, most of the time, while someone is typing.
A child that dies simply yields no index. It indexes the compiler's own
source at 763 functions, 137 structs and 15 enums.

The index is rebuilt per request rather than cached. Not free, but a
project here is a handful of files, and a cache needs invalidating on
every edit — precisely the stale-state bug language servers are prone
to. Correct first; measurable later if it matters.

31/31 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 101/101 parser goldens, 11/11 `typecheck_corpus`
rejected, sweep 9/9, Rust suite 1941/0.

### `run` compiles now, and a temp-directory leak (2026-08-19)

**`./sh run` went from 2 of 9 examples to 9 of 9** by compiling and
executing instead of walking the AST.

The tree-walking interpreter had fallen seven features behind the
backend — no `Ref`, no `spawn`, no FFI, no JSON, most methods missing —
because every feature this compiler gained had to be written for the
backend and then AGAIN for the interpreter, and the second half kept not
happening. Two implementations of one language's semantics is a standing
invitation to exactly that drift, and the drift was invisible because
the corpus validates the BACKEND. Compiling means `run` and `build`
cannot disagree: they are the same path.

Output is INHERITED rather than captured (a new
`process_run_inherit` shim entry point), so a program's output appears
as it happens and it can read stdin — neither of which `run_process`,
whose whole design is temp-file capture, can offer. `sh build` also
gained `--link-lib`/`--link-c` and picks up a project's own `native/*.c`
automatically, which is what `examples/asteroids` needs.

The interpreter module is left in place: it is Stage 3 of the documented
bootstrap story and still type-checks as part of the project.

**Correction (2026-08-20): the claim that first stood here — "unused by
any command" — was false.** It was still backing `test` and single-file
`run`, which I had not checked before writing it down. See "The test
runner was running on the wrong engine" below for what that cost.

**Then the shell stopped working**, and the cause turned out to be worth
more than the feature.

`/tmp` was full — on this machine a 32G RAM-backed tmpfs, 25G consumed.
`compile_ir_to_binary` created a scratch directory per `plum build` and
per `compile-ir` and never removed it, and the compile-and-run TEST
harness did the same once per test. The suite has hundreds; a session
like this runs `cargo test` twenty times. There were **53,969 leftover
directories**, and removing them freed **21GB of RAM**.

Both now clean up, best-effort, whether or not the compile succeeded —
a failure to remove scratch must not mask a successful build. A full
`cargo test` went from leaking roughly 1,250 directories to 18 (the
remainder are error paths in a few test helpers, worth an hour someday,
not today).

Two things this cost, both worth recording. The `bootstrap_fixed_point`
test "failed" for a while and I nearly went looking for a codegen
regression — it was the disk. And this is the second time a full `/tmp`
has impersonated a real failure in this project (the first made 18 of 28
corpus fixtures report CLANG FAIL). Both times the real error was two
layers down.

31/31 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, 101/101 parser goldens, 11/11 `typecheck_corpus`
rejected, sweep 9/9, LSP smoke ok, Rust suite 1941/0.

### Self-sufficiency: the compiler builds itself, with no Rust involved (2026-08-19)

`bootstrap/self-sufficiency` proves the thing that actually gates
deprecation:

```
gen1 -> gen2 (from a temp dir) ...
gen2 -> gen3 (from /tmp) ...
SELF-SUFFICIENT: gen2 and gen3 emit byte-identical IR (169781 lines),
no Rust compiler involved
```

`bootstrap-check` never proved this. It drives every stage through
`plum compile-ir`, so the Rust compiler is still holding the tools — it
answers "is the compiler compiled by itself the same compiler", which is
a different question from "can this compiler stand alone".

**It could not, and the reason was one relative path.** `sh build` read
`native_stdlib/*.c` relative to the CURRENT DIRECTORY, so it worked
only when run from the repo root. Every harness in this project happens
to run from the root, so nothing caught it; the failure only appears
when someone runs the compiler from somewhere else, which is what an
installed compiler always does. The new check runs every stage from an
unrelated working directory ON PURPOSE.

The fix is the one the real compiler already made: EMBED the shims.
Rust gets that free through `include_str!`; Plum has no equivalent, so
`bootstrap/gen-shims` generates `shims/shims.plum` from
`native_stdlib/*.c`, and `bootstrap/check-shims` fails if the two have
drifted. A generated file with a drift check is strictly better than a
hand-copied one, and the alternative — resolving paths relative to the
binary — still breaks the moment the compiler is installed without its
source tree.

`net_shim.c` is deliberately not embedded: the self-hosted runtime
declares nothing from it, and 166 unused lines in every compiler binary
buys nothing.

What this means concretely: the Rust compiler is now needed for exactly
one thing, compiling the FIRST self-hosted binary from source. Everything
after that — building the compiler, running it, testing it, checking
it, serving an editor — the self-hosted compiler does alone.

31/31 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, self-sufficiency check passing, 101/101 parser goldens,
11/11 `typecheck_corpus` rejected, sweep 9/9, LSP smoke ok, Rust suite
1941/0.

### The bootstrap seed (2026-08-19)

`bootstrap/seed/plum.ll` is the self-hosted compiler as LLVM IR, checked
in. A fresh clone now gets a compiler with `clang` alone:

```
./bootstrap/from-seed                          # no Rust toolchain
./sh.seed build bootstrap/self_host -o sh.real
```

**IR rather than a binary**, deliberately: text is readable, diffable
and reviewable in a pull request the way a committed `.so` is not, and
it builds anywhere clang runs. The cost is 6.1MB (0.9MB gzipped) of
generated text, which is why it is refreshed rarely.

**The real reason it exists is not convenience.** Depending on the Rust
compiler to build the self-hosted one confines the self-hosted
compiler's own source to the INTERSECTION of the two languages —
`main.plum` has to compile under both. That was a recurring tax, and
this session paid it four times: a `mkdir` primitive, three stdin
primitives, the placement of an `extern` block, and a `CStr` return's
lifetime were each designed twice. With a seed, the compiler's source
can use the compiler's own features and the seed is refreshed when it
can no longer keep up.

`check-seed` proves three things, the third being the one that matters:
clang can build the seed; the seed compiler can build today's source;
and what it produces emits IR IDENTICAL to the current compiler's. A
seed that built but produced a different compiler would silently
bootstrap something other than this source tree. When it fails, the fix
is `gen-seed` — expected occasionally, and a deliberate commit rather
than a reflex.

**The Rust compiler is now demoted, not deleted**, and the distinction
is the point. It is no longer required to build anything. It stays for
two jobs it is uniquely good at:

1. It is the ORACLE `example-sweep` compares self-hosted output against,
   byte for byte. That comparison caught the `Bool`-width FFI bug, the
   dropped match guards, and both nested-pattern miscompilations — three
   silent wrong answers that no self-consistency check could have found,
   because a compiler that is wrong in the same way twice still agrees
   with itself. Crucially the sweep compares USER programs, so this
   survives even after the compiler's own source diverges past what Rust
   can build.
2. It is a from-source path for anyone unwilling to trust a checked-in
   artifact — the standing mitigation for a trusting-trust seed.

Deleting it is the one step here that is not easily undone, and there is
no rush: it costs nothing to keep and is still earning its place every
time the sweep runs.

31/31 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, self-sufficiency passing, seed bootstrapping to an
identical compiler, 101/101 parser goldens, 11/11 `typecheck_corpus`
rejected, sweep 9/9, LSP smoke ok, Rust suite 1941/0.

### Memory: 4 GB to 51 MB (2026-08-19)

Compiling the compiler took **4,005 MB** peak RSS, against the Rust
compiler's **192 MB** on identical input — 21× worse, and the reason the
`sh` guard wrapper exists at all (its own comment records a 44.9GB OOM
that killed a terminal). At that size a modest laptop cannot build a
large Plum project.

**Measured before fixing**, and the split was decisive:

| phase | peak RSS |
|---|---|
| `emit-llvm` (no clang) | 3,580 MB |
| clang on the emitted IR | 54 MB |
| `check` (parse + typecheck) | 284 MB |

So codegen owned it. The runtime's own counters (`PLUM_RT_STATS`, which
only a self-hosted-BUILT binary has) named the cause:

```
alloc n=14,039,362  bytes=3,541,197,435  concat=8,644,862
```

3.5GB allocated to produce 6MB of IR, and peak RSS almost exactly equal
to total bytes allocated — nothing was being reclaimed, because the
accumulator chain held every intermediate alive.

**The cause was quadratic string building.** The emitter accumulated its
entire output by repeated `.concat`: appending function N copies the N−1
already appended. `cg_emit_insts` (every function body in the program),
the three generated-function emitters, and `cg_lines` all did this. I
had even written "`Array.fold` over `concat` is quadratic" in a comment
in the generated shims file, without connecting it to the emitter doing
the same thing for six megabytes.

The fix is a runtime primitive, `String.concat_all`, which sums the
lengths, allocates ONCE and memcpys each piece — then the five
accumulators collect into an array and join at the end.

| | before | after |
|---|---|---|
| peak RSS, `emit-llvm` | 3,580 MB | **51 MB** |
| peak RSS, full `build` | 4,005 MB | **51 MB** |
| bytes allocated | 3.54 GB | 0.95 GB |
| wall time, `emit-llvm` | 1.33s | 0.78s |
| system time | 0.75s | 0.02s |

Peak fell 70× while bytes allocated fell only 73%, and the gap is the
point: the remaining allocations are transient and get reclaimed, where
before the growing accumulator pinned everything. The system-time
collapse is the page-faulting for 3.5GB disappearing.

The compiler now uses a QUARTER of the Rust compiler's memory, and
builds itself under the guard wrapper's default 1GB cap with no
`SH_MEM` override — the overrides scattered through the harnesses are
now upper bounds rather than requirements.

**Two things this exercised for the first time.** `String.concat_all`
had to be added to the REAL compiler's prelude too (as ordinary,
quadratic Plum) because the self-hosted compiler's own source must
compile under both — the intersection tax the seed exists to lift,
charged one more time. And `check-seed` failed exactly as designed: the
seed predated the new function, said so, and `gen-seed` fixed it. That
is the refresh workflow working on its first real occasion rather than
in theory.

31/31 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, self-sufficiency passing, seed refreshed and verified,
101/101 parser goldens, 11/11 `typecheck_corpus` rejected, sweep 9/9,
LSP smoke ok, Rust suite 1941/0.

### The README stops asking for a Rust toolchain (2026-08-20)

A newcomer was still told "Plum is implemented in Rust as a Cargo
workspace" and to run `cargo build`. That has not been the truth since
the seed landed. The getting-started path is now two lines and needs
only clang:

```sh
./bootstrap/from-seed -o plum          # clang only, no Rust
./plum build bootstrap/self_host -o plum
```

Verified verbatim, from a clean shell, before it was written down.

Three other claims were stale, and correcting them meant being explicit
about which compiler a reader is running:

- **Editor support** is the one place the Rust implementation is still
  ahead, and the README now says so in a table rather than implying
  parity: live diagnostics, expression-precise hover, go-to-definition
  for locals and fields, and completion — against the self-hosted
  server's diagnostics-on-save and NAME-based hover/definition. A reader
  who cares most about editor support is told plainly to build the Rust
  one and point their editor at it.
- **"Interpreter vs. native codegen"** describes the Rust
  implementation. The self-hosted compiler has no interpreter: its `run`
  compiles and executes, so `run` and `build` cannot disagree. The
  section now says which compiler it is about, and why the other one
  dropped its interpreter.
- **Status** now leads with self-hosting, and points at
  `example-sweep` as the honest answer to "what still differs" — rather
  than a list kept by hand, which this project has already learned not
  to trust.

The point of the "Two compilers" table is that a reader should never
have to guess which one they are running. The Rust implementation is
labelled what it now is: oracle and reference, not the compiler.

31/31 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, self-sufficiency passing, seed verified, 101/101 parser
goldens, 11/11 `typecheck_corpus` rejected, sweep 9/9, LSP smoke ok,
Rust suite 1941/0.

### Expression-level spans, scoped to what a cursor can land on (2026-08-20)

Hover now reports the type the CHECKER inferred:

```
doubled: Point      # a local
p: Point            # a parameter
```

which a name-based lookup can never do — it finds top-level definitions
and nothing else, so a local resolves to whatever top-level name it
happens to share, or to nothing.

**The version I twice declined to build was the wrong shape.** "A
position on every expression" is 185 construction sites and 181 match
sites across four modules, and I priced the feature at that twice. But a
cursor can only land on an IDENTIFIER or a FIELD NAME — every other
expression is made of those plus punctuation. Positions on `EIdent` and
`EField` alone cost **33 sites**, an order of magnitude less, and answer
the same questions.

That is the whole trick, and it was available the entire time. The
lesson is not about spans: pricing a feature by its maximal
implementation and then declining it twice is how a cheap version stays
unbuilt.

The checker records each identifier occurrence with its resolved type
into a table (`NodeType`), gated behind `record_types` so `check`,
`build` and `emit-llvm` pay nothing for a table nothing reads. A `query`
subcommand runs the check and answers a position — a subcommand for the
same reason `check` and `defs` are, since the checker reports by
aborting and a query runs against the file someone is editing.

Two honest limits, both stated in the README rather than discovered:

- It answers on identifier USES. A `let` binding's own name is a
  PATTERN, not an expression, so hovering the `doubled` in `let doubled
  = ...` finds nothing. Recording pattern bindings is the natural next
  piece and needs positions on `PIdent`.
- Go-to-definition is still name-based. The checker records what a name
  RESOLVES TO, not where it was BOUND; a local's binding site is not in
  the table.

Latency is a per-hover project re-check: 26ms on a small project, ~0.9s
on the compiler's own 14k lines. Acceptable, and honest about it —
caching would need invalidating on every edit, which is the stale-state
bug language servers are prone to.

31/31 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, self-sufficiency passing, seed verified, 101/101 parser
goldens, 11/11 `typecheck_corpus` rejected, sweep 9/9, LSP smoke ok
(now asserting a local's inferred type), Rust suite 1941/0.

### Binding sites: go-to-definition for locals (2026-08-20)

The two limits recorded with the previous commit are closed. Hovering a
`let` binding's own name works, and go-to-definition on a local jumps to
where it was bound rather than to whatever top-level name it shares.

Priced the same way as before, and it came out the same way: `PIdent`
appears at **16 sites**. The environment's bindings gained a
`def_offset` — where the name was bound — so resolving an identifier now
answers both what type it has and where it came from. No path is stored
alongside it: a local's binding is always in the same file as its use,
because a function body does not span files.

A binding records ITSELF as its own definition, which is what makes
hover work on `let n = ..` and not only on the uses of `n`.

**Currying turned a missed call site into a closure again** — the third
time this session. Two `tyenv_extend` calls kept the old three-argument
form and became partial applications, surfacing as "match arms must
produce the same type: expected Function([Int], TyEnv), found TyEnv".
Caught immediately, one step removed from the cause, and by now a
recognisable signature: when a threading change produces a type error
mentioning `Function(...)` where a value was expected, look for a call
that did not get the new argument.

What is still name-based: field names and enum variants. Hovering `.x`
in `p.x` answers for `p`, because `EField` carries a position but the
checker does not yet record the FIELD as its own resolvable entity. That
is the same shape of work again — record it where the field type is
already known, in `infer_field`.

31/31 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, self-sufficiency passing, seed verified, 101/101 parser
goldens, 11/11 `typecheck_corpus` rejected, sweep 9/9, LSP smoke ok
(now asserting a local's binding site), Rust suite 1941/0.

### The test runner was running on the wrong engine (2026-08-20)

`sh test` was broken for every realistic test, and had been since it was
built.

```
$ sh test <project>
error: unbound function: assert_eq
```

It ran each test through the tree-walking INTERPRETER, which never loads
the prelude — so `assert_eq`, which essentially every test calls, did
not exist. A test using `Ref` or the stdlib failed the same way. The
real compiler passed all three of the same tests.

**How it survived: I validated it against the wrong tests.** When `test`
was built, its fixtures used `panic_raw` directly — chosen to exercise
the process-isolation design, which they did — and never called an
assertion. The feature was proven to isolate failures correctly while
being unable to run a normal test at all.

**And I had written the opposite down.** A previous entry claimed the
interpreter was "unused by any command", which I asserted without
checking; it was still backing `test` and single-file `run`. The
correction is inline above. Both mistakes have the same shape — stating
something convenient without running it — which is exactly what
`example-sweep` exists to prevent, and neither `test` nor `run <file>`
was covered by any sweep.

**The fix** is the one `run` already had: compile, don't interpret.
`test` now compiles the project ONCE against a synthesized dispatcher
`main` that reads the test name from `args()`, then runs each test as a
child of that binary. Each test still gets its own process — `panic_raw`
aborts rather than returning, so a single-process harness stops at the
first failure — but the child is an argument to a compiled binary rather
than an interpreter invocation. Compiling once rather than per test
matters: per-test compilation would be N full builds.

The dispatcher is synthesized as SOURCE and parsed. Two hundred
characters of Plum is easier to read, and to be sure is correct, than
the equivalent tree of `EIf`/`ECall` nodes.

Two supporting fixes fell out. The parent was type-checking the user's
items WITHOUT the prelude — the exact trap `emit-llvm` already carries a
comment about — so it rejected `assert_eq` before compilation could
accept it; that check is gone, since the compile checks the real
program. And the self-hosted prelude had no assertions at all: `assert`,
`assert_eq` and `assert_ne` are now mirrored from the real prelude,
message for message, so a failing test reads identically whichever
compiler ran it.

`bootstrap/test-smoke` guards it, and deliberately uses what a smoke
test is tempted to skip: prelude assertions, a `Ref`, a stdlib call, and
a failing test that must not stop the ones after it. It runs both
compilers and requires them to agree.

**The interpreter is now genuinely unreachable** — verified by grep this
time, not assumed; the only remaining mentions are in comments. Dead-code
elimination drops it, and the compiler's own emitted IR fell from
172,068 lines to 156,464 (−9%) as a result. Whether to delete the 1,079
lines is still a separate decision: it is Stage 3 of the bootstrap
narrative.

31/31 `exec_corpus` correct and leak-free, self-build fixed point
byte-identical, self-sufficiency passing, seed verified, 101/101 parser
goldens, 11/11 `typecheck_corpus` rejected, sweep 9/9, LSP smoke ok,
test smoke ok (both compilers), Rust suite 1941/0.
