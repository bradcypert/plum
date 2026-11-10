# Reverse String units directly into one exact buffer

`String.reverse` previously materialized `chars_of(s)`, built a reversed array,
and concatenated its per-character Strings. The new constructor scans the same
units and copies them into one exact-sized output cell, eliminating both
arrays and all individual character allocations.

The unit boundary helper is shared with the `chars_of`/empty-separator split
builder. For valid UTF-8 each unit is a codepoint. Combining marks reverse
independently of their base character; this does not reverse graphemes. For
unsafe malformed Strings a unit is the first byte plus subsequent continuation
bytes, including a leading continuation run. This deliberately preserves the
existing `chars_of` semantics instead of decoding or normalizing invalid bytes.
A second reversal need not restore malformed input, because its new ordering
can change how continuation runs group; involution properties apply only to
valid UTF-8. Raw-byte tests compare one call against the old composition.

## Candidate and design comparison

The initial allocation probe used the merged #493 source-built compiler and
one retained input: 32,768 repetitions of `aé\0🙂`, 262,144 bytes / 131,072
codepoints. Its native snapshot observer performed no allocations. Verification
ran after the observation.

| Candidate | Result allocations | Cumulative requested bytes during operation | Comment |
| --- | ---: | ---: | --- |
| String reversal | 131,090 | 6,946,545 | Output is still only 262,144 bytes |
| Bytes to Array[Int] | 17 | 6,413,888 | Final requested array storage is 3,211,216 bytes; exact array construction remains a separate follow-up |
| Binary formatting of Int::MAX | 245 | 7,101 | Much smaller per call; no input bytes |

These examples compare allocation opportunities, not equal-purpose workloads,
execution time, or overall application hotness. Reverse has a clear avoidable
intermediate representation and can improve without changing the public API
or ownership model. No current compiler source calls `String.reverse`.

Related-issue searches covered reverse, to_array, radix and String allocation:
[#278](https://github.com/bradcypert/plum/issues/278) was closed as not planned
(the recursive Array traversals are tail-lowered, so recursion alone does not
establish a stack bug). This work makes no such claim and does not change
`Array.reverse`.
[#248](https://github.com/bradcypert/plum/issues/248) is completed: character
splitting already builds directly into an owned array. Its allocation/release
behavior is preserved while sharing the boundary scanner.
[#16](https://github.com/bradcypert/plum/issues/16) established the distinct
Bytes representation; this work does not change Array or Bytes layout.

| Approach | Tradeoff | Decision |
| --- | --- | --- |
| Keep arrays and per-character Strings | Familiar source spelling, but O(codepoints) allocations and two arrays | Replaced |
| Prepend source slices to a growing String | Avoids arrays, but repeated prefix copies can be quadratic and still allocates slices | Rejected |
| Build a reversed byte Array first | Avoids character cells, but introduces a larger generic Array[Int] intermediate | Rejected |
| Borrow input; copy units directly into one exact result | O(bytes) work, constant auxiliary space, one result cell | Selected |
| Detect all palindromes before construction | Can share more unchanged values, but adds byte comparisons and a prepass before nonidentity construction | Deferred; this pass removes intermediates with a direct constructor and exposes identical-character measurements explicitly |
| Consume a unique input and reverse in place | Could eliminate changed-result allocation; requires a consuming compiler/runtime ABI and variable-width/raw-unit handling | Separate ownership design, not hidden in this change |

## Construction and bounds

The private prelude stub lowers to `plum_str_reverse` through the existing raw
primitive mechanism. The runtime borrows input and returns one owned reference.
It first validates `0 <= n <= Int::MAX - 17`, reserving room for the existing
16-byte header and terminating NUL. Invalid sizes fail before any byte read,
reference increment or allocation.

Empty input returns static storage. The first unit is scanned before allocation;
if it covers the whole input, the result increments the input reference and
shares the cell, even for a long malformed continuation run. Multiple units
always produce an independent exact-sized `n + 17` cell, even for a palindrome.
No byte-equality prepass is added.

For a source unit `[start,end)`, the output interval is `[n-end,n-start)`.
The scanner guarantees `0 <= start < end <= n`; successive intervals partition
the result without overlap or gaps. Each unit's own bytes keep their order.
Every input byte is scanned/copied a bounded number of times. There are no
per-unit stack allocations, recursive calls, codepoint decoding or expansion.
An allocation failure uses the standard fatal storage path. It occurs after
the first-unit scan, so the OOM test supplies real input bytes, not just headers.

## Perceus and retained memory

This is a produced-program runtime improvement. The input's reference count,
bytes and capacity are preserved regardless of uniqueness or aliases. Existing
compiler last-use releases and reference counting govern the borrowed input
and owned result; there is no new Perceus analysis pass or cell-layout change.

Empty results do not retain oversized empty input storage. A shared single-unit
result does retain its input capacity: a one-byte input with capacity 4096
keeps a requested 4113-byte cell alive after the caller releases its reference,
until the result is finally released. Keeping the alias can also affect later
append uniqueness. Multiple-unit output compacts into one exact independent
cell without retaining spare input capacity. Tests cover both unique and
shared sources, final owners and zero live bytes after release.

Requested allocation traffic, requested live/peak heap, compiler allocation
traffic and RSS are different quantities. Removing intermediates reduces
traffic and temporary live buffers for the measured programs, without implying
all programs retain less memory or that compiler RSS falls.

## Reproduction and verification

```sh
python3 bootstrap/measure-string-reverse.py BEFORE AFTER \
  --out docs/string-reverse-measurements.json
python3 bootstrap/string-growth-check.py AFTER --sanitize
```

The measurement builds ordinary programs with each compiler. An allocation-free
C observer independently verifies every output group, byte length and NUL while
input and output remain live. Setup is excluded from traffic/live deltas;
requested process heap peak includes setup. There is no LLVM substitution and
no timing or RSS improvement claim.

The public fixture compares against `Array.concat_all(Array.reverse(chars_of(s)))`,
including malformed raw Strings, NULs, combining marks, first-class calls,
mutable String/Bytes/CStr aliases and a large result. Public properties cover
valid UTF-8 reversal laws and independent expected output. Allocation fixtures
pin empty/single-character results to zero allocation, 1000 changed eight-byte
results to 1000 allocations / 25,000 bytes, and a large reversal to its input
and output cells only.

The native guard extracts actual emitted LLVM and tests 8192 unique/shared
raw-byte conversions against an independent boundary-table oracle. It checks
exact requests, static empty storage, reference counts, source immutability,
final releases, retained capacity, header failures, OOM, one million ASCII
units, and a million-byte continuation run in O0/O2 tracked/untracked builds.

## Produced-program results

`docs/string-reverse-measurements.json` contains 100 before/after rows across
sizes, debug/release modes, ASCII, Unicode/NUL, combining marks, identical
characters and a heap-allocated single supplementary codepoint. The observer
verifies every byte group. For the largest repeated inputs, both modes agree:

| Input | Before allocations / requested traffic | After | Requested process heap peak before → after |
| --- | ---: | ---: | ---: |
| 131,072 ASCII bytes | 131,090 / 6,684,401 bytes | 1 / 131,089 bytes | 5,898,241 → 262,178 bytes |
| 262,144 Unicode/NUL bytes | 131,090 / 6,946,545 bytes | 1 / 262,161 bytes | 6,160,385 → 524,322 bytes |
| 98,304 bytes with combining marks | 65,553 / 3,407,617 bytes | 1 / 98,321 bytes | 3,014,657 → 196,642 bytes |
| 32,768 identical ASCII characters | 32,784 / 1,670,929 bytes | 1 / 32,785 bytes | 1,474,561 → 65,570 bytes |
| One four-byte heap codepoint | 4 / 98 bytes | 0 / 0 bytes | 98 → 72 bytes |

The Unicode/NUL example saves 6,684,384 cumulative requested bytes per reversal
(about 6.68 MB), and 5,636,063 bytes of measured peak requested heap. Its final
output still occupies 262,161 requested bytes in either implementation; the
saving comes from temporary character cells and arrays. The single-codepoint
case's small process peak is dominated partly by setup. Empty reversal drops
one 16-byte temporary array allocation; its result was already static.

The emitted native guard compile/links for Windows x64 and macOS arm64 and
passes at O0 under QEMU with a statically linked Linux arm64/musl build. These
supplement native CI, without claiming execution on Windows or macOS locally.

## Compiler results

`docs/string-reverse-compiler-measurements.json` records the merged #493 compiler
and the new compiler built through the refreshed seed, each emitting the same
current compiler sources. Source hashes are verified against the final tree.
One warmup precedes two alternating measured samples per mode:

| Compiler operation | Before requested traffic | After |
| --- | ---: | ---: |
| Release LLVM emission | 880,591,799–880,591,800 bytes | 880,603,490 bytes |
| Debug LLVM emission | 1,047,483,799 bytes | 1,047,401,705 bytes |

Release increases about 11.7 KB (0.0013%); debug decreases about 82.1 KB.
This is effectively flat compiler allocation traffic, not a meaningful compiler
memory reduction. The current compiler does not call `String.reverse`; these
numbers include changes to emitted runtime size and source/module construction.
RSS varies across samples and supports no reduction claim. The material
measured gain is in produced programs which reverse strings.
