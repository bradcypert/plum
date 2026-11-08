# Share liveness continuations as source ranges

This follows [branch and match fragments](compiler-branch-fragments.md) under
#63. The outcome is less temporary allocation inside the compiler. Generated
programs retain the same ARC, reset/reuse and FBIP behavior.

On merged main `2eaffbe41224a4cf8324605bbab833fa5ccf4f8a`, a fresh release
emission probe attributes 80,070,496 requested bytes exclusively to `lv_cat`
and 74,253,408 to `lv_rest`. The instrumented compiler emits byte-identical
LLVM to its ordinary counterpart; instrumentation is excluded from timings.

## Design and alternatives

Liveness carries expressions that can read a value after the current point.
`lv_seq` used to materialize every remaining expression suffix, then append it
to the enclosing continuation. Blocks and field initializers likewise copied
statement RHS expressions and field values into temporary arrays. These copies
hold immutable syntax rather than computed user values.

`codegen/liveness_future.plum` owns a private continuation representation:

```text
Empty
Exprs(source expression array, start)
Stmts(source statement array, start)
Fields(source field array, start)
Join(earlier continuation, later continuation)
```

Constructors elide empty ranges and joins. A range shares an existing source
array, with no suffix allocation. A join shares both continuations, with no
flattening. `lv_future_none_read` queries this representation in the old flat
list's order, calling the same `cg_reads_max` operation for each expression.
Statement ranges project only RHS expressions; field ranges project values.
The block helper joins its remaining statements with its optional tail.

The invariant is that flattening a continuation yields exactly the old list
of queried expressions, in order. `LvCtx.exprs` changes representation; lexical
scope, mandatory lifetime/borrow pins, future writes, replacement evidence,
loop back edges, closure captures and reset/release frontiers keep their
existing policies. Additional reads still remove replacement evidence. The
remaining frontier/arm collectors keep their flat-array interfaces where they
serve other compiler queries; this is not an ownership-analysis redesign.

Statement projection deserves care: `TSLet`/`TSAssign` wrappers are not passed
to a whole-block read query. The old collector deliberately kept only their
RHS expressions. Treating a future rebinding as killing all subsequent reads
would change the conservative rule and could change moves/reuse. Those concerns
remain in the existing separate scope/write/replacement machinery.

| Alternative | Assessment |
| --- | --- |
| Share source ranges and joins | Removes suffix materialization and enclosing-list copies, retaining existing read semantics; selected |
| Optimize only array concatenation | Still allocates every operand/statement/field suffix |
| Build persistent lists of individual expressions | Can share suffixes but requires projected list construction and another traversal strategy for each source collection |
| Add general array views to the standard library/runtime | Broadens public collection lifetime and representation contracts for a compiler-local need |
| Compute read-name sets or use binding identities/SSA | Could eliminate repeated scans and support further precision, but requires a separate design for scoping, writes, replacement, captures, branches and loops |

Related #349, #442 and #452 are already resolved. This pass preserves their
ownership/reuse behavior and continues compiler-cost work under #63.

## Memory and Perceus

Continuation ranges retain their complete source arrays, including skipped
prefixes. The source body and traversal already retain these arrays while
liveness runs. Join nodes share enclosing continuations rather than copying
expression references. They hold descriptive syntax, not the actual values or
resources in a generated program.

These immutable nodes use ordinary Perceus reference counting. They are local
to liveness and are released when their contexts return; transformed output
syntax does not store them. No persistent cache, global registry, native buffer
or runtime cell change is introduced. Context depth follows nested syntax;
walking a wide operand or statement list does not add a join for every earlier
entry. Queries can recurse through nested joins, and the original read scans
still occur. This is not a whole-pipeline complexity or stack-bound proof.

The tradeoff is small range/join nodes and references to whole source arrays
in place of many copied arrays. Peak RSS and cumulative requested bytes must
be measured separately. Generated LLVM comparisons pin the intended contract:
user-program ABI, dup/drop, reset/reuse and resource cleanup stay unchanged.

## Regression coverage

The compiler test pins excluded prefixes, empty suffixes, statement RHS-only
reads, field values, block tails, joined contexts, and the interactions with
replacement evidence, mandatory pins, writes and frame ownership. Compiler
test discovery rises from 43 to 44.

`bootstrap/liveness-memory-check.py`, invoked by `mem-check`, emits array
operands at widths 512, 1024 and 2048 in both modes. Every later operand reads
the same parameter: earlier reads must preserve it, and the last can transfer
it. The input string is allocated at runtime and passed with unique ownership.
A 64-operand instance of the same generator also executes in the sanitizer
harness in both modes, so premature transfers are observable. The guard
asserts that all calls reach the generated function and checks byte
budgets that reject merged main. At width 2048, debug/release allocation falls
from about 133/129 MB to 39/35 MB.

Additional measurements cover statement ranges at widths 256–2048 and field
ranges at widths 256–512. Their raw LLVM also matches. Existing emitter and
other pass costs dominate wide blocks and fields, so they are not used to
claim linear whole-compiler scaling or set brittle budgets. The largest array,
block and field programs execute under ASan/LSan in debug and release.

Existing generated-program allocation fixtures check that FBIP/reuse counts
and byte budgets do not regress; the full sanitizer corpus covers aliases,
shadowing, assignments, closure captures, guards, resources and loop back edges.
Whole-compiler byte ceilings reduce by 150 MB to 1.65 GB debug / 1.5 GB release,
retaining roughly 0.3 GB of growth margin. The focused suffix guard detects
this regression independently. RSS ceilings remain unchanged.

## Measurements

Ordinary seed-to-source compilers run on identical current inputs after one
warmup, with three serial interleaved measured rounds and alternating order.
[Raw samples, input/binary hashes and scaling records](compiler-liveness-continuations-measurements.json)
include warmups. Requested GB are decimal; peak RSS is MiB.

| Whole-compiler workload | Requested GB before → after | Allocations before → after | Wall seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Release emit | 1.364 → 1.212 | 18,513,907 → 18,469,038 | 6.078 → 5.705 | 114.2 → 114.1 |
| Debug emit | 1.536 → 1.383 | 21,838,155 → 21,792,775 | 6.264 → 5.960 | 128.0 → 127.8 |
| Check | 0.322 → 0.322 | 9,939,234 → 9,939,234 | 2.443 → 2.445 | 46.8 → 46.7 |

Release saves 152,065,853 requested bytes (11.1%), debug 152,492,480 (9.9%).
Allocation counts fall only about 0.24%/0.21%: small range/join nodes replace
larger arrays of repeated syntax references. Checking has identical allocation
counts and a one-byte variation in requested bytes; it does not use liveness.

Timing medians improve about 6.1% release / 4.9% debug, but the three-sample
ranges overlap: release is 5.957–6.166 seconds before and 5.683–6.631 after;
debug is 6.257–7.402 before and 5.917–6.479 after. This is observational timing
rather than an established latency improvement. Peak RSS is essentially
unchanged. The demonstrated benefit is reduced compiler allocation traffic.

Every raw debug/release LLVM output hash matches across both compilers and
all rounds. All twenty array/block/field scaling pairs also have identical raw
LLVM. The largest generated cases execute successfully under ASan/LSan in both
modes with heap-owned inputs. Wide blocks and fields retain substantial costs
in other passes; their measurements should not be read as a whole-pipeline
linearity claim.

Validation passes: 392 ASan/LSan corpus fixtures; 43 generated-program allocation
fixtures without regressions; 44 compiler tests; 50 properties; 672 formatting
checks; refreshed seed bootstrap gate; memory/scaling/fuel and debug-info
checks; 35 stdlib doc pages and embedded shims. The tracked sanitizer harness
includes the shared generator's heap-input case in both modes. A compiler built
with ASan/LSan and memory tracking emits the whole compiler without findings
and produces byte-identical LLVM to ordinary emission.

## Reproduction

Build merged main `2eaffbe` and this branch through their own checked-in
seed-to-source paths, keeping executables outside the checkout. Run both on
identical current source:

```sh
./bootstrap/mem-check /path/to/changed-compiler
python3 bootstrap/liveness-memory-check.py /path/to/main-compiler # must fail byte budgets
PLUM=/path/to/changed-compiler ./bootstrap/alloc-check
python3 bootstrap/compiler-profile.py --before /path/to/main-compiler \
  --after /path/to/changed-compiler --samples 3 \
  --case check-compiler --case emit-compiler --case emit-compiler-debug \
  --out /tmp/plum-liveness-profile
```

The benchmark runs one warmup and three serial interleaved measured rounds,
alternating order. Raw samples retain input/binary and output hashes. The
ordinary baseline executable was built and validated through #472's own
seed-to-source path; that source tree is identical to merged main `2eaffbe`.
