# Count characters without materializing them

`String.char_len` previously called `chars_of(s).len()`. Counting required
an array and one separately allocated String per codepoint, even though none
of those values escaped. The shared string API now counts directly from the
existing reference-counted byte view. This benefits generated Plum programs
and the self-hosted compiler, including its qualified-name scans.

## Design and alternatives

| Approach | Assessment |
| --- | --- |
| Scan the existing byte view | Chosen: allocation-free counting through the existing prelude and ownership interfaces |
| Decode once in each compiler caller | Helps individual scans, but leaves every other `char_len` caller materializing characters |
| Add a native counting primitive | Can avoid per-byte RC operations, but duplicates an existing byte-prefix scan and expands compiler primitive plumbing; a possible later CPU optimization if profiling justifies it |
| Cache codepoint lengths in String cells | Makes repeated counts cheap, but changes cell layout and every string constructor for a cost most strings never need |
| Share decoded source arrays across lexer/parser | Could address other frontend allocations, but requires a separate source-context design and still allocates characters |

The private byte-prefix helper already served `String.index_of`. It now has a
shared name, `plum__string_count_starts`, and counts non-continuation bytes.
For a whole nonempty string, the first byte contributes one character and
scanning starts at byte one. This exactly preserves `chars_of`'s grouping even
for malformed native byte sequences, rather than assuming every String is valid
UTF-8. Search offset conversion retains its original zero/zero starting state.
Embedded NULs count normally; combining marks remain separate codepoints.

The scan is linear in UTF-8 byte length and uses constant auxiliary space.
The compiler emits its recursive self-call as `musttail`. There is no decoded
array, per-character string, new native buffer, persistent cache or registry.

## Memory and Perceus

The Bytes view shares the String allocation through ordinary Perceus reference
counting. Counting releases that view and preserves aliases and CStr owners.
The helper still incurs normal retain/release work for byte access; this change
does not introduce a borrowing analysis, change ARC, or extend FBIP inference.
It removes unnecessary allocations from the algorithm itself.

Compiler allocation traffic is distinct from a program's runtime traffic.
Both benefit here because the compiler is itself a Plum program calling the
same API. Refreshing the seed puts the new prelude into the distributed
seed-to-source compiler, rather than merely fixing newly emitted user programs.
Peak resident memory and cumulative requested bytes are measured separately.

## Coverage

The public property compares counts with an independent `chars_of` array over
250 deterministic strings, including UTF-8 width boundaries, combining marks
and NULs. The executable fixture checks temporary inputs, first-class calls,
aliases, CStr ownership through replacement, malformed bytes and a scan of
2,621,440 bytes. It also runs with ASan/LSan and memory diagnostics in debug
and release modes. `String.index_of` retains its existing property/allocation
coverage.

The allocation fixture counts a 20,480-codepoint mixed UTF-8/NUL string 100
times. Its 12 input-doubling allocations request 90,294 bytes; the scans add
none. The previous implementation requests 55,797,494 bytes across 2,048,112
allocations. The new fixture's 12-allocation and 120,000-byte limits reject
that implementation while leaving byte headroom for input construction.
Debug and release builds produce the same counts and output. The existing
calendar-format and URL-parse allocation guards also improve through the
shared API; their count limits are tightened to the observed 27,000 and
22,975,876 allocations respectively.

Related issue search found no dedicated `char_len` issue. #63 remains the
broader compiler-cost work; #16 established the bytes/codepoints distinction.
The closed malformed-input issue #135 informed compatibility checks, but this
change does not replace UTF-8 validation or claim to fix that issue again.

## Compiler measurements

The baseline is merged main `7ada2ea`; both binaries use their own
seed-to-source build path. Measurements use identical current program source
inputs, one warmup and three serial interleaved rounds on Linux x86_64 with
Clang 23.1.4. [Raw samples and hashes](string-character-counts-measurements.json)
include warmups. The compilers intentionally embed different preludes: emitted
LLVM contains the old/new count implementations, so its hashes differ. Output
identity is checked at program execution rather than asserted for the IR.

| Whole-compiler workload | Requested MB before → after | Allocations before → after | Median seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Release emit | 1135.05 → 1105.84 | 18,474,203 → 17,331,292 | 4.682 → 4.694 | 115.0 → 114.9 |
| Debug emit | 1303.18 → 1274.20 | 21,794,113 → 20,648,874 | 4.893 → 4.911 | 129.2 → 129.2 |
| Check | 321.41 → 305.26 | 9,930,288 → 9,291,559 | 1.713 → 1.713 | 47.7 → 47.4 |

Requested MB are decimal, cumulative allocation traffic, not live heap size or
binary size. Release emission saves about 29.22 MB (2.6%), debug emission
28.98 MB (2.2%), and checking 16.16 MB (5.0%). These samples establish no
material speed or peak-RSS improvement. Existing whole-compiler memory limits
remain intact; the focused allocation fixture guards the new behavior.

Reproduce the comparison with independently built baseline/candidate binaries:

```sh
python3 bootstrap/compiler-profile.py --before /tmp/plum-char-baseline-ci \
  --after /tmp/plum-char-ci --samples 3 --case check-compiler \
  --case emit-compiler --case emit-compiler-debug --out /tmp/plum-char-bench
```
