# Compiler workloads after Map #446

The Map change does not show a broad compiler throughput regression in this
local CLI comparison. Full compiler build times overlap; whole-compiler checking
is essentially flat. The small collections build costs about 0.10 seconds more.
The profile supports retaining the shipped trie and investigating generated-code
size/allocation traffic before considering further Map representation changes.

Before: `044c0b2`. After: merged main `230b302`. Both compilers were built twice
from their own sources using the same bootstrap executable and Clang 23.1.3, in
the default compiler build mode used by bootstrap. All workloads use the same
merged-main source inputs. Measurements are Linux x86_64 local results, not
portable performance guarantees or CI timing thresholds.

Three measured rounds follow a warm-up round, alternating before/after order.
Builds use separate cache directories and warm filesystem pages. Instrumented
heap runs follow all timing runs. Input hashes were rechecked afterward; check
and emission output hashes repeat within each version across ordinary and heap
binaries. All 50 invocations succeeded, including output validation for built
executables. Baseline and current IR are expected to differ.

## Time and RSS

| Workload | Before median s (range) | After median s (range) | Before RSS MiB | After RSS MiB |
|---|---:|---:|---:|---:|
| check-compiler | 1.591 (1.587–1.592) | 1.616 (1.601–1.622) | 44.3 | 44.4 |
| emit-compiler | 15.380 (15.129–15.661) | 15.288 (15.075–15.336) | 109.5 | 110.5 |
| check-properties | 0.190 (0.173–0.205) | 0.193 (0.181–0.195) | 20.5 | 20.5 |
| build-compiler | 33.753 (33.028–34.078) | 33.381 (32.844–35.042) | 390.6 | 388.2 |
| build-collections | 1.327 (1.307–1.350) | 1.423 (1.412–1.454) | 113.7 | 113.0 |

Check/emission RSS is the compiler alone. Build RSS includes Clang descendants
and is not a compiler-only memory metric. Release emission omits debug metadata;
these RSS numbers do not replace the existing default-mode memory gates. The
small property-suite check is too short to infer a trend from three samples.

## Allocation traffic and live heap

Plum counters are cumulative requested bytes, including freed allocations. Heap
peak/exit bytes come from separate malloc/calloc/realloc/free-wrapped binaries;
registry memory and allocator-internal allocations are excluded. Child Clang
heap is not tracked. Instrumented time/RSS is deliberately excluded above. Exit
bytes can include global compiler state and do not establish a leak.

| Workload | Before cumulative bytes | After cumulative bytes | Before peak payload bytes | After peak payload bytes | Before exit bytes | After exit bytes |
|---|---:|---:|---:|---:|---:|---:|
| check-compiler | 318,276,830 | 319,307,196 | 31,859,061 | 31,959,785 | 16,183 | 15,503 |
| emit-compiler | 6,589,826,935 | 6,524,276,024 | 72,183,985 | 72,538,413 | 90,952 | 72,576 |
| check-properties | 57,432,501 | 58,453,637 | 6,946,072 | 7,054,121 | 10,375 | 9,655 |
| build-compiler | 6,832,351,455 | 6,769,105,964 | 269,558,037 | 271,208,424 | 91,045 | 72,657 |
| build-collections | 33,722,014 | 45,261,790 | 3,701,874 | 3,867,531 | 12,560 | 12,123 |

The emission workload still allocates about 6.52 GB cumulatively while holding
about 72.5 MB at its wrapped live-heap peak. That is allocation turnover, not a
6.52 GB live footprint. The Map change reduces this workload's total allocation
traffic by only about 1%; the application Map construction result of 221× less
allocation must not be applied to whole-compiler allocation traffic. Peak heap
and RSS rise slightly for emission; no memory limit was changed.

A complete compiler build reaches about 271 MB of wrapped parent-process live
payload, versus 72.5 MB for emission alone. This peak excludes Clang's heap;
process-tree RSS above includes it. These maxima occur at unspecified times and
must not be added together. The build path therefore deserves its own phase-level
memory attribution; emission-only measurements do not characterize full builds.
The baseline full-build peak was about 270 MB, so this is not a new Map regression.

## Where the time goes

For current compiler emission, function emission accounts for roughly 80% of
the measured command time. This phase is the strongest next profiling target,
but phase timing alone does not identify its allocation sites or prove that
string concatenation is the cause. Add per-phase allocation attribution or a
CPU sampling profile before changing its implementation.

The small collections build emits 243,470 IR bytes before versus 363,840 after
(about 49% more). Its measured native compile/link phase rises by about
72 ms, accounting for most of the small end-to-end cost. The richer trie
helper implementation increases generated-code work; this is evidence to
investigate helper/code size, not proof of a Map-lookup CPU bottleneck.

No additional production optimization is made on this evidence. Next priority:
attribute function-emission allocation/CPU costs and test representative larger
applications before optimizing helper size. Long-lived LSP sessions and their
retained heap are not covered by this CLI experiment. NaN policy (#288) remains
a separate key-contract decision. #281 remains complete.

## Reproduce and audit

See [README.md](../README.md) for builds, measurement boundaries and invocation.
The adjacent `metadata.json` records input and binary hashes; `results.json`
contains all warm-up, measured and heap runs, including phase timings. The
streaming hash implementation avoids retaining entire IR/binary files in the
parent measurement process. An earlier buffering run was discarded. The heap
probe was checked against a controlled 60-byte peak/zero-byte-exit C program,
including successful/failed/zero-size reallocations and an untracked libc
allocation. Its source passes Clang `-Wall -Wextra -Werror`.
