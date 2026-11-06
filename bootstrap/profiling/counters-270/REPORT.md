# Counter synchronization (#270)

Eight workers each perform 10,000 iterations of all counter-producing helpers.
Each iteration requests six cells / 159 bytes, including two string constructors,
one concatenation, and both exact-fit and capacity-based array constructors.
Each parallel round must produce exactly 480,000 allocations / 12,720,000 bytes,
160,000 strings, 80,000 concatenations, and 160,000 arrays after joining workers.
The serial control passes on both versions. On main a parallel round reported
131,433 allocations / 3,420,005 bytes, 54,648 strings, 54,187 concatenations and
98,020 arrays. With the fix, all three joined rounds match exactly. The fixture
also passes with ASan/LSan enabled.

## Design

Use monotonic atomic increments and atomic reads for all shared counters.
[LLVM's atomic guidance](https://llvm.org/docs/Atomics.html#monotonic) identifies
this ordering as appropriate for counters that do not synchronize other memory.
The counters are aligned to eight bytes. The existing native HTTP test reader
also uses an atomic load.

Per-thread aggregation could reduce global contention, but needs registration,
thread retirement and a snapshot protocol, including native-created workers.
That is a broader runtime design than synchronizing the existing five globals.
The live/peak snapshot design in #437 must provide its own coherent event ledger;
independent atomic reads here are race-free but are not one transactional snapshot.

## Actual bootstrap-path cost

Run with `bootstrap/compiler-profile.py`, three measured rounds after warmup,
serially alternating before/after, with fresh per-invocation caches. Both compilers
are default debug builds. Workload emission and the full compiler build request
release output. The after compiler was built from the refreshed seed, matching
CI's seed-to-source path. Metadata records input/compiler/seed hashes and build
provenance. This measures the delivered change, including Clang compiling the
new atomic runtime. It is not a cross-platform timing gate.

| Workload | Before median | After median | Change |
| --- | ---: | ---: | ---: |
| Check compiler | 1.586 s | 1.670 s | +5.3% |
| Emit release compiler | 12.911 s | 13.482 s | +4.4% |
| Build release compiler, including Clang | 32.350 s | 38.089 s | +17.7% |

Compiler-only peak RSS rises from 44.3 to 44.7 MiB for checking and 110.3 to
111.8 MiB for emission. Full-build peak RSS, which includes Clang, rises from
389.4 to 461.4 MiB (+18.5%). Clang compile/link time accounts for most of the
larger build increase (19.412 s before / 24.447 s after in measured round 1).
Atomic updates can prevent optimizations that were legal for the old ordinary
counter accesses; do not treat synchronization as free. These measurements do
not isolate each LLVM optimization or predict contended multi-thread throughput.

## Larger memory story

This fixes measurement correctness; it does not reduce retained heap or allocation
churn. Cumulative requested allocation bytes change by less than 0.02% across
these workloads (release emission remains about 4.48 GB). The five counters
retain their sizes and meanings; no per-allocation metadata is added. Perceus
reuse continues to avoid new allocations and therefore new counter increments.
Counters remain collected unconditionally, so the atomic cost is also paid when
`PLUM_RT_STATS` is unset; that variable still controls only the exit dump.

Keep cumulative traffic, live requested bytes, peak live requested bytes, and
OS RSS separate. #437 adds the missing opt-in lifetime accounting and sanitizer
workflow. A cumulative total or a process-exit live total alone is not a leak
verdict. The seed refresh ensures that the compiler's own runtime gets the fix,
not only the user programs it emits.
