# Exact-size String repetition

Follow-up to #478/#480/#481. Generic appends keep their unique-only 1.5×
capacity policy. String.repeat knows the complete output length, so it can
avoid intermediate cells and spare output capacity.

## Alternatives measured

Compared on the same merged #481 compiler/runtime, in debug and release:

| `"abc".repeat(4096)` | New allocations | Requested traffic | Retained result request | Process peak |
| --- | ---: | ---: | ---: | ---: |
| Current geometric append loop | 17 | 50,948 B | 17,043 B | 28,398 B |
| Pure Plum binary chunks + exact join | 12 | 24,774 B | 12,305 B | 18,466 B |
| Exact allocation + prefix doubling | 1 | 12,305 B | 12,305 B | 12,305 B |

The bounded comparison used a temporary emitted-LLVM substitution for the
exact candidate. It was not overflow/OOM validation. Production measurements
below use actual, unmodified compiler builds and the refreshed seed.

Binary chunks improve the loop without adding a primitive, but create
intermediate Strings and retain selected blocks until the final join. The
chosen constructor uses the existing allocation/ARC boundary: one fresh cell,
no public builder or mutable-buffer interface, and no layout change. A native
realloc design is unnecessary when the final size is already known.

## Constructor and ownership

The private prelude bridge maps to one runtime implementation, like the other
String constructors. It borrows its input and returns an owned String:

- Nonpositive count or empty input returns a shared static empty value.
- Count one with nonempty input retains and returns that input, including any
  capacity it already owns; there is no new allocation or implicit shrink.
- Otherwise, checked multiplication and header/terminator sizing precede
  allocation and data copying. Invalid lengths or a result beyond
  INT64_MAX - 17 take the existing allocation-free fatal boundary.
- Allocate exactly `length * count + 17` requested bytes with ByteStorage
  provenance. The existing required/preferred allocator receives equal sizes;
  a failed allocation terminates before any store/copy.
- Copy the input once, then copy the initialized output prefix to its tail,
  doubling progress until the remaining tail is shorter. Each memcpy's source
  and destination are disjoint. Every output byte is written once, with
  O(log count) copy calls and constant auxiliary space.
- Store the logical length and final NUL; input bytes, aliases and layout stay
  unchanged. This is byte repetition, preserving UTF-8, embedded NUL and raw
  bytes obtained through existing unsafe interfaces.

Perceus remains the ownership model: shared inputs are immutable, the new
result has one owner, and final release retires its original requested size.
The constructor needs no new consuming-call inference. Unknown-length String
appends continue to use #480's policy.

## Production memory impact

Actual debug/release results agreed. Deltas exclude argument/input setup;
peaks cover the whole process. Bytes include cell headers and terminators,
but exclude allocator rounding, native buffers and registry metadata.

| Workload | Traffic before → after | Result request before → after | Peak before → after |
| --- | ---: | ---: | ---: |
| abc × 4,096 (12,288 data bytes) | 50,948 → 12,305 B | 17,043 → 12,305 B | 28,398 → 12,305 B |
| abc × 65,536 (196,608 data bytes) | 877,217 → 196,625 B | 294,867 → 196,625 B | 489,054 → 196,625 B |
| UTF-8/NUL × 8,191 (65,528 data bytes) | 258,682 → 65,545 B | 86,307 → 65,545 B | 143,838 → 65,545 B |

All nonempty multi-repeat cases allocate one exact-sized cell. Empty and
count-one cases create no new cells. A shared heap input still retains its
existing charge; zero new allocations does not mean the value occupies no
memory. Small old results could already use allocator slack, so exact request
sizes are not a promise of lower traffic at every tiny count.

This primarily improves **compiled programs that call String.repeat**. The
refreshed seed also puts the implementation in Plum itself. Whole-compiler
emission traffic stayed about 1.079 GB debug / 0.912 GB release, with RSS near
128 MiB / 114 MiB: no compiler-emission memory saving is claimed. Requested
traffic/live/peak measurements are separate from RSS; no timing claim is made.

Reproduce actual builds on a platform with the normal compiler toolchain:

```sh
python3 bootstrap/measure-string-repeat.py BEFORE_COMPILER AFTER_COMPILER --out /tmp/repeat.json
```

[Raw records](string-repeat-measurements.json) distinguish bounded alternatives,
actual compiled programs, and compiler-only emission profiles.

## Verification and bootstrap

The existing String runtime guard now covers repetition at O0/O2 and public
String/Bytes/CStr paths in debug/release, ordinary/diagnostic modes, including
ASan/LSan. Its extracted LLVM helpers now carry real sanitizer attributes when
requested. Injected allocation failure, negative lengths, product/header
limits, exact requests, origin totals, RC sharing and final release are
checked without enormous allocations.

Public fixtures cover partial final fills, NUL/UTF-8/raw bytes, unchanged
input aliases, INT_MIN counts and empty input with INT_MAX count. Generated
public properties compare against concat and check length, additive and
multiplicative repetition laws. Two abort fixtures check overflow diagnostics
and unreachable code. The allocation fixture requires one 196,625-byte cell
for abc × 65,536.

Source-built documentation is regenerated. The seed is refreshed because both
prelude and generated runtime changed, followed by a seed → source rebuild,
reproduction check and measurements of that actual compiler. Native shim
content and its fingerprint are unchanged.
