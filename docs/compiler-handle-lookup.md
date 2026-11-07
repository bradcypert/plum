# Retain parsed handle names for allocation-free lookup

The follow-up to #466 profiles the remaining compiler emission cost in #63.
An ordinary compiler's generated IR was temporarily instrumented to measure
allocation deltas around codegen helpers. Child costs were subtracted from
parents; the instrumented compiler emitted byte-identical release IR to the
ordinary compiler. Timing from that probe is not used as performance evidence.

The largest exclusive allocation cost was cg_is_handle: 885,619 calls accounted
for 1,137,849,420 requested bytes and 26,564,604 allocations. Its context lookup
split each candidate handle's qualified name to recover its short name. Recursive
early-release safety analysis repeatedly asks this question during emission and
replay. Other context callers pay the same splitting cost.

## Design

The parser already provides the short declaration name. HandleInfo now owns a
reference to it alongside the qualified identity. One private constructor builds
that metadata in both context passes. Classification and drop-hook lookup compare
the stored names, preserving matching order, short-name aliases and qualified
identities. Neither query allocates.

| Approach | Benefit | Cost / reason for choice |
| --- | --- | --- |
| Retain the parsed short name | Removes splitting in both queries; reuses immutable declaration data | One extra reference per handle; chosen because the data already exists at construction |
| Build qualified/short-name indexes | Also avoids linear scanning | More retained metadata and construction rules; the profile does not establish a need to replace the small handle table |
| Memoize resource-safety answers | Could avoid repeated recursive analysis | Requires concrete-type keys, context scoping and lifecycle rules; broader than the measured name-splitting problem |

## Memory and Perceus

This removes temporary String and Array traffic throughout compiler handle
queries. It does not change Perceus moves, resets, early-release decisions,
tail-call cleanup or conditional emission replay. Before and after ordinary
compilers emit byte-identical release IR for the same complete compiler source.
Tests cover short/qualified names, duplicate short names and drop hooks, missing
names, empty contexts, and handle references resolved across the two context
passes. They also run under ASan/LSan with memory tracking.

Each live HandleInfo gains one eight-byte field. It shares the parsed String
cell rather than copying its bytes, and can keep that cell alive through the
context's lifetime. Both temporary contexts can coexist during construction;
normal reference counting releases their records and names. There is no global
memoization table or persistent cache. Native resource representation, payload
accounting and diagnostic registry behavior are unchanged.

The compiler's requested-byte budgets now cap debug emission at 4.5 GB and
release emission at 4 GB. The merged #466 compiler fails both budgets on the
current source, while the source-built replacement passes. RSS budgets remain
unchanged. The seed is refreshed so the initial bootstrap compiler also benefits.

## Measurements

Baseline is main at a424f838741c06e25a3d8f91bd1492c9c1fb46ce, after #466.
Medians below use decimal GB of cumulative requested bytes and MiB of peak RSS.
[Raw samples, binary hashes and input hashes](compiler-handle-lookup-measurements.json)
record all three measured rounds. Every debug and release emission in the
comparison has the same output hash across both compilers and all rounds.

| Whole-compiler workload | Allocated GB before → after | Allocations before → after | Wall seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Debug emit | 4.952 → 3.682 | 78,475,893 → 48,828,939 | 13.104 → 12.251 | 126.7 → 126.8 |
| Release emit | 4.783 → 3.514 | 75,180,795 → 45,534,745 | 12.883 → 12.049 | 112.7 → 112.4 |
| Check | 0.320 → 0.318 | 9,859,127 → 9,818,254 | 1.663 → 1.682 | 47.7 → 47.7 |

Release allocation traffic falls 26.5%, debug traffic 25.7%; both remove about
29.6 million allocations per invocation. Median emission time falls about 6.5%
in each mode. Peak RSS and checking behavior are essentially unchanged. Release
times range from 12.856–12.907 s before and 11.999–12.242 s after; debug times
range from 13.063–13.143 s before and 12.245–12.263 s after. These observations
support a churn reduction, without establishing a lower retained-memory peak.

The remaining profile includes repeated recursive type/resource analysis and
about 661 MB attributed to array-literal text assembly. Those are separate
follow-ups; this change keeps the existing analysis and emission algorithms.

The [emission-fragment follow-up](compiler-emission-fragments.md) removes literal
and argument prefix copying while preserving Perceus analysis and emitted IR.

## Reproduction

The benchmark uses ordinary compilers built through seed-to-source, with the
same current input tree, one warmup and three serial interleaved measured
samples per combination. No memory registry is enabled. Requested bytes measure
cumulative allocation traffic; peak RSS includes allocator behavior and native
memory and is reported separately. Timing is observational, not a CI threshold.

```sh
python3 bootstrap/compiler-profile.py --before /path/to/before --after /path/to/after \
  --samples 3 --case check-compiler --case emit-compiler --case emit-compiler-debug \
  --out /tmp/plum-handle-profile
```
