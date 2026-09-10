# Plum

Plum is a general-purpose ML-style language (OCaml/F# family: type
inference, algebraic data types, pattern matching, `Result`-based
errors) that does not need a garbage collector to get there.

That makes it usable in the places a collector rules out, which is where
the design pressure came from: code that has to sit directly on a C ABI
boundary without translation friction, work with hard latency
requirements, and eventually constrained hardware. But the target is
ordinary software first. See "Design decisions" below for what that
ordering does and does not commit to.

## The gap

Memory-management philosophy today is a triangle with an empty middle:

- **GC languages** (OCaml, Go, Haskell) are ergonomic and safe, but the
  collector rules out constrained hardware and rules out hard latency
  guarantees. You trade control for ease.
- **Manually-proven languages** (Rust, and C/C++ if "manually careful"
  counts) give full control and zero overhead anywhere, but the
  programmer is the proof engine. The borrow checker doesn't manage memory
  for you; it forces you to demonstrate, by hand, in the type system, that
  your code is correct. Every program pays that tax, whether or not it
  actually needed zero-cost guarantees. Most CLI tools, firmware, and glue
  code don't; they need "good enough and predictable," not "provably
  optimal."
- **Nobody occupies the middle**: automatic, not manually proven, but
  still not a tracing GC. Roc proved the model works (reference counting +
  compiler-driven in-place mutation, no borrow checker) but targets
  application and scripting use, not hardware. Austral targets the
  hardware/systems space with an ML-ish flavor, but goes the opposite
  direction on ergonomics. It exposes linear types to the programmer, so
  you're back to manually proving things by hand, just with different
  syntax than Rust.

Roc has the memory model without the target audience. Austral has the
target audience without the automatic model. That gap is empty because
it's genuinely hard, not because it's unwanted.

## The pitch

Plum is for people who want Rust's reach (real hardware, real C interop,
no GC) without Rust's proof burden. You write code that looks like it
assumes a garbage collector exists. The compiler makes it behave like it
doesn't.

Concretely, that means: reference counting plus compiler-driven
functional-but-in-place optimization (mutate in place when uniquely
owned, copy otherwise) as the memory model, invisible in the surface
language: no linear types, no lifetime annotations, no borrow checker.
You get predictable, GC-free memory behavior as a consequence of how the
compiler treats ordinary-looking immutable code, not because you proved
anything to it.

## What Plum is not trying to be

- **Not a Rust competitor on zero-cost guarantees.** Refcounting has a
  real, nonzero cost. Plum's bet is that most systems-adjacent code would
  rather pay a small, predictable runtime cost than a large, upfront
  cognitive one. If a workload genuinely needs proven-zero overhead
  (kernels, the hottest of hot loops), Plum is the wrong tool. Reach for
  Rust or Zig.
- **Not an OCaml replacement.** OCaml's ecosystem, tooling, and GC are
  mature and excellent. If a project can afford a GC, OCaml is a safer
  choice than Plum today and will be for a long time. Plum only wins in
  the specific situations where a GC is disqualifying.
- **Not chasing every feature a "real" ML language has on day one.** No
  user-definable typeclasses in v1 (a small built-in set of `Num`, `Eq`
  and `Show` covers the common ergonomic wins). No full effect system,
  just a lightweight unsafe/extern marker that propagates from FFI call
  sites. Power features get added once the memory model and interop story
  are proven, not before.

## Litmus test

If a proposed feature only makes sense assuming a garbage collector
exists, it doesn't belong in Plum.

## Design decisions

Plum is a general-purpose ML-style language first (web APIs, CLI tools,
games), not a systems/embedded language first. The memory model is
justified by frame-time predictability and clean C FFI, not by fitting
on a microcontroller; embedded reach is a welcome side effect, not the
goal.

Most of what follows has since been built. This section records the
decisions and says where each one stands; the full reasoning behind
every one of them, including the ones that changed along the way, lives
in `DESIGN.md`. If this section ever looks inconsistent with
`DESIGN.md`, `DESIGN.md` is the one to trust, and `README.md` is the
place to look for what is actually checked rather than claimed.

- **Memory management**: reference counting + FBIP (Perceus algorithm),
  no tracing GC, no borrow checker. Uniqueness tracking is
  compiler-internal, invisible in the surface type system. Immutable by
  default, with `Ref[T]` as the explicit opt-in for genuinely
  graph-shaped shared state (game entity graphs, and the like).
  **Built.** Reference cycles are only possible through `Ref`; a
  `Weak`-by-convention story and a scoped, budget-bounded cycle
  collector for that one type remain possible later additions rather
  than commitments.
- **Concurrency**: Go-inspired tasks + channels + `select`, with channel
  send implemented as an ownership move (reusing the same static
  last-use analysis FBIP needs) so the default non-atomic refcounts stay
  race-free without a runtime cost. **Built**, on OS threads. A real
  green-thread scheduler is still a later upgrade, and an explicit
  atomic-refcounted type remains the intended escape hatch for genuine
  cross-task sharing.
- **Backend**: LLVM, targeting the C ABI directly (not compiling through
  C source, which gives no reliable tail-call guarantee; that matters
  for an ML-style language built around recursion). **Built.**
- **Error handling**: `Result`-based, explicit. No exceptions as the
  primary mechanism: a simpler runtime, no unwinding machinery to
  reconcile with refcount cleanup, and a better fit for constrained
  targets. **Built**, and there is no early `return` either.
- **Syntax**: Rust-shaped surface syntax (braces, `fn`, expression-
  oriented `match`), not OCaml/F#'s literal look, while keeping ML
  semantics (inference, ADTs, pattern matching, `Result`) underneath.
  **Built.** The pitch: "Rust with the lifetimes deleted."
- **Ad-hoc polymorphism**: a small built-in set of compiler-known
  bounds rather than user-definable typeclasses. **Built** as `[T: Ord]`,
  `[T: Eq]` and `[T: Show]`. User-definable typeclasses are still not
  offered, and are not a v1 commitment.
- **FFI priority**: calling into existing C libraries matters more early
  on than being called from C, though both are goals. `extern "C"`
  blocks with explicit C-ABI types, no implicit string or allocation
  coercion at the boundary. **Built**, and exercised by
  `examples/asteroids` against real raylib.
- **How it was bootstrapped**: the memory model and FBIP pass were
  validated on a simplified typed IR with a tree-walking interpreter
  before any of the LLVM backend was written, because the risky,
  unproven part of the design was the memory model rather than codegen.
  That worked, and the sequencing is now history.
- **Implementation language**: Plum. The first compiler was written in
  Rust; the self-hosted one replaced its code generator on 2026-08-21
  and the Rust implementation was retired on 2026-08-25. Building from
  source needs `clang` and nothing else.
- **Target platforms**: hosted Linux, macOS and Windows are primary and
  published, with cross-compilation through `plum build --target`.
  Raspberry Pi is close to free. WASM is deliberately deprioritized:
  its concurrency, FFI and standard-library stories are all undesigned,
  and nothing is pushing on it. RISC-V microcontrollers (ESP32-C3/C6/H2)
  are aspirational; the Xtensa-based original ESP32 is deprioritized, as
  it needs a non-mainline LLVM fork.
