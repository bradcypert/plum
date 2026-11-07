# Cache completed resource/type queries within a compiler program

This follows [literal/argument fragment collection](compiler-emission-fragments.md)
and continues the cold-codegen allocation work in #63. A helper-level probe on
main at 34aca827f277635274d92fe9bbff3e70c07605f5 attributes 440,028,480 requested
bytes and 14,280,238 allocations exclusively to `cg_early_release_variants`,
called 1,466,983 times. `cg_arm_scoped` separately requests 131,962,592 bytes.
Repeated declaration lookup, instantiation and traversal now dominate these
queries. The temporary probe preserves emitted IR; its timings are excluded
from the benchmark.

## Correctness before caching

The checker previously applied its nominal recursion guard before inspecting a
type's arguments. Seeing `Array` twice could therefore hide the resource in
`Array[Array[Resource]]`. The same gap affected function-containing aggregates;
the function walk also omitted tuples. Generic enum nesting had the same bug.

The walkers now inspect finite argument syntax before guarding expansion of
declaration fields or variant payloads. Resources remain opaque: nested handle
equality and rendering are rejected. Function references still compare by
identity; the function-equality check deliberately does not inspect a `Ref`'s
value. Pure recursive types remain supported.

Destructor discovery also follows `ITRef` to its payload. A declaration with an
unconstructed `Option[Ref[Payload]]` variant still needs the payload destructor
referenced by its generated release helper. Main fails to link that case with
an undefined `plum_rel_Payload`; the corrected compiler builds and runs it.

These fixes are isolated in commit d16fd31. The performance baseline includes
them, so caching is compared with the same corrected traversal semantics and
destructor discovery, rather than benefiting from differences in emitted code.

## Design and alternatives

`CgProgram` owns a private `Ref[Map[ITy, CgTypeFacts]]`. A root program gets a
fresh map after its declaration context is complete; derived function and
global contexts share it. The declaration context remains immutable throughout
that lifetime. Callers retain their existing concretization steps.

Each structural type key maps to two independent facts: whether early release
is safe and whether the type contains a handle. A miss computes both using the
existing walkers, starting with empty recursion guards, then stores the
completed result. Intermediate answers under a path-dependent `seen` set are
never cached. Function types remain capture-opaque: they are unsafe for early
release but do not prove the presence of a handle. Primitive types bypass the
map entirely.

Full `ITy` keys retain type arguments and qualified names. Rendered names and
LLVM symbol spellings are unsuitable keys: symbol sanitization currently has
collisions tracked by #320. The cache distinguishes `a.b.Thing` from
`a_b.Thing` independently of that unresolved symbol issue.

| Approach | Benefit | Cost / decision |
| --- | --- | --- |
| Cache completed root queries per program | Avoids repeated lookup and instantiation while retaining existing predicates and context lifetime | Retains queried composite types and two booleans until the program is released; chosen |
| Fill each fact independently on demand | Avoids computing an unused second fact | More cache states and a possible second lookup/update; the paired result is small and measured allocation savings justify the simpler completed entry |
| Precompute declaration properties through a dependency graph/fixed point | Could remove more repeated instantiation across related types | Requires a larger design for generic dependencies, recursive declarations, conservative closure/resource rules and diagnostic witnesses; separate design work before replacing the visitors |
| Use a global or mangled-name cache | Easy lookup across compiler operations | Incorrect context/invalidation and collision behavior; rejected |
| Merge all type visitors into one predicate | Reduces duplicated traversal code | Equality, rendering, early release and identity-based references have different semantics; sharing an answer would erase those distinctions |

## Ownership and memory

This is compiler metadata owned by one program, not a process-wide registry.
The map retains descriptive type trees and facts, never program resource values
or native payloads. Function specializations share the same map reference.
Ordinary Perceus reference counting releases the map, its keys and values when
the program and derived contexts are released. The cache contains no back
reference to its owning program.

Storage grows with distinct composite root types queried in that program,
including their type syntax. Structural hashing/equality still walks keys, and
computing both facts on a miss can do unused work. This trades retained metadata
for substantially fewer temporary declaration arrays and instantiated types;
it does not establish a constant-space analysis or eliminate all traversal.
Cumulative requested bytes and peak RSS are measured separately below.

Perceus move/reset/reuse rules, runtime cell layout, native buffers and diagnostic
registry accounting are unchanged. The corrected nested-resource classification
does affect cleanup: resources inside nested containers keep scoped finalization
instead of being missed. The escaping-owner regression verifies that cleanup
does not destroy a resource still owned by the returned aggregate.

## Regression coverage

Five negative fixtures reject nested array/enum handle equality, nested handle
rendering, nested closure-array equality and tuple closure equality. Two runtime
fixtures check nested resource scope exit, escaping owners, recursive pure
equality, function-reference identity and unconstructed reference payloads.
Both runtime fixtures run under tracked ASan/LSan in debug and release modes.
An ASan/LSan-instrumented, tracked compiler also emits the entire compiler
without findings; its output matches the ordinary executable byte for byte.

The compiler unit compares cached facts with uncached walkers over repeated
nested types, tuples, references and recursive enums. It checks concretized
generic specializations, separate programs with identical type spelling and
qualified-name collisions. The compiler-unit discovery floor increases to 41.

Whole-compiler requested-byte limits tighten to 2.9 GB for debug emission and
2.7 GB for release, retaining the existing platform RSS ceilings. The seed is
regenerated and verified through the seed-to-source bootstrap path.

## Measurements

Medians use decimal GB of cumulative requested bytes and MiB of peak RSS.
[Raw samples, binary hashes and input hashes](compiler-resource-type-facts-measurements.json)
record all three measured rounds. Debug and release output hashes match across
both compilers in every round. The before compiler includes the correctness
fixes in d16fd31; these figures isolate the cache, not the entire change from
main.

| Whole-compiler workload | Allocated GB before → after | Allocations before → after | Wall seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Debug emit | 3.048 → 2.381 | 49,821,818 → 29,005,788 | 15.723 → 8.040 | 126.7 → 127.1 |
| Release emit | 2.879 → 2.212 | 46,510,366 → 25,700,779 | 15.416 → 7.874 | 113.2 → 113.4 |
| Check | 0.319 → 0.319 | 9,846,136 → 9,846,136 | 2.110 → 2.289 | 48.4 → 48.1 |

Release allocation traffic falls 23.2%, debug traffic 21.9%, saving about
667 MB per invocation. Allocation counts fall 44.7% and 41.8% respectively.
Release timing ranges are 15.401–15.896 s before and 7.808–8.012 s after;
debug ranges are 15.697–16.496 s before and 8.038–8.240 s after. Both emission
medians improve about 49%. Checking does not use this cache, has identical
allocation counts/bytes, and has overlapping timing ranges (2.021–2.471 s before,
2.019–2.653 s after).

Peak RSS is essentially unchanged and slightly higher in the emission medians.
The result improves allocation churn and observed emission time, not retained
memory at the compilation peak. Sanitizer lifetime checks complement RSS;
neither RSS nor cumulative requested bytes measures the cache's exact live size.
Sanitization, reachability and other text construction remain allocation costs
outside this cache. Declaration-graph analysis remains an alternative if a new
profile justifies that larger redesign.

## Reproduction

Build the uncached baseline from correctness commit d16fd31 using its existing
seed (`bootstrap/from-seed`, then that executable's `build bootstrap/self_host`).
Build the changed compiler through the refreshed seed in this branch. Run both
ordinary executables against the same changed source tree:

```sh
python3 bootstrap/compiler-profile.py --before /path/to/corrected-uncached \
  --after /path/to/changed-compiler --samples 3 \
  --case check-compiler --case emit-compiler --case emit-compiler-debug \
  --out /tmp/plum-resource-profile
```

The harness uses one warmup and three serial interleaved measured samples with
alternating order, isolated caches and no diagnostic address registry. Requested
bytes are cumulative allocation traffic; RSS measures the process peak. Timing
is observational and is not a CI threshold.
