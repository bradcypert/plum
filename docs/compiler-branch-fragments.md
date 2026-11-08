# Join branch and match text once per section

This continues #63 after [batched callgraph worklists](compiler-callgraph-worklists.md).
On merged main `413f2de6fd5b7d8e25c7b03ee7922f4ec40c486d`, a fresh
release-emission probe attributes 171,044,951 requested bytes exclusively to
`cg_if`, 148,234,735 to `cg_match_arms`, and 97,927,958 to `cg_match`.
These exclude instrumented child helpers. The probe's emitted LLVM is
byte-identical to ordinary emission; instrumented timings are excluded.

## Design and alternatives

The branch emitters assemble an immutable String by successively appending
labels, child code, stores, jumps and cleanup. Every append copies the growing
prefix, including child bodies already emitted. Match traversal also copies
all preceding arms into each new accumulated section.

Use the existing `cg_join_text` operation at both levels:

```text
if/match template: ordered fragments -> one section String
match arms:       owned arrays of arm sections -> join at traversal end
Emit:             same code/allocas/releases/globals Strings as before
```

`cg_if` and `cg_match` join their instruction fragments once. Their larger
alloca/release/global combinations also join once. `cg_match_arms` joins each
arm's instruction template, then appends its code, alloca, release and global
sections to independently owned arrays. Only the final traversal step flattens
those arrays into `ArmsEmit`. Each array has one append per selected branch;
ordinary Perceus ownership permits geometric array growth without copying the
whole prefix per arm. No new general builder or runtime abstraction is needed.

The traversal still emits pattern, guard and body in the same order, computes
exactly the same register/label numbers, and keeps scoped cleanup, false-guard
cleanup renaming, pending tail cleanup and reuse-token placement unchanged.
The external `Emit` and `ArmsEmit` interfaces remain unchanged; only the private
arm traversal's accumulation parameters become fragment arrays.

| Alternative | Assessment |
| --- | --- |
| Join existing section fragments once | Removes both measured prefix-copy patterns with the existing text interface; selected |
| Interpolate shorter chunks but keep a growing match String | Improves individual templates but still repeatedly copies all preceding arms |
| Reuse a mutable String or introduce a native buffer | Requires uniqueness/capacity rules and a new text representation; does not follow automatically from Perceus, which cannot make aliased immutable concatenation free |
| Carry ropes/fragments through every `Emit` | Could avoid copying child text at enclosing expression levels too; affects all emitters, cleanup-text rewriting, debug annotations and retention; requires a separate design and comparison |
| Change liveness continuation storage first | A separate measured cost (`lv_cat`/`lv_rest` about 154 MB); changes analysis inputs and traversals instead of the largest remaining text-copy cost |

Related ownership issues #281, #349, #442 and #452 are already closed. This
change does not reopen their ownership rules or alter user collection behavior.
It is compiler-cost work under #63, not a new Perceus ownership algorithm.

## Memory and Perceus

Fragment arrays own references to immutable section Strings. Each match holds
its completed arm fragments until the final join, at which point the output
String briefly coexists with the input Strings and arrays. They are released
through ordinary Perceus reference counting on return; there is no persistent
cache, native buffer, global builder or diagnostic registry change.

The old growing String is replaced with bounded template arrays and geometrically
grown arm-section arrays. Allocation traffic is the primary target. Keeping
fragments alive until their join has a distinct peak-memory cost; less cumulative
allocation alone does not establish a smaller live heap or RSS.

Child emitters still return flat Strings. Deeply nested expressions can copy
child text once at each enclosing expression, and other passes retain their
existing costs. This change removes template-prefix and arm-list-prefix copies;
it does not establish linear complexity for the whole compiler or arbitrary
nesting depth. A full fragment/rope representation remains a separate design.

Generated user-program LLVM is compared byte for byte, including dup/drop,
reset/reuse, scoped resources, ABI and debug metadata. The runtime and ownership
analysis are unchanged. This optimization uses existing Perceus array reuse
inside the compiler rather than weakening reference accounting.

## Regression coverage

`bootstrap/branch-memory-check.py`, invoked by `mem-check`, emits fixtures at
widths 128, 256 and 512 in debug and release. One has 32 nested conditionals
around a wide body; the other has a wide match with alternating guarded and
unguarded arms. It checks that the requested branch bodies are emitted and
bounds compiler allocation traffic. Fixed depth amplifies template copying
without claiming complexity bounds as depth grows. The budgets reject merged
main at width 512 for both shapes and modes.

The `branch_emission` execution fixture covers both condition outcomes,
missing-else Unit results, true and false guards, fallthrough, effect order,
branch-local strings/globals, closures, Unicode/NUL contents and a tail cycle.
It runs in the sanitizer corpus and the tracked sanitizer harness in debug and
release. Existing resource/guard/reuse fixtures cover observable cleanup timing.
The largest generated fixtures are also built and run under ASan/LSan in both
modes; all 512 match arms and both conditional outcomes execute.

Whole-compiler requested-byte ceilings tighten from 2.2/2.0 GB to 1.8/1.65 GB
for debug/release. RSS ceilings remain unchanged.

## Measurements

Ordinary compilers built through their own seed-to-source paths run on identical
current inputs, with one warmup and three serial interleaved measured rounds.
[Raw samples, input/binary hashes and scaling records](compiler-branch-fragments-measurements.json)
include warmups. Medians use decimal GB of requested bytes and MiB of peak RSS.

| Whole-compiler workload | Requested GB before → after | Allocations before → after | Wall seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Release emit | 1.713 → 1.362 | 18,501,101 → 18,436,391 | 7.390 → 7.515 | 113.6 → 113.6 |
| Debug emit | 1.883 → 1.533 | 21,814,771 → 21,751,764 | 7.769 → 7.786 | 128.2 → 128.2 |
| Check | 0.321 → 0.321 | 9,914,331 → 9,914,331 | 2.616 → 2.651 | 46.9 → 46.6 |

Release emission saves 350,961,663 requested bytes (20.5%); debug saves
350,354,180 (18.6%). Allocation counts fall only 0.35%/0.29%: large repeated
copies disappear, while the fragment arrays introduce some small allocations.
Checking has identical allocation counts and a one-byte variation in requested
bytes; neither emitter is used for that workload.

Timing does not establish a speedup: release ranges are 5.571–7.456 seconds
before and 7.331–8.093 after; debug ranges are 4.934–8.568 before and
4.991–8.551 after. The release median is slightly slower. RSS is essentially
unchanged. This is a measured allocation-traffic improvement, not a demonstrated
latency or retained-memory reduction.

Every raw debug/release LLVM output hash matches across both compilers and all
rounds. All twelve scaling comparisons also have identical raw LLVM. At width
512, debug/release branch allocation falls from 74.2/71.5 MB to 50.5/47.8 MB;
match allocation falls from 109.7/106.4 MB to 48.0/44.7 MB. Remaining costs in
other passes still grow with these inputs.

Validation passes: 392 ASan/LSan corpus fixtures, 43 compiler tests, 50
properties, 671 formatting checks, memory/scaling/fuel guards, debug-info
checks, refreshed seed bootstrap gate, 35 stdlib doc pages and embedded shims.
The tracked sanitizer harness covers both modes. A compiler built with
ASan/LSan and memory tracking emits the entire compiler cleanly and produces
byte-identical LLVM to the ordinary changed compiler.

## Reproduction

Build merged main `413f2de` and this branch through their own checked-in
seed-to-source paths, keeping output executables outside the checkout. Run
both ordinary compilers on the same current source:

```sh
./bootstrap/mem-check /path/to/changed-compiler
python3 bootstrap/branch-memory-check.py /path/to/main-compiler # must fail its byte budgets
python3 bootstrap/compiler-profile.py --before /path/to/main-compiler \
  --after /path/to/changed-compiler --samples 3 \
  --case check-compiler --case emit-compiler --case emit-compiler-debug \
  --out /tmp/plum-branch-profile
```

The benchmark runs one warmup and three serial interleaved measured rounds,
alternating order. Allocation counters and raw IR hashes are recorded alongside
input/binary hashes. Timing measurements exclude instrumentation and other
validation builds. The scaling records use the harness's exact generated
source and compare raw IR at each width and mode.

The next allocation pass is [shared liveness continuations](compiler-liveness-continuations.md).
