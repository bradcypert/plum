# Base64 validation without intermediate arrays

The decoder previously collected an integer array of sextets, then a second
integer array of decoded bytes, then copied those bytes into a Bytes cell.
Public optional byte reads also allocated per encoded character. Both public
entry points shared this path, including its padding and error precedence.

## Alternatives compared

A bounded experiment compared the existing two-array pipeline, a direct-byte
validator with one output integer array, and the same validator followed by
exact-sized Bytes construction. Debug/release agreed, and all three matched an
oracle covering valid tails, both alphabets, malformed padding, nonzero pad
bits, NUL/Unicode input and diagnostic precedence. The exact prototype replaced
only its payload sink's LLVM and did not contain production size guards; the
production measurements below use unmodified compiler builds.

For 131,076 encoded bytes (`YWJj` repeated 32,768 times, followed by `+/8=`),
returning 98,306 binary bytes:

| Approach | Public-call allocations | Requested traffic | Observed additional live bytes | Process peak requested bytes |
| --- | ---: | ---: | ---: | ---: |
| Two integer arrays | 131,111 | 9,592,291 | 98,323 | 4,095,961 |
| One integer array | 17 | 1,695,435 | 98,323 | 1,335,241 |
| Exact payload | 2 | 98,347 | 98,323 | 229,436 |

The exact path includes Result bookkeeping: it is one payload allocation, not
one allocation for the entire public Result-returning call. The observed live
difference is taken inside the matched arm after temporary Results may have
been released. The retained Bytes request is unchanged.

A late invalid byte after 131,072 valid characters previously incurred
6,348,544 requested bytes. Both direct validators reject it with 120 bytes of
traffic and no decoded output buffer. Returning a nonzero-pad-bits or truncated
error after that prefix uses one 24-byte Result allocation in the prototype,
rather than the old growing arrays.

## Chosen seam and Perceus

Validation and Result construction stay in Plum. A tail scanner reads bytes
without optional objects and returns the validated prefix length. It preserves
these stages in order:

1. Scan all characters and validate optional terminal padding.
2. After that succeeds, reject truncated groups and nonzero unused tail bits.
3. Allocate/fill the final Bytes only after full validation succeeds.

Moving tail checks earlier could change errors: a later invalid character or
padding error must still outrank a partial-group error. A single output-array
approach removes the sextet array but still stores eight-byte integers and
copies them into Bytes. A fully native Result-returning parser would couple the
runtime to enum tags/layout assigned by the compiler. The existing runtime
explicitly avoids that convention for other Result-returning operations, and
this change follows the same boundary.

A scalar classifier is shared by validation and the writer. It accepts both
alphabets, including their mixed use as before. Padding/canonical-tail behavior
remains as specified by the existing API and covered by
[RFC 4648 §§3.5 and 4](https://www.rfc-editor.org/rfc/rfc4648.html#section-3.5).
Whitespace and arbitrary nonalphabet bytes remain errors, with the same messages
and byte offsets. All errors remain ordinary typed Results, using the compiler's
normal ARC and reuse logic.

Consuming input reuse was also considered. Decoding into a dead, unique encoded
buffer could avoid the payload allocation, but would retain its larger encoded
capacity (roughly four bytes of input storage per three output bytes). It also
needs an explicit compiler-proven consuming boundary; a unique reference count
alone does not authorize mutation of borrowed input. The chosen constructor
keeps compact output and the established borrowed contract without adding an
ad-hoc ownership exception.

The private runtime constructor accepts only a validated prefix. It defensively
checks signed/header limits, prefix bounds, group remainder and padding shape
before allocation or byte reads. Grammar validation remains the Plum caller's
precondition; this is not a second public decoder API. Group arithmetic is
checked without rounded-length overflow. Nonempty payloads request exactly
`decoded_length + 17` bytes through the existing allocator and ByteStorage
accounting. Empty payload uses the shared static cell. Conditional byte reads
handle both short tails, and every output has a length and trailing NUL.

Input is borrowed and never mutated or retained by the result, including unique
input with surplus capacity. Perceus ownership and String/Bytes/CStr layout stay
intact. Results stay in Plum, avoiding a hard-coded native enum representation
or an untyped status-code/error protocol.

## Larger memory story

The primary benefit is in **compiled programs decoding base64**: eliminate
intermediate arrays and per-character option cells, and avoid output allocation
on malformed input. Successful nonempty payload storage stays the same exact
size. Result/error bookkeeping still allocates; zero output buffers does not
mean zero allocations. Input remains live across the measurement callback, and
process peak includes setup. Traffic, observed live storage, process peak and
RSS are distinct quantities.

The compiler loads Encoding for hex and URI operations, although base64 decoding
is not a hot path in ordinary compiler emission. Separate compiler measurements
below use a refreshed seed and an actual seed-to-source executable. They cover
the combined module/runtime change, not a claim that every compilation decodes
base64. No timing or RSS improvement is inferred from requested-byte totals.

Related [#197](https://github.com/bradcypert/plum/issues/197) already fixed
malformed padding/canonical tail bits. Its behavior is preserved, including
optional padding and both alphabets. [#211](https://github.com/bradcypert/plum/issues/211)
was closed after its stack-exhaustion claim was disproved. This change targets
allocation and does not reopen or claim to fix either issue.

## Reproduction and safeguards

```sh
python3 bootstrap/measure-base64-decode.py /path/to/before /path/to/after \
  --out /tmp/base64-decoding-measurements.json
python3 bootstrap/string-growth-check.py /path/to/after --sanitize
bootstrap/property-check /path/to/after
PLUM=/path/to/after bootstrap/alloc-check
```

An allocation-free native observer checks every decoded byte or the exact error
message, length and NUL. Traffic includes public Result bookkeeping; live bytes
are a snapshot delta inside the matched arm. The result's CStr keeps its owner
alive. Both modes cover padded/unpadded tails, alphabet variants, late invalid
bytes, bad tail bits, truncation and empty input over five sizes.

The public fixture keeps a frozen pre-change decoder as a compatibility oracle
using only public APIs. It compares 11,111 strings of lengths 0–4 over ten symbols
(including NUL and Unicode), explicit precedence cases, both public decoder
names, independent Python-generated all-byte encodings, large buffers and mutable
bindings holding String/Bytes/CStr aliases. Properties cover valid spellings,
alphabet/padding choices, round trips and invalid-suffix diagnostic order.

The emitted runtime guard runs O0/O2, tracked/untracked and optional ASan/LSan.
It checks the classifier for all 256 bytes and out-of-range values, direct
construction for lengths 0–258, borrowed input with spare capacity, exact
requests/ByteStorage release, scalar boundary arithmetic and header-only failure
cases proving invalid sizes/OOM precede payload reads.

## Production measurements and compiler tradeoff

[Program measurements](base64-decoding-measurements.json) confirm the prototype
figures in debug/release using unmodified compiler binaries. The 98,306-byte
successful payload uses two public-call allocations / 98,347 requested bytes:
one 98,323-byte Bytes cell and Result bookkeeping. Empty public calls still
allocate Result bookkeeping, while their Bytes payload is static. Late invalid
characters use 120 requested bytes in the measured case; bad tail bits and
truncation use 24 bytes. These error paths allocate no decoded buffer.

[Compiler measurements](base64-decoding-compiler-measurements.json) use identical
final sources, one warmup and two recorded samples per mode:

| Compiler emission | Before requested bytes | After requested bytes |
| --- | ---: | ---: |
| Default/debug | 1,075,541,729 | 1,075,651,356–1,075,699,345 |
| Release | 908,616,375 | 908,663,297–908,663,298 |

Compiler emission traffic increases by about **47–158 KB** in these samples.
This tradeoff is recorded alongside the much larger program-decoding savings;
this change does not reduce allocation traffic in every workload. Requested
ranges are observed samples. RSS is recorded independently, with overlapping
ranges, and no RSS or timing improvement is claimed. The compiler memory guards
continue to pass.

Focused public budgets require 1,000 short successful calls to stay within
2,000 allocations / 46,000 requested bytes, and 1,000 invalid-tail calls within
1,000 allocations / 24,000 requested bytes. These counts include Result work.
