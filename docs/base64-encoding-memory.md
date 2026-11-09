# Exact-size base64 encoding with static alphabets

Both base64 encoders built an `Array[String]` of one-character fragments and
joined it. Byte reads also went through the optional-index API. Two global
`chars_of` calls initialized 128 one-character Strings and two arrays whenever
Encoding loaded, including programs that only used hex or percent encoding.

## Alternatives compared

A bounded experiment compared the existing fragment path, an owned String
accumulator using Perceus concat reuse and direct byte reads, and an exact-size
constructor shared by the standard and URL-safe variants. Both modes passed
comparisons covering empty input, every byte value and both short tails. The
exact prototype replaced only the measured wrapper's LLVM; its bounded unchecked
size calculation was exploratory. Production measurements use ordinary compiler
builds with checked sizing and no LLVM substitution.

For `abc` repeated 32,768 times followed by `FB FF` (98,306 input bytes), standard
base64 produces 131,076 payload bytes. Debug/release reported the same requested
memory in this experiment:

| Approach | Construction allocations | Requested traffic | Retained result request | Whole-process peak requested bytes |
| --- | ---: | ---: | ---: | ---: |
| Fragment array | 98,323 | 5,693,109 | 131,093 | 2,510,035 |
| Owned String accumulator | 23 | 582,348 | 194,187 | 425,329 |
| Exact-size constructor prototype | 1 | 131,093 | 131,093 | 232,776 |

The accumulator removes fragment/optional objects but grows/copies repeatedly
and retains spare capacity. Exact sizing is available before reading any input
bytes, so it avoids both costs. The prototype comparison keeps the old Encoding
startup tables for every variant; production additionally removes that startup
heap cost.

## One constructor, two policies

The shared private runtime constructor borrows Bytes and selects a 64-byte static
alphabet. Standard encoding uses `+/` with padding; URL encoding uses `-_` and
omits padding, preserving Plum's existing contracts. Group encoding and padding
follow [RFC 4648 §§4–5](https://www.rfc-editor.org/rfc/rfc4648.html#section-4).

Nonempty output requests exactly `payload_length + 17` bytes: the unchanged
16-byte cell header, ASCII output and NUL. Empty output keeps using the shared
static cell. Borrowed input is never mutated, including unique cells with spare
capacity. The result does not retain input, and String/Bytes/CStr aliases stay
immutable. Perceus ARC and compiler-selected reuse remain intact; no new writable
buffer interface or cell layout is introduced.

The scalar length helper divides input into groups and a remainder before
checked multiplication/addition. It avoids `n + 2` overflow and checks the
URL-tail size directly rather than validating padded size first and subtracting
padding afterward. With `M = INT64_MAX - 17` and `q = M / 4`, `3*q + 1` input
bytes have a valid URL result of exactly `M` bytes, while their padded result is
too large. Native scalar tests cover that boundary without enormous allocations.
Input/header limits, group multiplication and tail addition are checked before
allocation or byte reads, using the existing allocation-free size-error path.

The group writer receives one, two or three bytes. Conditional byte loads avoid
reading an absent tail; unused bits start at zero. It writes two or three URL
tail characters, or four padded standard characters, with no endian-dependent
word loads or temporary managed objects. Standard and URL wrappers share this
writer and planner, rather than maintaining separate algorithms.

## Startup and larger memory story

The principal saving is in **compiled programs calling either base64 encoder**:
remove per-byte optional objects, fragment arrays, their retain/release work and
their overlap with output. The nonempty result remains the same exact size.

The alphabets are now 128 static bytes instead of managed global tables. Any
program loading Encoding avoids the old **130 startup allocations / 3,360
requested bytes**, even if it never base64-encodes anything. Those static bytes
still occupy binary storage; zero startup heap allocation is not zero storage.
The startup guard calls empty base64, hex and percent encoders and requires zero
managed allocations. Percent-encoding budgets are updated deliberately to
remove the startup allowance introduced in #486.

The compiler also loads Encoding for its own hex/URI operations. Its separate
traffic measurements below use a refreshed seed and actual seed-to-source
compiler, so the distributed compiler runs with the new startup behavior.
Requested traffic, retained requested storage, process peak and RSS describe
different quantities. No timing or RSS improvement is inferred from traffic.

Related [#211](https://github.com/bradcypert/plum/issues/211) was closed as a
false-positive stack report after generated musttail calls were reviewed. This
change targets allocations and makes no stack-exhaustion claim. It leaves all
decoder algorithms and their accepted-input rules unchanged.

## Reproduction and safeguards

```sh
python3 bootstrap/measure-base64-encode.py /path/to/before /path/to/after \
  --out /tmp/base64-encoding-measurements.json
python3 bootstrap/string-growth-check.py /path/to/after --sanitize
bootstrap/property-check /path/to/after
PLUM=/path/to/after bootstrap/alloc-check
```

The observer checks every output byte, length and NUL without Plum allocations.
Traffic/live deltas exclude input/setup; process peak includes them and Encoding
startup. Both alphabets, all three tail residues and five sizes are measured in
debug/release with unmodified compiler builds. Input stays live across
observation and the result's CStr keeps its owner alive.

The emitted runtime guard runs O0/O2, tracked/untracked and optionally ASan/LSan.
It checks lengths 0–258 against an independent C reference, unique borrowed input
with surplus capacity, exact requests and ByteStorage release, 10,000 scalar
lengths, near-limit valid/invalid padding boundaries and injected OOM before any
byte read. The public fixture independently checks both all-byte encodings and
SHA-256 fingerprints of every prefix length 0–256, generated with Python's
`base64` and `hashlib`. Properties check output lengths, alphabet/padding
relations, group-aligned concatenation and decoding through public APIs.

The allocation fixture checks 1,000 calls for six short-tail/alphabet cases:
exactly 6,000 result allocations / 123,000 requested bytes with no startup cost.

## Production measurements

[Program measurements](base64-encoding-measurements.json) confirm exact result
requests in both modes for every measured nonempty input. For the 98,306-byte
standard case above, traffic remains 131,093 bytes and production process peak
is **229,416 bytes**, a further 3,360 bytes below the prototype because startup
alphabets no longer occupy heap storage. The corresponding unpadded URL result
is 131,092 requested bytes. Empty per-call output was already static before;
the new zero-allocation startup benefit is recorded separately.

[Compiler measurements](base64-encoding-compiler-measurements.json) compare the
same final sources, one warmup and two recorded samples per mode:

| Compiler emission | Before requested bytes | After requested bytes |
| --- | ---: | ---: |
| Default/debug | 1,074,208,401–1,074,259,634 | 1,073,950,819 |
| Release | 907,614,180–907,614,181 | 907,240,704–907,240,705 |

The combined constructor/module-loading change saves about 0.26–0.37 MB of
compiler emission traffic in these samples, separately from the much larger
program-level base64 savings. Requested totals are observed ranges; RSS is
recorded independently in the artifact. These few samples do not establish an
RSS or timing improvement. The URL allocation fixture also drops exactly 130
allocations when the common Encoding startup cost is removed.
