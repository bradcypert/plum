# Collect literal and argument emission fragments

This follows #467's handle-lookup optimization and continues the cold-codegen
work in #63. A fresh helper-level allocation probe on main at
f8e3950d116946252a66f3d1dcd69ea1bc0c015d attributes 660,883,084 requested bytes
to array-literal text assembly, excluding child emission costs. That is now the
largest individual allocator in the release-emission profile. The temporary
instrumented compiler emits byte-identical IR to the ordinary compiler; its
timings are excluded from the benchmark.

## Design and alternatives

The old loops append every element's text to the accumulated prefix. As a
literal widens, later appends repeatedly copy earlier output. Reused array
literals go through a separate argument collector with the same pattern.

The collectors now keep ordered fragments for instructions, entry allocas,
releases and globals. Each section is joined once; empty sections keep the
empty constant and one-fragment sections return their owned fragment. Argument
operands and reused-array stores use the same collection approach, and comma
lists use the existing Array.join. Arrays and tuples share the ordered store
walk through their existing layout-offset functions.

The result interfaces remain Emit/FieldsEmit/ArgVals with String sections.
Register numbering, evaluation order, allocation/store order, cleanup ordering
and tail-call replay remain intact.

| Approach | Benefit | Cost / decision |
| --- | --- | --- |
| Collect fragments at the existing batch loops | Removes prefix copies in fresh/reused literals, tuples and argument batches; retains current emitter interfaces | Temporary arrays and fragment references until joining; chosen for the measured hotspot |
| Grow uniquely owned String capacity geometrically | Could improve append loops throughout programs | Changes runtime capacity/retention policy, including storage shared with CStr; shared prefixes still require copying, so this needs separate runtime and peak-memory evaluation |
| Introduce a native text builder | Could append into spare capacity | Adds native buffer ownership and accounting responsibilities to codegen |
| Change every Emit section to a rope/fragment tree | Could remove prefix copies across more composition sites | Changes many emitters, branch merges, debug attachment and tail-cleanup rewriting; a larger redesign requiring its own evidence and review |
| Memoize resource-safety analysis first | Targets another substantial profile cost | Requires type keys and context/lifecycle rules; does not remove literal-prefix copying |

## Ownership and memory

The fragment arrays are local, uniquely owned accumulators. Self-rebinding
Array.push uses the existing Perceus reuse path. Each fragment stays alive while
the collector owns it; joining creates the final section, and normal reference
counting releases the temporary arrays and strings. No process-wide cache,
native text storage or interior String view is introduced.

The temporary fragment headers, pointer arrays and final joined strings can
coexist. This trades repeated copying for bounded per-batch storage, so
cumulative requested bytes and peak RSS must be measured separately. Runtime
cell layout, native resources, NativeBuffer accounting and registry overhead
are unaffected. Perceus moves, resets and release decisions are unchanged.

## Regression coverage

The runtime fixture checks nested block locals, closure-generated text, UTF-8
and embedded NUL literals, left-to-right side effects, tuples, aliased array
replacement and later unique reuse. Tracked sanitizer checks run it in debug
and release. A sanitizer-instrumented, tracked compiler also emits the wide
fresh/reused fixtures and the nested fixture without ASan/LSan findings.

bootstrap/emission-memory-check.py generates 128-, 256- and 512-element dynamic
String arrays in both modes. It inspects the fixture function's IR to verify
the fresh allocation or reuse path was actually exercised. A first let binding
can use the reuse path with a null old cell; the fresh fixture therefore returns
its literal directly. The budget permits fixed prelude work plus per-element
cost and rejects the merged compiler at the larger widths.

At width 512, the merged compiler requests about 122–127 MB for the fresh case
and 157–161 MB for the reused case. The changed compiler requests about
44–48 MB and 46–50 MB respectively. The complete compiler still performs other
analyses; this guard bounds traffic for these inputs rather than proving the
entire compilation pipeline asymptotically linear.

The whole-compiler byte budgets tighten to 3.5 GB for debug emission and 3.3 GB
for release, with existing RSS ceilings retained. The regenerated seed is
verified through seed-to-source bootstrap so the initial compiler benefits too.

## Measurements

Medians use decimal GB of cumulative requested bytes and MiB of peak RSS.
[Raw samples, binary hashes and input hashes](compiler-emission-fragments-measurements.json)
record all three measured rounds. All debug and release output hashes match
across both compilers and every round.

| Whole-compiler workload | Allocated GB before → after | Allocations before → after | Wall seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Debug emit | 3.680 → 3.016 | 48,766,287 → 48,720,538 | 12.110 → 12.117 | 126.7 → 126.5 |
| Release emit | 3.512 → 2.848 | 45,472,661 → 45,427,631 | 11.840 → 11.827 | 112.4 → 112.5 |
| Check | 0.318 → 0.318 | 9,814,960 → 9,814,960 | 1.756 → 1.675 | 47.5 → 47.5 |

Debug allocation traffic falls 18.1%, release traffic 18.9%, saving about 664 MB
per invocation. Allocation counts change only about 0.1%: the improvement removes
large prefix copies while adding small fragment arrays. Timing and RSS are
essentially unchanged. Release times range from 11.826–11.924 s before and
11.813–11.864 s after; debug times from 12.109–12.143 s before and
12.001–12.175 s after. Checking's timing ranges overlap too. These results do
not establish a meaningful speedup or a reduction in the retained-memory peak.

Repeated resource/type analysis and other text-composition sites remain in the
profile. This change removes the measured batch-prefix copies while preserving
their current analysis, ownership and cleanup behavior.

## Reproduction

```sh
python3 bootstrap/emission-memory-check.py /path/to/current-source-compiler
python3 bootstrap/compiler-profile.py --before /path/to/before --after /path/to/after \
  --samples 3 --case check-compiler --case emit-compiler --case emit-compiler-debug \
  --out /tmp/plum-fragments-profile
```

The benchmark uses ordinary seed-to-source compilers on the same current input
tree, one warmup and three serial interleaved measured samples with alternating
order. No registry is enabled in timing runs. Requested bytes are cumulative
traffic, not live memory; peak RSS is reported separately. Timing is
observational and is not a CI threshold.
