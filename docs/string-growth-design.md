# Proposed String growth policy for #478

Status: approved for production implementation. The comparison measurements
below came from isolated emitted-LLVM experiments before implementation.

## Recommendation

Use **1.5× growth only when a uniquely owned String needs more capacity**.
Keep ordinary concatenation and shared-receiver copies exact-sized. Preserve
the existing String/Bytes/CStr representation and Perceus ownership rules.

This fixes unknown-length append loops behind their existing interface.
The 1.5× bound favors retained/peak requested storage over the lowest possible
allocation count. A 2× factor remains a defensible throughput-oriented choice.

## Perceus

The [Perceus PLDI paper, §2.4](https://www.microsoft.com/en-us/research/uploads/prod/2021/06/perceus-pldi21.pdf)
uses compiler liveness and runtime uniqueness to reuse storage safely. Its
constructor reuse pairs same-sized allocations. It does not prescribe a
geometric String-capacity factor. Our inference is that capacity planning
belongs beneath the existing ownership check, rather than replacing Perceus
or weakening alias protection.

The compiler already selects the consuming concat-reuse entry point. Its
capacity fallback copies to an exact-sized String, making the next append
depend on allocator slack again.

## Decision flow

For the existing consuming concat-reuse operation:

~~~text
logical_length = checked(a.length + b.length)
required_bytes = checked(logical_length + 17)

if a is uniquely owned and required_bytes <= allocator_usable_size(a):
    append in place; preserve the original allocation charge
else:
    preferred_bytes = required_bytes
    if a is uniquely owned:
        old_capacity = allocator_usable_size(a) - 17
        candidate = old_capacity + floor(old_capacity / 2)
        if candidate and candidate + 17 are representable:
            preferred_bytes = max(required_bytes, candidate + 17)
    new_cell = allocate_required_preferred(required_bytes, preferred_bytes, ByteStorage)
    copy only the two logical byte ranges
    store logical_length and a NUL at the logical end
    release a after both copies
    return new_cell
~~~

The reuse entry point consumes a and borrows b; that contract remains. If b
aliases a, neither source is freed before copying is complete. Normal concat
continues to borrow both inputs, allocate an exact result and leave them
unchanged, even if a runtime count happens to be one. Compiler call selection
and ARC/reuse inference do not change.

The target is based on **old usable capacity**, not 1.5× the new required
length. A large one-off suffix gets only the required space when it dominates
the target. An empty append fits without reserve when the receiver is unique.
No extra empty-append optimization or automatic shrinking is proposed here.

An oversized buffer can later become shared. Its aliases retain the original
allocation; this policy does not retroactively shrink it. A fresh copy from
a shared receiver does not replicate that spare capacity.

## Bounds, allocation failure and accounting

Required length and header/terminator arithmetic must be checked before
allocation, pointer offsets or copying. The representable data bound is
INT64_MAX - 17; malformed negative lengths are rejected defensively.
Unrepresentable optional reserve falls back to the valid minimum request.

Introduce one private allocation module/interface:

~~~text
allocate_required_preferred(required_bytes, preferred_bytes, origin)
    -> owned non-null cell, or an allocation-free fatal diagnostic
~~~

It tries the preferred request and, if that fails and differs from the minimum,
retries the minimum. If both fail, it terminates before a null store or copy.
This is not a catchable language exception or a guarantee against operating-
system overcommit/OOM termination.

Only the actual successful cell and its chosen request count toward Plum-cell
allocation totals. A failed speculative malloc creates no live cell. Failed
native attempts are not counted as successful Plum allocations.

Amortized growth assumes reserve requests succeed. Repeated fallback under
memory pressure may return to exact-growth copying; it preserves values and
progress when the minimum fits, rather than promising linear work regardless
of available storage. Shared snapshots likewise retain their inherent copy cost.

Keep allocation-copy-release, not realloc, in this change. This preserves the
existing lifetime/provenance model and records old/new overlap in requested-
byte peaks, without defining address-preserving realloc events.

Diagnostics need a narrow internal operation to register an already allocated
pointer, actual request and origin. Factor the existing registry insertion so
ordinary allocations and the reserve allocator share checks and locking.
Do not put registry metadata in String cells. Both build modes use the same
growth policy. Only concat construction adopts the new allocator interface;
a general allocator redesign is out of scope.

An in-place append keeps the cell's original request. Replacement growth gets
a fresh ByteStorage event. Final release retires the capacity-sized original
request, not the current logical length. String, Bytes and owned CStr aliases
still prevent in-place mutation through ordinary reference counts.

## Measurements

These are **comparison experiments**, not production overflow/OOM/sanitizer
validation. They use bounded inputs and temporary LLVM substitutions, Linux
x86_64/glibc, Clang 23.1.4 and a source-built compiler matching merged main
6321c97. Production source is unchanged.

The existing opt-in memory registry supplies traffic/live/peak metrics. An
allocation-free native observer identifies the result cell's original request
and usable size. Values, logical lengths, terminators, saved snapshots and
String/Bytes/CStr aliases are checked across 91 runs. Cases include repetition,
mixed UTF-8/NUL chunks, self-concat, retained snapshots, aliases, large suffixes
and empty appends.

All bytes below are decimal requests, including headers. Traffic and peak
include small common argument/setup work. Output request is the result cell's
own charge. Allocator rounding is separate in the [raw records](string-growth-design-measurements.json).
No timing or RSS improvement is inferred.

### Stopping points matter

| Workload/policy | Allocations | Cumulative bytes | Result request | Peak requested bytes |
| --- | ---: | ---: | ---: | ---: |
| Repeat abc 4,096 times: current | 777 | 4,732,625 | 12,299 | 24,580 |
| Same: unique 1.5× | 25 | 51,137 | 17,043 | 28,398 |
| Same: unique 2× | 19 | 32,935 | 16,383 | 24,574 |
| Repeat abc 5,460 times: current | 1,033 | 8,407,249 | 16,394 | 32,773 |
| Same: unique 1.5× | 25 | 51,137 | 17,043 | 28,398 |
| Same: unique 2× | 20 | 65,702 | 32,767 | 49,150 |
| Mixed UTF-8/NUL, 8,192 appends: current | 2,057 | 33,589,455 | 32,777 | 65,538 |
| Same: unique 1.5× | 27 | 115,053 | 38,355 | 63,918 |
| Same: unique 2× | 21 | 131,235 | 65,535 | 98,302 |

Both geometric policies remove the observed quadratic traffic. A smaller
factor does not guarantee a smaller allocation at every particular length:
growth phases differ. At a growth event, 1.5× bounds explicitly requested data
capacity to less than 1.5× the new logical length; 2× permits almost twice that
length. Allocator rounding and the header are separate. Requested old/new
overlap can approach 2.5× versus 3× the new logical length. These are bounds,
not pointwise promises.

### Shared copies stay exact

Retaining every prefix inherently retains quadratic logical content; an
ownership optimization cannot erase observable snapshots.

| 256 retained prefixes | Cumulative bytes | Live requested bytes |
| --- | ---: | ---: |
| Current | 109,186 | 106,096 |
| Unique-only 1.5× or 2× | 109,186 | 106,096 |
| Reserve 2× on shared copies too | 207,874 | 204,784 |

Unique-only policies also preserved exact-sized new results and all original
String/Bytes/CStr contents in the alias test. Large one-off suffixes and empty
appends kept baseline request sizes.

## Alternatives

| Alternative | Assessment |
| --- | --- |
| Unique-only 2× reserve | Fewer growth events; larger worst-case retained/transient reserve. Choose if throughput outweighs the recommended memory bound. |
| Reserve on shared copies | Rejected: needlessly duplicates spare storage in retained histories. |
| Public builder interface | Useful for explicit capacity control, but adds an interface and finish/ownership semantics while existing loops remain problematic. |
| Capacity word in String/Bytes cells | Changes shared representation, CStr/FFI assumptions and native producers. Existing allocator-size support makes that migration unnecessary here. |
| Known-size String.repeat algorithm | Valuable independently: doubling/joins give exact final storage and linear traffic. It does not fix arbitrary append loops or close #478 alone. |
| Realloc | Potentially avoids copies, but needs separate diagnostic/provenance and failure semantics. Defer. |

The known-size repeat experiment built 49,152 output bytes with 99,260
cumulative requested bytes, 49,169 retained bytes and a 73,794-byte peak.
It remains worth an algorithmic improvement after generic growth is fixed.

## Production results

The implementation preserves the approved policy. It adds checked concat
lengths, the required/preferred allocator and a shared diagnostic registration
boundary. Native fatal reporting writes static messages without allocation
and exits immediately; overflow/OOM does not run user cleanup.

[Production records](string-growth-production-measurements.json) measure actual
programs built by unmodified compiler binaries, including the current compiler
rebuilt from the refreshed seed. Debug and release results agree:

| Workload | Traffic before → after | Allocations before → after | Retained request before → after | Peak before → after |
| --- | ---: | ---: | ---: | ---: |
| abc × 4,096 | 4,732,436 → 50,948 B | 769 → 17 | 12,299 → 17,043 B | 24,580 → 28,398 B |
| abc × 5,460 | 8,407,060 → 50,948 B | 1,025 → 17 | 16,394 → 17,043 B | 32,773 → 28,398 B |
| Mixed UTF-8/NUL, 8,192 appends | 33,589,266 → 114,864 B | 2,049 → 19 | 32,777 → 38,355 B | 65,538 → 63,918 B |

Traffic/retained deltas exclude argument setup; peaks cover the whole process.
Allocator usable sizes remain separate. Exact counts can vary slightly with
allocator placement/rounding; the raw records include a second invocation.
All output bytes and terminators are
verified. Reproduce on Linux/glibc with:

```sh
python3 bootstrap/measure-string-growth.py /path/to/main-compiler /path/to/seed-built-compiler --out /tmp/string-growth.json
```

This is a generated-program runtime improvement: growing uniquely owned
Strings now amortizes allocation/copy traffic when reserve succeeds. It trades
some retained capacity for fewer copies; the first row deliberately shows a
higher peak. It does not promise lower peak memory or RSS for every workload.
Retained shared histories still cost their observable logical contents; fresh
shared copies request exact space. No automatic shrink or new String header
is introduced.

Plum's compiler inherits that runtime after the seed refresh. On the same
current compiler sources, debug emission traffic falls from 1,202,336,242 to
1,086,906,587 B (115.43 MB less); release falls from 1,035,133,226 to 919,988,221 B
(115.15 MB less). These cumulative totals are separate from live/peak memory;
no timing or RSS reduction is inferred.

The focused guard runs actual emitted helpers at O0/O2 and public ownership
paths in debug/release, ordinary/diagnostic modes. It injects reserve failure,
minimum failure and invalid sizes; verifies old/new peak overlap, byte origin,
original-request retirement, aliases and exact shared snapshots; and covers
10,000 near-limit scalar growth cases without enormous allocations. Traffic
budgets scale with logical length over four doubling input sizes. CI runs
it on Linux/macOS with ASan/LSan and Windows without sanitizers.

The intentional diagnostic leak fixture uses a terminating worker so stale
pointers in returned allocator frames cannot appear as main-stack roots.
Both ordinary (32 B) and adopted (80 B) leaks must be reported with ordinary
LeakSanitizer stack/register scanning enabled, in debug and release.

## Production validation plan

1. Implement checked concat sizing, required/preferred allocation, diagnostic
   registration and the chosen unique-only growth rule together. Ordinary and
   shared copies remain exact.
2. Add public String tests and allocation/live-capacity guards for repeated
   appends, mixed chunk sizes, stop-after-growth, snapshots, Unicode, NUL,
   empty inputs/appends, self-concat and String/Bytes/CStr aliases.
3. Inject preferred-allocation failure and both-allocation failure at the
   allocator seam. Test required-size/optional-reserve overflow with bounded
   arithmetic tests, without enormous real allocations.
4. Validate default/diagnostic builds, ASan/LSan, origins/live peaks and
   concurrency; run the relevant complete suites and cross-platform CI.
5. Refresh native fingerprints and seed as required, rebuild seed → source,
   and remeasure actual emitted programs and the compiler. A functional seed
   comparison alone does not refresh its own embedded runtime.
6. Open the production PR with traffic, retained capacity, peak, correctness
   and failure-path evidence. Revisit the compiler's fragment renderer only
   after a separate comparison justifies switching it.

The 1.5× policy, fallback and accounting contract were approved on 2026-11-08.
Production validation is recorded in the implementation PR.
