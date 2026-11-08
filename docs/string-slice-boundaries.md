# Slice strings by scanning boundaries and copying the result

`String.slice` used to materialize every character in the input, copy selected
character references into another array, then concatenate them. Even an empty
or one-character slice of a large string allocated and released the entire
decoded input. The shared API now finds the two codepoint boundaries directly
in the existing byte storage and copies the selected bytes once.

## Design and alternatives

| Approach | Assessment |
| --- | --- |
| Scan boundaries, copy the selected bytes | Chosen: existing Bytes primitives, one result allocation for a nonempty slice, no intermediate character arrays |
| Decode once in individual compiler callers | Useful for repeated scans but leaves ordinary programs and other library callers with the expensive slice implementation |
| Add a native UTF-8 slice primitive | Could reduce per-byte RC overhead, but expands primitive plumbing for an operation expressible in the prelude |
| Return shared substring views | Can remove the result copy but changes String representation, CStr interoperability and backing-buffer retention; a separate ownership/representation design |
| Cache decoded characters or byte offsets | Speeds repeated indexing but adds persistent storage, retention and invalidation responsibilities |

The private boundary scanner is tail recursive. Its state is the byte offset
and number of additional character starts to skip, after consuming the first
byte of a group. Continuation bytes never count as starts. This preserves the
existing `chars_of` grouping even for malformed raw bytes, including a leading
continuation run and truncated sequences. Valid UTF-8 is never split inside a
codepoint; NULs and combining marks keep their existing semantics.

Negative starts clamp to zero; reversed/empty ranges return an empty String.
Out-of-range endpoints stop at the input length. Arithmetic uses differences
only after checking the ordering, so both signed Int extremes are supported.
The two boundary scans cover disjoint input prefixes. Auxiliary storage is
constant, apart from the returned String. Both recursive branches emit a
guaranteed `musttail` call, including in debug builds.

## Perceus and the memory story

The input's Bytes view shares storage through ordinary Perceus RC. The result
has independent owned storage from `bytes_slice_raw`; `str_of_bytes_raw`
retags that owned allocation without copying or validating it again. This is
safe here because the selected range follows the input's existing character
groups, and preserves raw bytes instead of rejecting previously accepted
strings. No pointer arithmetic or new native primitive is introduced.

This removes temporary arrays and per-character Strings from the algorithm.
It does not change ARC, FBIP inference, cell layout, CStr ownership, or cache
policy. Short slices do not keep a large source buffer alive after their input
is released. Byte access still has normal retain/release work; this is not a
new borrowing analysis.

Ordinary compiled programs benefit whenever they call this API, including
indirect calls through the library. The compiler benefits as another Plum
program. Its seed is refreshed and rebuilt through seed → source so the
distributed compiler itself uses the new implementation. Cumulative allocation
traffic, returned-result storage and peak resident memory are separate costs.

## Scope and related issues

The qualified-name loop identified during the previous profile still scans
prefixes repeatedly. This PR removes its character materialization cost but
does not claim to make that caller linear; a single-pass name scan remains a
follow-up. No type-name resolution or alias lookup rule changes.

Related-issue search found #118's same repeated-slicing pattern in public path
helpers, already resolved with byte scanning. That precedent supports the
shared byte-boundary approach. #440 concerns alias-qualified type resolution
and remains separate. #63 remains the broader compiler-cost work; #16
established the bytes/codepoints distinction.

## Coverage and allocation guard

The public property compares slices with the previous independent combination
of `chars_of`, `Array.slice` and `Array.concat_all` on 200 deterministic Unicode
strings and 64 boundary pairs per string. It includes UTF-8 width boundaries,
NULs, combining marks, Int extremes, reversed and out-of-range bounds, and
checks that adjacent slices reconstruct the input.

The executable fixture also checks malformed bytes, temporary inputs,
first-class calls, immutable aliases and CStr owners through replacement of
both the input and result. Its 2,621,440-byte input exercises long scans with
bounded stack usage. It runs under ASan/LSan with memory diagnostics in both
debug and release modes.

The allocation fixture constructs a 20,480-codepoint mixed UTF-8/NUL input by
explicit doubling, then takes 500 slices near its beginning, middle and end,
including empty ranges. The baseline requests 278,645,594 bytes in 10,241,312
allocations. The new implementation requests 96,594 bytes in 312 allocations:
12 for input construction and 300 for nonempty results. Its 312-allocation and
120,000-byte limits reject the old implementation. Empty results allocate no
heap storage.
Debug and release builds produce the same counts and output. The existing
URL-parse workload also improves from 22,975,876 to 8,523,776 allocations
through this shared API; its allocation limit is tightened accordingly.

## Compiler measurements

The baseline compiler is the final #475 seed-to-source binary, whose source
tree is identical to merged main `43381d3`. The candidate uses its refreshed
seed and the same build path. Both process the same current program sources;
their embedded prelude implementations intentionally differ. Generated LLVM
hashes therefore differ, while executable output is checked by the corpus.
[Raw samples, input and binary hashes](string-slice-boundaries-measurements.json)
include one warmup and three serial interleaved measured rounds on Linux
x86_64 with Clang 23.1.4.

| Whole-compiler workload | Requested MB before → after | Allocations before → after | Median seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Release emit | 1105.78 → 1071.42 | 17,330,401 → 16,113,607 | 5.334 → 5.312 | 114.3 → 114.7 |
| Debug emit | 1274.60 → 1238.80 | 20,648,614 → 19,370,144 | 5.321 → 5.049 | 128.8 → 129.1 |
| Check | 305.24 → 285.15 | 9,290,952 → 8,623,876 | 2.086 → 1.755 | 47.2 → 47.2 |

Requested MB are decimal cumulative allocation traffic, not binary size or
simultaneously live storage. Release emission saves about 34.35 MB (3.1%),
debug emission 35.80 MB (2.8%), and checking 20.09 MB (6.6%). Peak RSS is
effectively flat. Checking is faster in these samples (1.953–2.184 seconds
before, 1.716–1.795 after). Emission timing ranges overlap: release
5.164–5.438 before / 4.893–5.459 after; debug 5.057–5.408 before /
4.971–5.752 after. This is not a general claim of faster builds.

Existing whole-compiler memory ceilings remain in place; the focused slicing
allocation fixture rejects materializing characters independently of RSS.

```sh
python3 bootstrap/compiler-profile.py --before /tmp/plum-char-ci \
  --after /tmp/plum-slice-ci --samples 3 --case check-compiler \
  --case emit-compiler --case emit-compiler-debug --out /tmp/plum-slice-bench
```
