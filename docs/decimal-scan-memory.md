# Decimal rounding without temporary digit Strings

`Float.to_fixed` keeps its existing decimal expansion, precision clamp
(0 through 17), half-even decisions, carry algorithm and signed-zero behavior.
The change removes allocation from reading that expansion. It benefits **Plum
programs using fixed decimal formatting**, including programs built by the
distributed bootstrap compiler. It is not a reduction in compiler memory use.

## Design and alternatives

The old private digit reader sliced one character and searched
`"0123456789"`, producing temporary Strings and optional lookup results.
An exact tie can repeat that work across the remaining zeroes. It also counted
Unicode characters repeatedly in strings known to contain ASCII decimals.

We compared three source implementations with the unchanged compiler before
choosing the production change. For `Float.to_fixed(1.5, 0)`, both ordinary
debug and release builds measured:

| Approach | Allocations | Cumulative requested bytes |
| --- | ---: | ---: |
| Original | 47 | 1,025 |
| Public `Bytes.at` with optional result; byte lengths | 27 | 665 |
| Guarded raw digit reader; byte lengths | 7 | 185 |
| Selected: guarded digit reader plus one scalar dot scan | 6 | 161 |

The selected implementation uses existing compiler primitives behind private
decimal helpers. It checks the digit index before the unchecked byte load,
accepts only ASCII `0` through `9`, and retains the previous zero fallback for
missing or non-digit input. A single bounded scan locates the decimal point
without allocating an `Option`; its position is passed to rounding instead of
performing both `contains` and `index_of`. The sign adjustment happens once.
The remaining decimal lengths use the existing constant-time byte length.

The fixed expansion and assembled digits are ASCII. Dot scanning starts at
zero, stops at the stored byte length, and only advances after an in-range
read. Digit scanning checks both bounds. Bytes borrows retain the normal
Perceus ownership rules. Neither helper can mutate the input or produce a
pointer that escapes. Nonfinite, dotless representations still return the raw
formatter result; the platform formatter and its representation are unchanged.

Using `Bytes.at` would remove some work but keep optional cells in this hot
loop despite the scalar fallback required by the existing algorithm. Moving
the entire formatter into C, replacing its rounding policy, or adding a new
intrinsic would broaden the change unnecessarily. Private ASCII scans remove
the temporary work while keeping rounding in Plum and reusing existing
storage and ownership interfaces.

## Memory results

The ordinary public API measurements compare the merged #498 compiler with
the refreshed-seed-to-source compiler containing this change. These are
requested **Plum cell** bytes, including their headers and terminators, not
allocator usable sizes, libc scratch, diagnostic bookkeeping, or process RSS.
The per-call traffic excludes argument setup; peak covers the whole probe
process, including setup. Debug and release figures agree on these cases.

| Value / precision | Before allocations / bytes | After allocations / bytes | Result live bytes (both) | Peak before → after |
| --- | ---: | ---: | ---: | ---: |
| 0.5 / 0 | 44 / 965 | 5 / 143 | 18 | 163 → 158 |
| 1.5 / 0 | 47 / 1,025 | 6 / 161 | 18 | 163 → 158 |
| 0.375 / 2 | 45 / 978 | 8 / 198 | 18 | 173 → 173 |
| 9.99 / 1 | 17 / 386 | 10 / 236 | 19 | 173 → 173 |
| -0.001 / 2 | 12 / 305 | 9 / 239 | 22 | 173 → 173 |
| 1/3 / 2 | 10 / 245 | 7 / 179 | 18 | 173 → 173 |
| Maximum finite / 17 | 12 / 2,150 | 9 / 2,084 | 483 | 930 → 930 |

For 1.5, 864 fewer cumulative requested bytes means less allocation/free
traffic while formatting; it does **not** mean 864 fewer bytes of resident
memory. The result is still an 18-byte allocation, and peak drops by only
5 bytes. Most other peaks above are unchanged. Slicing, carry output and
concatenation still allocate as before. This is not an allocation-free
formatter and does not establish a speed or RSS improvement.

There is no ARC/FBIP redesign here: result construction still follows the
existing Perceus retain/release and unique-buffer reuse rules. Avoiding
temporary cells reduces the work those rules must manage. Public immutable
String/Bytes/CStr aliases and cell layouts remain intact.

### Compiler cost

Two alternating samples per mode, following a warmup, compile the same final
source tree with the before and after compilers:

| Compiler emission | Before requested bytes | After requested bytes | Increase |
| --- | ---: | ---: | ---: |
| Release | 881,776,727 | 881,887,943 | 111,216 (0.0126%) |
| Debug | 1,048,959,574–1,048,959,576 | 1,049,173,070–1,049,173,071 | 213,495–213,496 (0.0204%) |

The additional helper/source has a small compilation cost. These figures are
cumulative requests during compiler emission, not the memory of the produced
program or a compiler peak/RSS measurement. This change is justified by the
runtime formatting reductions above, not by compiler savings. Raw samples,
binary hashes and input hashes are in
[decimal-scan-compiler-measurements.json](decimal-scan-compiler-measurements.json).

## Reproduction and regression coverage

```sh
python3 bootstrap/measure-decimal-scans.py /path/to/before /path/to/after \
  --compare-designs --out /tmp/decimal-scans.json
python3 bootstrap/measure-decimal-scans.py /path/to/before /path/to/after \
  --sanitize --out /tmp/decimal-scans-asan.json
```

[decimal-scan-measurements.json](decimal-scan-measurements.json) records 1,174
observations: 28 values × 10 precisions × 2 modes × 2 compilers, plus nine
focus cases in three source models and both modes. The frozen legacy model
reproduces the original text and all measured memory fields on every focus
case. The two alternatives change only the reads and decimal lengths. Models
declare existing compiler primitives explicitly for this experiment; they do
not add a public API or expose private helpers to property tests.

The harness requires unchanged before/after text, including near ties,
positive/negative finite extremes, normal/subnormal minima, signed zero,
infinities, NaNs and extreme precision arguments. Its C component supplies
binary floating-point values and observes output/storage; it does not use a
second C float formatter as a cross-platform rounding oracle. The ordinary
1,120 before/after cases also pass under AddressSanitizer/LeakSanitizer.

The public execution fixture checks exact ties, near misses, carries,
precision clamps, negative zero, and immutable String/Bytes/CStr aliases.
The public property checks 1,024 positive and 1,024 negative exact half ties.
The allocation corpus formats 1.5 and 0.375 a thousand times each, with
portable ceilings of 22,000 allocations and 600,000 requested bytes. Ordinary
Linux builds measure 14,000 / 359,000; ASan's exact usable capacities measure
18,000 / 439,000. The guard allows allocator-dependent reuse while rejecting
the old temporary-string algorithm and the optional-digit alternative.

Validation also covers the refreshed seed gate, all execution/property/self
tests, formatter, documentation, allocation and compiler-memory gates,
sanitizers, declarations, builtins, LSP, and cross compilation. The public
fixture links for Windows x64, macOS arm64 and Linux arm64; its Linux arm64
binary runs under QEMU. Native Windows/macOS execution remains a CI check.

Related issue review: [#23](https://github.com/bradcypert/plum/issues/23)
introduced fixed formatting and is completed;
[#267](https://github.com/bradcypert/plum/issues/267) is the completed
precision/rounding contract retained here.
[#33](https://github.com/bradcypert/plum/issues/33) concerns deferred formatting
syntax. No syntax/API work or issue closure is implied by this optimization.
