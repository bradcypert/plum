# Shared Unicode trimming: compiler allocation measurements

The first profile-guided optimization after #465 fixes #263 as well as a
compiler hotspot. Directional trimming previously split strings into character
arrays; trim_end also reversed twice. Debug annotation calls trim_end on IR
lines that usually have no trailing whitespace. Origin and emission-span probes
pointed to these temporary strings and arrays in debug assembly.

All three trims now use the existing runtime Unicode whitespace predicate and
scan only the requested boundaries. An unchanged result retains the original
cell; a changed result copies only the retained bytes. Decoder scratch lives in
the entry block, keeping stack use constant for long whitespace runs.

## Before and after

Baseline: main at e90a0d58dcc21935f956a1c422a4e8d5b0dce71e. Both ordinary
compilers were built through the seed-to-source path and process the same current
source tree. The after compiler uses the refreshed seed, so its own prelude and
runtime contain the fix. Merely compiling a corrected user program would not
establish that the compiler itself benefits.

Linux x86_64, three serial interleaved samples after one warmup per combination,
alternating before/after order. The table reports medians; GB means decimal
requested bytes accumulated over the invocation, and MiB is peak RSS from wait4.
The [raw samples, binary hashes and input hashes](compiler-trim-measurements.json)
include phase times and timing ranges. No registry is enabled for these runs.

| Whole-compiler workload | Allocated GB before → after | Allocations before → after | Wall seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Debug emit | 7.009 → 4.952 | 126,640,891 → 78,456,787 | 18.514 → 15.867 | 126.8 → 126.4 |
| Release emit | 4.785 → 4.784 | 75,175,680 → 75,164,421 | 15.648 → 15.587 | 112.3 → 112.4 |
| Check | 0.319 → 0.319 | 9,852,689 → 9,847,205 | 2.004 → 2.041 | 47.9 → 47.5 |

Debug emission uses 29.4% fewer requested bytes and 38.0% fewer allocations;
median wall time falls 14.3%. Timing varies across rounds (debug before
15.270–18.579 s, after 13.200–15.902 s), so the deterministic allocation
reduction is the stronger evidence. Release and check changes are negligible.
The emitted IR changes because the runtime and prelude implementations change;
debug-info and compiler unit checks cover annotation behavior independently.

The focused trim_boundaries fixture applies all three no-op trims 1,000 times
to a dynamic 4,096-byte string, then trims a padded copy. It drops from
16,437,259 allocations / 839,469,887 bytes to 259 / 536,887 bytes. Remaining
allocations construct the input and padded result. Its count and 600,000-byte
ceiling reject a return to materializing characters. The compiler debug byte
budget is tightened from 8 GB to 6 GB, rejecting the old implementation while
leaving about 1 GB for source growth. RSS ceilings stay unchanged.

Reproduce the compiler comparison with preserved before/after executables:

```sh
python3 bootstrap/compiler-profile.py --before /path/to/before --after /path/to/after \
  --samples 3 --case check-compiler --case emit-compiler --case emit-compiler-debug \
  --out /tmp/plum-trim-profile
```

## Place in the memory plan

This removes allocation churn in debug code generation. It does not materially
lower the compiler's live high-water mark or RSS, and the release-emission
allocation hotspot remains. The next profile-guided target is expression
emission and its replay; the diagnostics can distinguish those costs from
retention rather than treating allocation traffic as a leak.

The [handle-lookup follow-up](compiler-handle-lookup.md) profiles those emission
helpers and removes repeated name splitting while preserving replay and cleanup.

Perceus ownership, cell layout and native ABI are unchanged. Shared no-op results
retain existing capacity until the final owner drops and can prevent in-place
updates while another alias or CStr remains. Changed trims own independent cells
and cannot retain a large source through an interior view. These are ordinary
reference-counted lifetimes, visible to Memory.snapshot/by_origin; native buffer
accounting and registry overhead are unaffected. Unicode, embedded NULs,
temporary owners, CStr aliases and long boundaries have executable coverage,
including tracked ASan/LSan in debug and release modes.

The related-issue audit also found #271 already implemented by #414. Current
public stdin ownership tests pass; its stale tracker and native-buffer roadmap
were reconciled separately from this optimization.
