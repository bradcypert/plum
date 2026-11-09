# Percent decoding without an output array

`Encoding.percent_decode` previously read each byte through `Bytes.at`, built an
`Array[Int]` of decoded bytes, and copied it into Bytes on success. A late invalid
escape discarded the entire intermediate array. This pass removes both the
per-byte Option cells and the output array while preserving the public API.

## Alternatives compared

The checked-in [measurement harness](../bootstrap/measure-percent-decode.py)
compares the original decoder, a source-level prototype that uses direct byte
reads but retains the output array, and the final validated constructor. All
three must produce the same bytes or exact diagnostic before a row is recorded.
Before/after use ordinary compiler builds without rewriting emitted LLVM; the
array prototype is compiled by the before compiler. Debug and release agree.

For `a%00%FF+` repeated 32,768 times (262,144 input bytes, 131,072 output bytes):

| Approach | Public-call allocations | Requested traffic | Additional live requested bytes | Whole-process peak requested bytes |
| --- | ---: | ---: | ---: | ---: |
| Optional reads and output array | 262,162 | 9,625,241 | 131,089 | 2,670,513 |
| Direct reads and output array | 18 | 3,333,785 | 131,089 | 2,670,513 |
| Validation and exact construction | 2 | 131,113 | 131,089 | 393,274 |

Removing optional reads helps, but the integer array still stores eight-byte
values and then copies them into the final byte cell. Validation followed by
exact construction avoids both forms of temporary storage. The two allocations
include a 131,089-byte Bytes cell and 24 bytes of Result bookkeeping. Successful
escaped output retains the same exact payload size as before.

Appending `%GG` to that encoded prefix previously incurred 9,494,318 requested
bytes. The direct-read array prototype still uses 3,202,790 bytes; validation
without an output array uses 118 bytes for the same error. The truncated `%` and
`%G` suffixes use 120 bytes in these workloads. These failures allocate no decoded
buffer, but Result/error-message bookkeeping still allocates.

Unescaped `abc+é` followed by NUL, repeated 32,768 times, previously incurred
12,148,329 requested bytes. The new public call uses one 24-byte Result allocation
and shares the existing input cell. Additional live requested storage is zero at
the observation point; the original 229,393-byte input allocation is still live.
Sharing retains that cell's capacity, including surplus capacity if present.
Empty output uses the shared static empty cell plus normal Result bookkeeping.

## Boundary and Perceus ownership

Validation stays in Plum and returns a typed Result containing the escape count.
This keeps error construction and enum layout under the compiler's normal rules.
A fully native Result-returning decoder would duplicate compiler-assigned enum
representation. A single native status/error protocol would introduce another
error interface; neither is needed here.

```text
scan original encoded bytes, count valid triplets
  first incomplete or nonhex escape → ordinary Err with its byte offset
  no escapes → owned reference to unchanged input
  escapes → checked exact Bytes constructor → ordinary Ok
```

The shared hex validator remains the authority for syntax. The runtime writer
only converts already validated hex digits using their nibble arithmetic; it
does not invent another public parser or error precedence. `%G` is truncated,
`%GG` is invalid, the first bad escape wins, and offsets count encoded UTF-8 bytes.
Literal `+` stays plus, raw NUL/Unicode bytes are preserved, and `%2520` decodes
once to `%20`. Escaped output can contain invalid UTF-8 and remains Bytes.

The constructor borrows input and returns a +1 owner. For unescaped nonempty input
it increments the normal reference count. For escaped input it allocates a fresh,
exact `decoded_length + 17` ByteStorage cell through the existing checked
allocator. Input is never mutated, including a unique input with spare capacity.
The scalar guard validates the stored input length and checks
`0 <= escapes <= input_length / 3`; `input_length - 2 * escapes` then cannot
underflow, overflow, or exceed the header limit. The writer checks output bounds,
triplet bounds, and the final length. Lexical validity and the exact escape count
remain private caller preconditions. Invalid sizes and allocation failures use
the existing fatal path; malformed public input remains a typed Err.

This follows Plum's existing Perceus ARC and copy-on-write conventions. Shared
input/output/CStr aliases prevent unsafe in-place changes and release through
the existing allocator. Sharing an unchanged whole cell avoids a copy while
retaining its original capacity. An escaped result copies only decoded bytes;
it cannot keep a larger encoded buffer alive through an interior slice.

Consuming input reuse could remove the escaped-output allocation when a proven
last use owns the input. It would also retain the larger encoded allocation
(up to three input bytes per decoded byte) and needs an explicit compiler
consuming boundary. A reference count of one alone does not authorize mutation
of borrowed input. The compact borrowed constructor uses the established ABI
without adding a separate ownership rule or changing cell layout.

## Larger memory impact and related behavior

The primary benefit is in **compiled programs calling percent decoding**,
including URL component decoding and LSP file paths. Ordinary compiler emission
does not exercise URI decoding; its separate before/after results below cover
module-loading and emitted-runtime overhead rather than URL throughput.
Requested allocation traffic, additional live storage, whole-process requested
peak, and process RSS are different metrics. The observer keeps input live and
checks every output byte or exact diagnostic without allocating; its snapshot
occurs inside the matched Result arm after temporary Result cells may be freed.
No timing or RSS improvement is inferred from requested-byte totals.

Related [#234](https://github.com/bradcypert/plum/issues/234) already established
URL percent validation in all components, and
[#160](https://github.com/bradcypert/plum/issues/160) fixed LSP file-URI escaping.
Their grammar, errors, path handling and representation remain intact. This
optimization serves their shared Encoding dependency without claiming to reopen
or fix either completed bug.

## Reproduction and safeguards

```sh
python3 bootstrap/measure-percent-decode.py /path/to/before /path/to/after \
  --out /tmp/percent-decoding-measurements.json
python3 bootstrap/string-growth-check.py /path/to/after --sanitize
bootstrap/property-check /path/to/after
PLUM=/path/to/after bootstrap/alloc-check
```

[Program measurements](percent-decoding-measurements.json) include 210 rows:
three approaches, debug/release, five sizes, and seven valid/error workloads.
A frozen original decoder compares exact public results for 11,111 short strings
over a ten-symbol alphabet containing `%`, hex/nonhex digits, plus, NUL and
Unicode. Additional checks cover all 256 bytes with upper/lowercase hex, mixed
and adjacent escapes, invalid UTF-8 output, large buffers, single-pass decoding,
and String/Bytes/CStr aliases after input or output rebindings/appends.

Emitted-runtime tests exercise actual compiler output at O0/O2 with and without
memory tracking and ASan/LSan. Native checks cover mixed literals/all triplets,
NUL/high bytes, immutable unique input with spare capacity, identity sharing and
last-owner release, static empty output, exact allocation requests, ByteStorage
accounting, near-limit arithmetic, header-only size/count failures and injected
allocation failure before payload reads. Public property tests use only public
APIs. The refreshed seed is rebuilt to the same source compiler and checked for
bootstrap consistency; source documentation is regenerated with that compiler.

Allocation budgets for 1,000 calls include Result/error bookkeeping:

| Workload | Allocations | Requested bytes |
| --- | ---: | ---: |
| `a%00%FF+` | 2,000 | 45,000 |
| Unescaped `abc+é` | 1,000 | 24,000 |
| Invalid `abc%GG` | 3,000 | 108,000 |

## Compiler measurements

[Separate compiler measurements](percent-decoding-compiler-measurements.json)
use the refreshed seed-to-source compiler and its predecessor on identical final
sources, with one warmup and two recorded samples per mode:

| Compiler emission | Before requested bytes | After requested bytes |
| --- | ---: | ---: |
| Default/debug | 1,076,612,993–1,076,612,994 | 1,076,612,797 |
| Release | 909,321,709–909,414,973 | 909,446,002–909,446,003 |

Debug traffic is effectively unchanged (196–197 bytes lower); release traffic
increases by about **31–124 KB** in these samples. These compiler costs include
loading the changed Encoding module and emitting the larger runtime. They are
separate from the decoder workloads and are not compiler memory savings. RSS
ranges overlap and no RSS or timing improvement is claimed. Samples ran alongside
other validation, so their elapsed times are deliberately omitted. Compiler memory
guards continue to enforce the existing budgets.

To reproduce the compiler comparison on Linux:

```sh
python3 bootstrap/compiler-profile.py --before /path/to/before --after /path/to/after \
  --case emit-compiler --case emit-compiler-debug --samples 2 --out /tmp/percent-compiler-profile
```
