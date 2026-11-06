# Compiler workload profiling

`bootstrap/compiler-profile.py` compares two compiler binaries on identical
inputs from the current checkout. It runs whole-compiler checking, release IR
emission, property-suite checking, a complete release compiler build including
Clang, and a small executable build whose output is checked against its golden.
A compiler produced by the full build must also successfully check collections.

This is a Linux measurement tool requiring Python 3.11+, not a timing gate. Samples run serially,
alternating before/after order between rounds. Sample zero warms each workload
and is excluded from summary medians. Ordinary binaries provide wall/CPU time,
peak RSS and Plum's cumulative requested allocation counters. Each invocation
uses its own cache directory, so these are fresh-cache builds on warm filesystem
pages. A GNU timeout caps individual invocations at ten minutes when available.
Build RSS includes Clang; check/emit RSS measures the compiler alone.

## Build the comparison

Build both compiler versions twice: the first stage embeds that version's
prelude, and the second stage compiles the compiler using it. Otherwise both
compilers can internally use the bootstrap executable's old Map representation.
For the #446 comparison the before revision is `044c0b2`, after is `230b302`.
Use separate detached worktrees and the same bootstrap compiler/toolchain.
For example, from the repository root, for each checkout:

```sh
# Run in the chosen source checkout. Point bootstrap_compiler at the same
# checked-in bootstrap executable/guard for both versions.
SH_TIMEOUT=600 "$bootstrap_compiler" build bootstrap/self_host -o /tmp/profile-stage1
/tmp/profile-stage1 build bootstrap/self_host -o /tmp/profile-normal
```

Keep distinct output paths for before/after. Compiler builds here use the
ordinary default build mode, matching the bootstrap workflow; workload emission
and executable builds explicitly request release output.

## Optional live-heap measurement

Build separate instrumented binaries from the same first stages. On Linux with
Clang and GNU-compatible linker wrapping, create an executable CC wrapper:

```sh
#!/bin/sh
exec clang "$@" -Wl,--wrap=malloc -Wl,--wrap=calloc -Wl,--wrap=realloc -Wl,--wrap=free
```

Then build in each source checkout, substituting absolute helper/wrapper paths:

```sh
PLUM_CC=/tmp/profile-cc /tmp/profile-stage1 build bootstrap/self_host \
  --link-c /absolute/path/to/bootstrap/profiling/heap.c -o /tmp/profile-heap
```

`heap.c` records requested bytes for direct linked malloc/calloc/realloc/free
calls. Its fixed registry is excluded from payload counts; allocator-internal
allocations are outside this measurement. Reallocation failure preserves the
original record. The bounded registry aborts rather than silently omitting
records if exhausted. Reported exit bytes may include intentionally retained
global compiler state; they are not a leak verdict. Peak payload bytes, RSS,
and cumulative allocation traffic answer different questions.

Instrumented runs occur after all ordinary measurements. Their timings and RSS
include instrumentation overhead and must not be used as performance results.

```sh
python3 bootstrap/compiler-profile.py \
  --before /tmp/profile-before-normal --after /tmp/profile-after-normal \
  --before-heap /tmp/profile-before-heap --after-heap /tmp/profile-after-heap \
  --out /tmp/compiler-profile
```

The heap flags are optional. `--samples` defaults to three measured rounds;
repeat `--case NAME` to select workloads. Metadata records compiler/harness
hashes, platform, input hashes, revision and commands. Raw stdout/stderr and
per-run JSON preserve phase timings, generated IR sizes and output digests.
Results, caches and build outputs use the output directory; compiler scratch
uses its normal temporary area. Build outputs are validated before accepting a sample. Rerun with the same input checkout to compare results.

Recorded comparisons: [Map migration #446](map-446/REPORT.md) and
[atomic allocation counters #270](counters-270/REPORT.md).
