# Batch pending callgraph work instead of copying each remainder

This follows [injective symbol identities](compiler-symbol-identities.md) in
the #63 compiler-allocation work. On merged main da13a511e30d30d207d6df12a8b7221c39cdb001,
a fresh release-emission helper probe attributes 211,443,312 requested bytes
exclusively to `cg_reachable` and 130,078,640 to `cg_reaches`. The instrumented
compiler emits byte-identical IR to its ordinary counterpart; its timings are
excluded from measurements.

## Design and alternatives

Both traversals formerly removed the first pending call with `Array.slice`
and appended discovered calls with `Array.concat`. A wide frontier repeatedly
copied and retained/released its remaining entries. These are two different
traversal policies with the same pending-work storage problem.

`codegen/callgraph.plum` keeps the existing entry interfaces and uses a current
batch, a read cursor and an independently owned array of newly queued calls.
`cg_pending_push` appends each newly discovered call with self-rebinding
`Array.push`, using existing Perceus reuse and geometric capacity growth. When
the current batch ends, the queued array becomes the next batch and a fresh
empty queue is started. No front-removal slice or remainder concatenation is
needed.

The invariant is:

```text
old pending = current_batch[read..] ++ newly_queued

pop:    read one current entry; increment read and fuel
expand: append discovered calls to newly_queued, in order
rotate: replace exhausted batch with newly_queued; fuel stays unchanged
```

This preserves breadth-first order, duplicate/runtime handling, initial global
roots, concrete generic arguments and existing diagnostics. The cycle search
keeps its `fuel > 400` conservative-false guard. Specialization discovery keeps
its `fuel > 10000` diagnostic and checks for an empty queue before fuel. Fuel
counts popped occurrences, including duplicates; the initial root starts at
zero. A different queue must not silently alter which cycles permit frame
elision or which programs exceed the existing limit.

| Approach | Benefit | Cost / decision |
| --- | --- | --- |
| Two FIFO batches and a cursor | Avoids pending-array copies while preserving order and fuel exactly | Retains a consumed prefix until its batch ends; chosen |
| Append-only array and a cursor | Simple FIFO indexing and amortized appends | Retains every consumed queue entry until traversal ends; clearing slots adds mutation/optional-entry or compaction policy |
| Two stacks | Standard amortized queue | Needs reverse/pop handling or another cursor; batching expresses existing insertion order directly |
| Cache adjacency and compute strongly connected components | Could avoid repeated body walks and cycle searches | Requires a larger design for concrete generic instances, termination limits, deferred closure calls and cycle/frame semantics; separate design discussion before adopting it |
| Deduplicate on enqueue | Reduces pending duplicates | Alters fuel accounting and potentially bounded cycle decisions; outside this storage change |

The private batch state uses owned function parameters, keeping the append
receiver unique without a mutable global queue or a new ownership abstraction.
The two policies share appending while retaining their separate visited-state,
failure and return behavior. Function-body call collection is unchanged.

## Memory and Perceus

Worklist arrays hold `CgInst` descriptions: module/name strings and type syntax,
not actual user resources. The current batch remains live until rotation, even
for entries already read. The next batch is accumulated separately. A batch is
released on rotation or return through ordinary reference counting; there is
no persistent queue cache. Other state such as the completed specialization
list and cycle-search visited names retains its existing lifetime.

Pending storage follows the current/next frontier sizes instead of the total
history of popped entries. This does not make the entire compiler linear or
constant-space: repeated body/lookup/visited scans and other text/metadata work
remain. Immutable type descriptions can also share subtrees across entries.
Requested-byte traffic and peak RSS are measured separately.

Existing Perceus move/reset/reuse and resource-finalization rules remain intact.
The optimization relies on uniquely owned array appends rather than a native
buffer or runtime representation change. Generated user-program ABI, runtime
cell layout, native payload ownership and diagnostic registry accounting are
unchanged. Batched metadata retention is a tradeoff to inspect at the peak;
fewer requested bytes alone do not prove lower live memory.

Whole-compiler byte ceilings tighten to 2.2 GB for debug and 2.0 GB for release
from 2.5/2.3 GB. Existing Linux RSS limits (144/128/64 MiB for debug/release/check)
and macOS limits (192/192/80 MiB) remain unchanged.

## Regression coverage

The compiler unit pins FIFO discovery through global roots, shared callees,
duplicate calls, native calls and distinct generic specializations. It checks
the final permitted/empty-queue discovery steps, mutual recursion, conservative
negative results, the exact cycle-fuel boundary, and duplicate/runtime fuel
consumption. The compiler-unit discovery floor rises to 43.

`bootstrap/reachability-memory-check.py` emits shared-callee graphs at widths
512, 1024 and 2048 in debug and release. It checks exact user-function discovery
order and uniqueness, then enforces requested-byte budgets. At width 2048,
main requests about 638 MB debug / 513 MB release; batching requests about
454 MB / 329 MB. Both implementations emit identical IR at every measured
width. The budgets reject main at the largest width. Remaining frontend and
emitter costs still grow on these inputs; this is a regression bound over the
selected range, not a proof of whole-pipeline linearity.

The same harness emits 10,000 repeated calls in an array and accepts the final
permitted occurrence, then checks that 10,001 calls retain the existing
monomorphization diagnostic. Array elements avoid a deeply nested block AST.
Both main and the changed compiler satisfy those boundary assertions.

The full 391-fixture sanitizer corpus, 43 compiler units, 50 properties,
670 formatting checks, seed gate, memory/debug-info checks and docs/shim checks
pass. A tracked ASan/LSan compiler also emits the entire compiler without
findings; its IR matches ordinary emission byte for byte.

## Measurements

Medians use decimal GB of requested bytes and MiB of peak RSS.
[Raw samples and input/binary hashes](compiler-callgraph-worklists-measurements.json)
include three measured rounds and the wide-graph comparison. Debug and release
output hashes match across both compilers and all rounds on the same current
source, including the new traversal and its unit.

| Whole-compiler workload | Allocated GB before → after | Allocations before → after | Wall seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Debug emit | 2.223 → 1.886 | 22,140,007 → 21,820,264 | 7.134 → 6.225 | 128.4 → 128.3 |
| Release emit | 2.052 → 1.716 | 18,824,709 → 18,504,289 | 7.454 → 6.826 | 114.0 → 113.9 |
| Check | 0.321 → 0.321 | 9,920,535 → 9,920,535 | 2.471 → 2.501 | 47.1 → 46.9 |

Release traffic falls 16.4% (336 MB), debug traffic 15.2% (337 MB). Allocation
counts fall only 1.7% and 1.4%: the gain removes large copies of pending arrays,
rather than millions of tiny allocations. Checking uses neither traversal and
has identical requested bytes and allocation counts.

Timing is noisy: release ranges are 6.374–7.846 s before and 5.927–6.867 s after;
debug ranges are 5.473–7.864 s before and 4.981–7.623 s after. Checking ranges
overlap too. The emission medians improve, but these samples do not establish
a reliable speedup. Peak RSS is essentially unchanged. This is an allocation
traffic improvement, not a demonstrated retained-memory reduction. Repeated
visited/lookup/body scans and branch/text composition remain separate costs.

## Reproduction

Build main da13a511e30d30d207d6df12a8b7221c39cdb001 and this branch through their
own checked-in seed-to-source paths. Run ordinary executables on identical
current source:

```sh
python3 bootstrap/reachability-memory-check.py /path/to/changed-compiler
python3 bootstrap/compiler-profile.py --before /path/to/main-compiler \
  --after /path/to/changed-compiler --samples 3 \
  --case check-compiler --case emit-compiler --case emit-compiler-debug \
  --out /tmp/plum-reachability-profile
```

The benchmark uses one warmup and three serial interleaved measured rounds with
alternating order, isolated caches and no diagnostic address registry. Requested
bytes measure cumulative traffic; RSS measures process peak. Instrumented runs
are excluded from timing, which is observational rather than a CI threshold.
