# Incremental compilation design proposal

This is the architecture gate following issue 63's artifact cache and cold
compiler optimizations. It does not claim that current modules compile
independently. Current `run` reuse is whole-program IR plus validated native
objects; linking and package verification remain fresh.

## First deliverable

Keep whole-program type checking initially. It validates every body,
including unreachable functions, and is now relatively inexpensive after
removing repeated source decoding. Partition the validated typed program
into stable emission units, cache those units and objects, and link fresh.
Do not add serialized incremental inference in the same change.

This saves emission/backend work after local edits while keeping today's
diagnostic and inference semantics. It must be described as incremental
emission, not incremental type checking. Cache statistics should show each
unit's identity and why it was rebuilt.

## Units and ownership

- An application entry unit owns `main`, the global initialization
  dispatcher and entry-specific support. It calls each global initializer
  in exactly the order established by project collection, including
  initializers whose results are otherwise unused.
- A runtime unit owns runtime globals, tracing/thread-local state and
  generic runtime operations. Its key includes the compiler ABI, mode and
  target. A process must contain exactly one instance of this state.
- A module unit owns its ordinary functions, globals and per-global
  initializer functions. Internal literals use function/global identities
  and local ordinals, not counters inherited from other units.
- Generic instantiations belong to the defining module. Its key includes
  the ordered, deduplicated set of requested concrete instantiations and
  the definitions/layouts used to emit them. Adding an instantiation can
  rebuild that module even if its own source did not change.
- Generated equality, release, formatting and channel-copy helpers belong
  to a separate helper unit initially. This is conservative and keeps one
  definition per helper. Splitting that unit further is a measured follow-up.

Defining-module ownership avoids introducing cross-platform ODR/COMDAT
coalescing as a prerequisite. Consumer-owned duplicated specializations and
one object per specialization are alternatives, not the first implementation.
Measure both object/link overhead and invalidation breadth before revisiting.

## Stable identity and dependency contract

Define a versioned internal symbol encoding for fully qualified names,
type arguments and literal identities. It must be unambiguous for names
that differ only in punctuation/module separators. Freeze the encoding
before unit caching and use the ABI version in every unit key.

Normalize inference variables and substitutions when computing typed-body
identities. Current transient `ITVar` numbering is not a stable interface.
Persist resolved types and canonical binders, not the allocation order of
the inference engine. Include source locations when debug metadata needs
them; content-only identity cannot claim identical debug output after moves.

A validated emission-unit description includes:

- Ordered typed bodies, global initializers and concrete specializations.
- Imported function calling conventions, parameter/return representations
  and extern declarations.
- Every nominal field/payload layout, enum tag, ownership/release contract,
  handle property and constructor definition the unit uses.
- Generic definition bodies used for specialization, even when public
  signatures did not change.
- Compile-time embedded content and its source identity.
- Mode, target, compiler/ABI identity and relevant debug file/location data.

Explicit dependency edges come from the same validated typed tree consumed
by emission. Do not infer them by regex, module names in strings or only
the `use` list. Function values, closures, callbacks, global references,
patterns, field access, spawn, channels and synthesized nodes all count.

Until inference is split, imported bodies are validated by the full checker
each time. Unit cache validation only needs to establish that the resolved
emission description and its dependencies are identical. Reuse the artifact
store's atomic publication, integrity checks, native validation and fresh
link policy.

## Cycles, reachability and initialization

Compute strongly connected components of the semantic dependency graph.
Validate cycles as a group; object emission may still produce module units
if declarations/layouts are available first. Keep one project-wide
reachability computation so newly needed generic instances and helpers are
discovered before unit selection. Globals are roots even if their values
are not referenced.

Changing an initializer's body rebuilds its owner. Changing global membership
or initialization order also rebuilds the entry unit. Do not let object order,
linker sections or native constructor ordering define Plum initialization.

## Debugging and code quality

Each IR unit must contain self-consistent debug metadata. Metadata node IDs
are local to the unit; source files, line tables and symbol locations remain
correct. Produce macOS `.dSYM` from the current run's materialized objects,
before scratch cleanup. Keep the existing stack-trace state in the runtime
unit rather than duplicating it in each module.

LLVM optimization currently sees the whole emitted program. Splitting units
can reduce inlining and other optimization opportunities. Compare runtime
performance, binary size and tail-call behavior before making this the
default. Evaluate optional LTO separately: it may recover optimization but
move most saved time back into the link. Functional equivalence alone does
not establish acceptable code quality.

## Acceptance gates

1. A clean partitioned build matches the existing full compiler's observable
   behavior across execution/rejection corpora, examples, properties,
   allocation checks, ASan/leak checks, modes, traces and debug locations.
2. Each edit scenario produces equivalent clean and incremental results:
   body-only change, signature/layout change, generic body/instance change,
   embed edit, global addition/order change, dependency changes and cycles.
3. Statistics prove which units rebuilt. A local ordinary function edit
   should retain unrelated module/runtime objects. Changes that affect
   consumers must invalidate them even if an exported name/signature is
   unchanged. Never promise exactly one rebuilt module for every edit.
4. Verify Linux x86_64/arm64, macOS and Windows link/debug behavior in their
   actual CI jobs. Cross-linking alone does not validate runtime behavior.
5. Measure cold build, unchanged run, edit-to-readiness, link time, peak RSS,
   executable size and representative runtime throughput. Do not ship a
   faster build loop that silently loses constant-stack tail calls or causes
   a material runtime regression.
6. Prove byte-identical self-hosting fixed points and a working fresh-clone
   seed path after the internal ABI/symbol changes.

Only after this stage is correct and useful should a separate proposal add
serialized module interfaces and cached type checking. That proposal needs
an explicit contract for global inference dependencies, normalized schemes,
SCC invalidation, compiler schema migration and error equivalence.
