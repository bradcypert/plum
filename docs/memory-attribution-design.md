# Allocation origins across Perceus reuse (#461)

## Decision and alternatives

Report the physical allocation's original construction family. This is a
provenance profile, not a census of current source-language types or source
locations. Keep Perceus ownership, reuse decisions and ordinary cell layout.

| Model | Question answered | Tradeoff |
| --- | --- | --- |
| Original construction family (chosen) | Which allocation families produced traffic and still-live storage? | Bounded metadata/cardinality; origin survives reuse; cannot identify a particular source line or the current occupant |
| Original allocation site/call stack | Which expressions/call chains caused allocation? | More precise causes, but provenance must travel through shared runtime constructors; typed expressions do not currently retain every source span; stacks interact with tail calls and optimization |
| Current storage category | What occupies storage now? | Requires events for construction, reset storage, every reuse path and native construction; a category must describe shared storage rather than alias types |
| Both provenance and current category | Both history and present occupancy | Two independent axes, more event/counter traffic and a complete state-transition contract; do not present one as the other |

Provenance is independently useful for allocation churn and retained storage.
Current-category or exact-site profiling would be separate additions with their
own semantics. A one-time allocation tag is never labeled as a current type.
String/Bytes share storage layout, and CStr shares the backing cell; grouping
them as byte storage avoids pretending there is one source-language alias type.

Perceus sections 2.4–2.5 distinguish fresh constructor allocation from writing
into a reuse token. The profiler follows that distinction: successful reuse is
not an allocation event. Reference:
https://www.microsoft.com/en-us/research/wp-content/uploads/2020/11/perceus-tr-v4.pdf

## Accounting contract

The closed origin inventory is unspecified/native, byte storage, array storage,
record, variant, closure, ref and handle. Records include tuples; variants
include Option/Result; handles include task/channel endpoint cells and declared
native handles. The native resource behind a handle remains excluded.

Every fresh generated cell allocation supplies its construction family through
one compiler emission helper. Runtime string/array constructors do the same.
The existing native producer entry point remains compatible and uses unspecified
origin. Diagnostic hooks are compiled only with --memory-stats; ordinary cell
allocations retain their existing entry point and no origin metadata.

The address registry stores requested size and original origin. In-place reset,
cross-type reuse, retained aliases and logical capacity changes do not move a
cell between origins or change its original requested byte charge. A replacement
allocation receives its own origin/request; overlap counts toward peaks. Final
storage release subtracts from the stored origin, irrespective of current value.
Reset tokens remain charged as live storage until reused or freed. Immortal
constants are excluded. NativeBuffer payload accounting stays separate.

## Interface and coherent capture

Keep Memory.snapshot/delta unchanged. Add Memory.by_origin(), returning None
without tracking or an OriginSnapshot with total: Snapshot and rows of
{ origin: Origin, stats: Snapshot }. Return all eight origins, including zero
rows, so the inventory and ordering do not depend on a workload.

Freeze totals and all origin rows together under the existing registry mutex.
Read the native frozen fields after unlocking, before invoking any callbacks.
Public records/arrays/Some allocate ordinary tracked Plum cells after capture;
immortal constructor constants (including hoisted origin labels) are excluded.
retaining them affects later samples. Native frozen storage is thread-local.

The sums of row allocations, allocated_bytes, live_allocations and live_bytes
equal the total in the same capture, including active workers. Row peaks are
independent lifetime maxima: their sum need not equal the process peak. Separate
calls to snapshot/by_origin observe separate times. Profile reads never reset
traffic or peaks.

Metadata is native and excluded from payload counts. On supported 64-bit
targets, a registry slot grows from 16 to 24 bytes. There are eight fixed native
counter rows and fixed thread-local frozen profile storage. Preserve encoded
address keys so profiling cannot conceal unreachable cells from LSan. Never
hold the registry mutex across destruction, Plum allocation or callbacks; fail
closed on metadata/counter errors.

## Delivery and verification

Implement the full fixed-family profile in one PR: compiler/runtime allocation
coverage, native ledger and coherent capture, public interface, documentation,
generated seed and regression checks. No prototype-only production path.

Cover all origins, cross-type reuse/reset, native fallback, aliases, replacement
growth, final release, bounded registry churn, frozen and active-worker captures,
row/total conservation, non-additive peaks and ordinary builds. Verify combined
ASan/LSan still detects a leaked tracked cell. Build through the refreshed seed;
measure ordinary, previous aggregate diagnostics and origin diagnostics using
source-built compilers, reporting time/RSS and metadata/observer costs explicitly.

## Measured diagnostic cost

Linux x86_64, three serial interleaved samples of source-built debug compilers
emitting this compiler's source. The aggregate baseline is merged #464; origins
and ordinary use this change. All commands emit ordinary user IR, discard stdout
and enable the same legacy traffic report. Time is wall time; RSS uses Linux
wait4 per-process maxima. These are workload observations, not portable limits.

| Compiler instrumentation | Median seconds | Median peak RSS (MiB) |
| --- | ---: | ---: |
| Ordinary | 15.233 | 126.48 |
| Previous aggregate registry | 17.740 | 184.77 |
| Registry with origins | 18.091 | 200.77 |

Origins add about 2.0% time and 16 MiB (8.7%) peak RSS over the aggregate
baseline in this workload. Compared with ordinary compilation, full diagnostics
cost about 18.8% time and 58.7% RSS. Registry slot metadata rises 50% (16→24 bytes),
which is not a 50% increase in whole-process RSS. Instrumentation can perturb
allocator layout/capacity reuse; allocation counts/traffic need not be identical
across instrumented and ordinary runs. The baseline also predates the emission
helper changes; the comparison is end-to-end, not an isolated instruction cost.

At startup, one retained public origin snapshot allocated 20 managed cells and
824 requested bytes after the captured point. This is observer traffic, separate
from native registry/TLS metadata. These exact observer numbers describe this
compiler/layout, not an interface guarantee. Retaining snapshots can therefore
dominate an otherwise empty program's later live-byte sample.

The [raw samples and executable hashes](memory-attribution-measurements.json)
record commands, platform, allocation traffic, and all repetitions. To reproduce,
build ordinary and --memory-stats source compilers from the current source and
the baseline worktree, then run each compiler serially three times on identical
source with `PLUM_RT_STATS=1`, measuring process time/RSS. Do not mix sanitizer
builds or concurrent compiler workloads into this comparison.
