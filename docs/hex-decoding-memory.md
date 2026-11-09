# Hex decoding without an output array

`Encoding.hex_decode` previously allocated optional byte-read results, built an
`Array[Int]` of decoded values, and copied that array into Bytes. A late invalid
pair discarded the intermediate array. Validation now reads bytes directly and
constructs the exact Bytes payload only after all validation succeeds.

## Alternatives compared

The [measurement harness](../bootstrap/measure-hex-decode.py) compares the original
public decoder, a source-level prototype with direct reads and the original
output array, and the final validated constructor. Every recorded row first
checks all output bytes or the exact diagnostic, length and NUL. Before/after use
ordinary compiler builds without substituting LLVM; the array prototype is built
by the before compiler. Debug and release agree.

For `6100ff2b` repeated 32,768 times (262,144 input bytes, 131,072 output bytes):

| Approach | Public-call allocations | Requested traffic | Additional live requested bytes | Whole-process peak requested bytes |
| --- | ---: | ---: | ---: | ---: |
| Optional reads and output array | 262,162 | 9,592,505 | 131,089 | 2,645,937 |
| Direct reads and output array | 18 | 3,301,049 | 131,089 | 2,645,937 |
| Validation and exact construction | 2 | 131,113 | 131,089 | 524,322 |

The array still stores eight-byte integers and then copies them into Bytes.
Removing it avoids that storage as well as the optional reads. The two final
allocations include a 131,089-byte Bytes cell and 24 bytes of Result bookkeeping.
Retained successful output stays exactly the same size as before.

With `GG` or `AG` appended to that valid prefix, the original decoder requests
9,461,555 bytes; the direct-read array prototype requests 3,170,051; the final
path requests 115. An early invalid pair falls from 153 to 105 bytes in the
measured workloads. These errors allocate no decoded payload, while diagnostic
and Result bookkeeping still allocate. Odd-length rejection already uses a
static error Result: it remains zero-allocation and takes precedence over any
invalid digit. Empty success uses a static empty Bytes cell plus ordinary Result
bookkeeping (one 24-byte allocation in the measured calls).

The observer keeps input live and snapshots inside the matched Result arm, after
temporary Results may have been freed. Traffic/live deltas exclude input setup.
Peak is whole-process requested storage: in the exact-path examples it includes
earlier overlapping input-construction allocations, not just the decoder. No
RSS or timing conclusion follows from these requested-byte totals.

## Design and Perceus ownership

```text
hex_decode(encoded)
  odd byte length → existing static Err
  empty → static empty Bytes in ordinary Ok
  validate every pair in Plum
    first invalid pair → same Err with pair-start byte offset
    valid → checked exact Bytes constructor → ordinary Ok
```

Odd-length rejection must precede lexical checking. An invalid low digit still
reports the first byte of its pair, and offsets count encoded UTF-8 bytes.
Uppercase, lowercase and mixed-case hex remain accepted; whitespace, separators,
`0x` prefixes, NUL and non-ASCII digits remain invalid. Output is Bytes, so
arbitrary bytes including NUL and invalid UTF-8 are preserved. No text validation
or normalization is introduced.

Validation and typed Result construction stay in Plum. The hex and percent
validators continue to share the existing `hex_digit_value` grammar. Their
native writers now share `plum_hex_pair`, a small arithmetic conversion for
already validated ASCII digits, rather than duplicating conversion arithmetic.
This helper is not a second parser. The same runtime ownership and failure tests
exercise both writers.

The private hex constructor borrows input and returns a +1 owner. It checks the
stored input length against the signed/header limit and rejects odd lengths
before allocation or byte reads. The exact length is `input_length / 2` with no
rounding addition or multiplication overflow. Each bounded output position reads
one complete input pair and writes one byte; the final cell has an explicit
length and NUL terminator. Nonempty output requests exactly
`decoded_length + 17` bytes through the existing checked ByteStorage allocator.
Empty output uses static storage. Lexical validity remains a private caller
precondition; malformed public input is always a typed Err, while impossible
internal sizes and allocation failure use the established fatal path.

A fully native Result-returning decoder would couple the runtime to
compiler-assigned enum layout. Allocating a byte buffer before validation could
avoid a second scan, but spends output storage on malformed input and requires a
separate error/ownership protocol or mutable construction interface. Direct reads
with the array avoid neither the eight-byte temporary values nor the final copy.
The existing validation/constructor boundary keeps those concerns separate.

Consuming in-place decoding could avoid one payload allocation at a proven last
use. It would also retain the twice-as-large encoded allocation and require an
explicit compiler consuming boundary. Reference count one alone does not permit
mutation of a borrowed argument. The chosen constructor always leaves input
unchanged, including unique input with spare capacity, and returns independent
compact storage. It follows existing Perceus ARC/copy-on-write rules without a
new ownership convention or cell layout. String/Bytes/CStr aliases continue to
own their cells and release through the normal allocator.

## Larger memory story and related issues

The main savings are in **compiled programs calling hex decoding**, particularly
large successful inputs or late invalid pairs. Compiler callers include LSP's
internal command filename envelopes and package hash decoding. Ordinary compiler
emission is a separate workload; its measurements below describe combined
module-loading and emitted-runtime costs rather than repeated hex decoding.

The original Encoding API in [#25](https://github.com/bradcypert/plum/issues/25)
continues to return Bytes/typed errors with the same accepted spellings. Related
[#211](https://github.com/bradcypert/plum/issues/211#issuecomment-6132629271) and
[#274](https://github.com/bradcypert/plum/issues/274#issuecomment-6132630419) were
closed after their stack-exhaustion claims were disproved by generated guaranteed
tail calls. This change targets allocation and does not reopen or claim to fix
those false positives. The new Plum validator also remains directly tail
recursive with a matching signature.

## Reproduction and safeguards

```sh
python3 bootstrap/measure-hex-decode.py /path/to/before /path/to/after \
  --out /tmp/hex-decoding-measurements.json
python3 bootstrap/string-growth-check.py /path/to/after --sanitize
bootstrap/property-check /path/to/after
PLUM=/path/to/after bootstrap/alloc-check
```

[Program measurements](hex-decoding-measurements.json) contain 240 rows: three
approaches, debug/release, five sizes and eight valid/error workloads. They cover
lower/upper/mixed-case input, early and late invalid pairs, odd-length precedence,
and empty input. The observer verifies arbitrary decoded binary bytes without
allocating or assuming native Result layout.

A frozen original public decoder compares exact results for 11,111 short strings
over ten symbols including NUL and Unicode. Additional fixtures and properties
cover all 256 values with upper/lower/mixed-case digits, invalid low digits,
encoded-byte pair offsets, odd-length precedence, invalid UTF-8 output, large
buffers and String/Bytes/CStr aliases after rebindings/appends.

The emitted-runtime guard uses actual compiler output at O0/O2, tracked/untracked
and ASan/LSan. It checks every valid hex-pair combination in four case forms,
output lengths 0–258, unchanged unique input with spare capacity, exact requests,
ByteStorage accounting and final release, static empty output, scalar near-limit
sizes, and header-only invalid-size/allocation-failure cases before payload reads.
Existing percent-decoder tests cover the shared conversion helper as well.
Public property tests stay within the public API. The refreshed seed is rebuilt
to the source compiler and checked for bootstrap consistency/native filename
bytes. Documentation is regenerated with that source-built compiler.

Allocation budgets for 1,000 calls include Result/error bookkeeping:

| Workload | Allocations | Requested bytes |
| --- | ---: | ---: |
| Valid `6100fF2b` | 2,000 | 45,000 |
| Invalid pair `00AG` | 3,000 | 105,000 |
| Odd-length `G00` | 0 | 0 |

## Compiler measurements

[Separate compiler measurements](hex-decoding-compiler-measurements.json) compare
the predecessor and refreshed seed-to-source compilers on identical final
compiler/stdlib sources, with one warmup and two recorded samples per mode:

| Compiler emission | Before requested bytes | After requested bytes |
| --- | ---: | ---: |
| Default/debug | 1,077,193,449–1,077,193,451 | 1,076,887,453–1,076,887,454 |
| Release | 909,931,372–909,989,298 | 909,776,096–909,794,125 |

Emission traffic is about **306 KB lower in debug** and **137–213 KB lower in
release** in these samples. This is a small combined module/runtime effect,
separate from the much larger per-decode program savings; ordinary emission is
not a hex-decoding workload. RSS ranges overlap and no RSS or timing improvement
is claimed. Samples ran alongside other validation, so elapsed times are omitted.
The compiler memory guards continue to enforce the existing budgets.

To reproduce the compiler comparison on Linux:

```sh
python3 bootstrap/compiler-profile.py --before /path/to/before --after /path/to/after \
  --case emit-compiler --case emit-compiler-debug --samples 2 --out /tmp/hex-compiler-profile
```
