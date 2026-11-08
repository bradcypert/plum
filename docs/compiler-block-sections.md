# Collect block and function sections; keep statement cleanup LIFO

This continues #63 after [shared liveness continuations](compiler-liveness-continuations.md).
It has two outcomes: less temporary allocation inside the compiler, and a
cleanup-order correction in generated programs. It introduces no new FBIP
analysis or runtime representation.

## The cleanup correction comes first

The remaining emitter copies revealed a pre-existing asymmetry. Declarations
and expression statements appended their deferred cleanup after later
statements' cleanup; assignments prepended it. Interleaving these forms could
therefore release assignment temporaries before owners created later.

The `statement_cleanup_order` fixture uses closures owning native handles,
whose existing lifetime is the enclosing frame. Mixed statement groups on
merged main release IDs in order `3, 5, 6, 4, 2, 1`. Correct statement-group
LIFO order is `6, 5, 4, 3, 2, 1`, consistent with the rule recorded in
`DESIGN.md` under “Releases run LIFO (2026-09-05)”. The fixture also covers a
returned scalar, destructuring, cleanup before tail calls and an escaping
closure owner. It checks live-owner counts before exit as well as drop order.

Commit `b673e8d` corrects assignment ordering using the existing recursive
emitter. Commit `0c5492b` refreshes that baseline's seed. The isolated baseline
is built through its own seed-to-source path before the collection change.
This separates the intentional behavior change from the text-storage work.

Related #1 and #9 established scope/resource cleanup, and #349 established
ownership/reuse rules; those issues are closed. No open issue for this exact
assignment-order gap was found. This fix reorders existing deferred statement
groups. It retains their internal release order, existing resource/closure
lifetimes, and immediate release of overwritten values at assignment stores.

## Design and alternatives

A fresh profile on merged main `79d43c3bb805da3108861be569c6aaf00c0d72bc`
attributes 92,346,010 requested bytes exclusively to `cg_fn`, 30,850,402 to
`cg_stmts_at` and 15,452,137 to `cg_stmts`. The probe includes lexer/parser/
typechecker helpers too, so this target is chosen against the broader pipeline.
Its output matches ordinary LLVM byte for byte; instrumented timings are
excluded.

The old statement traversal emits one statement, recursively emits the entire
remainder, then combines their sections while unwinding. Each enclosing step
copies the growing remainder. Function assembly also appends full body text
several times while adding headers, cleanup and the return.

```text
cg_stmt            → one statement's sections, environment and next counter
cg_stmts_collect   → owned arrays of code/allocas/releases/globals
block completion  → join forward sections; reverse cleanup groups and join
cg_fn              → join instructions once; join complete function once
```

`codegen/blocks.plum` keeps the existing `cg_stmts`/`BlockEmit` interface.
`cg_stmt` emits just one statement and returns its environment/counter.
The collector emits statements left to right, preserving each debug marker
and numeric-counter step, including the historical extra assignment step.
Empty sections are skipped. At completion, code, allocas and globals join in
source order; cleanup groups join in reverse statement order. Reversing groups
does not reverse instructions inside a group.

The accumulators are owned parameters to a tail-recursive collector, using
ordinary Perceus array reuse. Its emitted self-call is `musttail`: the previous
statement result is released before the next call, allowing the environment
and accumulator arrays to be uniquely owned where possible. No explicit
mutable global builder is needed. `cg_block_ty` joins its body/tail/cleanup
pieces once, and `cg_fn` joins instruction and function sections at their
existing finalization points. Hoisted allocas, debug attachment, tail cleanup
replay and parameter cleanup ordering remain in place.

| Alternative | Assessment |
| --- | --- |
| Collect owned sections per block/function | Removes the measured remainder/prefix copies through existing interfaces; chosen |
| Change only individual concatenation templates | Leaves copying of every later statement's text |
| Mutable loop with accumulators | Requires managing the previous result's lifetime so it cannot keep the next environment shared; owned tail recursion fits existing Perceus conventions |
| Rope/fragments through every `Emit` | Could reduce copies across arbitrary nested expressions, but also affects cleanup text rewriting, debug annotation and retention; separate design |
| Stream directly to an output writer | Needs hoisted allocas, cleanup discovery/replay, debug locations and module globals settled before writing; larger pipeline redesign |

The invariant relative to the corrected baseline is identical section text,
environment, register/global numbering and cleanup placement at every block
return. Raw debug/release LLVM comparisons verify this on the whole compiler
and all generated scaling inputs. Main itself intentionally differs on the
cleanup regression; output identity is asserted against the corrected baseline.

## Memory and Perceus

This reduces copies of compiler-owned LLVM text. It does not alter generated
program allocation/reuse decisions beyond the explicit cleanup-order fix.
Existing FBIP allocation fixtures check that reuse behavior remains intact.

Each block retains its completed section Strings in arrays until the block
joins them. The reversed cleanup array temporarily shares references to the
same Strings; the join then allocates the final section. Those inputs briefly
coexist with the joined output and are released through ordinary Perceus RC.
The former recursive unwind held statement fragments and copied remainders.
Function joining similarly has a temporary overlap with body sections.

There is no persistent cache, native buffer, arena, registry or cell-layout
change. The transformed AST, symbol identities, resource type facts, liveness
rules and reset/reuse tokens retain their existing representations. Child
emitters still return flat Strings, and frontend/environment/type work can
still grow on wide inputs. This is not a proof of whole-pipeline linearity or
constant stack/memory for arbitrary source nesting. Cumulative requested bytes,
allocation counts and peak RSS are reported separately.

## Regression coverage

`statement_cleanup_order` runs in the full sanitizer corpus and in the tracked
sanitizer harness in both modes. Its original-main output differs from the
recorded LIFO result; corrected baseline and the collector match it. Escaped
owners survive, resources remain live until the existing cleanup point, and
tail calls clean their frames in the expected order.

`bootstrap/block-memory-check.py`, invoked by `mem-check`, emits expression,
declaration and assignment blocks at widths 256, 512 and 1024 in both modes.
It checks that all calls reach the emitted function and sets requested-byte
budgets that reject the corrected baseline. Every fixture uses a heap string;
a 32-statement instance of each generator also executes in the sanitizer
harness in both modes. The largest generated programs execute separately under
ASan/LSan too.

At width 1024, assignment blocks fall from about 673/615 MB debug/release to
85/67 MB. Expression and declaration blocks also avoid repeated section
copies. Other passes still contribute to those totals. Whole-compiler byte
ceilings reduce by 50 MB to 1.6 GB debug / 1.45 GB release; RSS ceilings remain
unchanged.

## Measurements

Ordinary seed-to-source compilers process identical current inputs, with one
warmup and three serial interleaved measured rounds in alternating order. The
before compiler includes only the cleanup correction and its refreshed seed.
[Raw samples, hashes and scaling records](compiler-block-sections-measurements.json)
include warmups. Requested GB are decimal; peak RSS is MiB.

| Whole-compiler workload | Requested GB before → after | Allocations before → after | Wall seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Release emit | 1.213 → 1.135 | 18,467,356 → 18,476,067 | 4.650 → 5.025 | 114.7 → 114.9 |
| Debug emit | 1.384 → 1.303 | 21,787,762 → 21,794,979 | 5.202 → 4.862 | 128.9 → 129.1 |
| Check | 0.321 → 0.321 | 9,930,665 → 9,930,665 | 1.792 → 2.062 | 47.2 → 47.2 |

Release emission saves 78,080,110 requested bytes (6.4%); debug saves 81,175,665
(5.9%). Allocation counts rise slightly, about 0.05%/0.03%, because collection
introduces small arrays and per-statement results while removing larger copies.
Checking has identical allocation bytes/counts and does not use these emitters.

Timing is mixed: the release median is slower, the debug median faster. Ranges
overlap: release is 4.643–5.783 seconds before and 4.642–5.877 after; debug is
4.898–6.008 before and 4.862–5.897 after. These samples do not establish a
speedup or a consistent latency direction. Peak RSS is essentially unchanged.
The measured memory benefit is lower cumulative compiler allocation traffic.

Every debug/release raw LLVM hash matches the corrected baseline across all
rounds. All 24 scaling pairs also match it. The native cleanup regression
rejects original main's first drop (ID 3 instead of 6) in debug and release;
corrected baseline and collection produce the exact recorded output in both
modes under ASan/LSan. The fixture's native callback checks the drop sequence
itself, so the sanitizer harness also asserts order in release mode.

Validation passes: 393 ASan/LSan corpus fixtures; 43 generated-program allocation
fixtures without regressions; 44 compiler tests; 50 properties; 674 formatting
checks; refreshed seed bootstrap gate; memory/scaling/fuel and debug-info
checks; 35 stdlib doc pages and embedded shims. The six largest generated block
programs execute under ASan/LSan. A compiler built with ASan/LSan and memory
tracking emits the whole compiler without findings, producing byte-identical
LLVM to ordinary emission. Runtime ABI, cell layout and FBIP allocation counts
remain intact; deferred assignment cleanup order changes intentionally.

## Reproduction

Build corrected baseline `0c5492b` and this branch through their checked-in
seed-to-source paths, keeping output executables outside the checkout. Run on
identical current source:

```sh
./bootstrap/mem-check /path/to/changed-compiler
python3 bootstrap/block-memory-check.py /path/to/corrected-baseline # must fail budgets
PLUM=/path/to/changed-compiler ./bootstrap/alloc-check
python3 bootstrap/compiler-profile.py --before /path/to/corrected-baseline \
  --after /path/to/changed-compiler --samples 3 \
  --case check-compiler --case emit-compiler --case emit-compiler-debug \
  --out /tmp/plum-block-profile
```

The benchmark runs one warmup and three serial interleaved measured rounds,
alternating order. Raw records retain binary/input hashes and output hashes.
