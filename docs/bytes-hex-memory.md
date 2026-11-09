# Exact-size byte-to-hex construction

`Bytes.to_hex` and `Encoding.hex_encode` share one implementation. Each input
byte always produces two lowercase ASCII bytes. Building an `Array[String]`
of digit fragments therefore spends memory on pointers and array growth
before allocating an already predictable result.

## Alternatives considered

A bounded experiment compared the existing fragment array, a pure Plum owned
String accumulator, and a direct exact-size fill. All three passed an oracle
covering all 256 byte values, empty input and repeated binary inputs, in debug
and release modes. The accumulator used the existing compiler-selected Perceus
concat reuse and a shared sixteen-digit String table. The direct-fill prototype
replaced only the measured wrapper's emitted LLVM; it was exploratory and did
not contain production size/failure handling. Production evidence below uses
ordinary compiler builds without any LLVM substitution.

For 262,144 input bytes (`a`, `é`, NUL, `🙂`, repeated 32,768 times), both modes
reported the same requested-memory figures:

| Approach | Construction allocations | Cumulative requested bytes | Retained result request | Whole-process peak requested bytes |
| --- | ---: | ---: | ---: | ---: |
| Fragment array | 19 | 13,360,673 | 524,305 | 9,895,857 |
| Owned String accumulator | 26 | 1,983,045 | 663,507 | 1,367,991 |
| Exact-size fill | 1 | 524,305 | 524,305 | 786,466 |

The accumulator is a substantial improvement using ordinary ownership reuse,
but has repeated growth/copy work and retains spare capacity. Exact construction
uses the known final size and avoids both costs. Extending this into a public
mutable builder, writable views, or a new Bytes representation would add an
ownership contract that hex encoding does not require.

## Production contract

The private runtime constructor borrows its Bytes input and returns a +1 String.
Nonempty output requests exactly `2 * input_length + 17` bytes: the unchanged
16-byte cell header, ASCII payload and trailing NUL. Empty output uses an
immortal static cell. The caller's input stays unchanged, including when uniquely
owned with enough spare capacity to hold the result.

Repetition and hex share checked scaled-length arithmetic. Signed multiplication
and header limits are validated before allocation or byte offsets. Each caller
supplies its own allocation-free fatal diagnostic; repetition keeps its existing
behavior and message. Hex uses the existing allocator and ByteStorage accounting.
The fill uses byte loads, nibble indexes into a sixteen-byte table and byte
stores, with no endian dependency or temporary managed objects.

This preserves Perceus reference counting, release insertion and uniqueness-based
reuse. It improves a known-size constructor; it does not introduce a second
ownership system or make all encoding operations allocation-free. JSON's
separate digit helper remains because JSON escaping still uses it.

## Memory impact

The main benefit is in **programs produced by Plum**: each nonempty hex conversion
has one result allocation and no fragment arrays. Logical output remains two
bytes per input byte; the retained nonempty result is the same exact size as
before. Empty results avoid the previous 17-byte allocation. Requested traffic
and process peak are measured separately; neither establishes a reduction in RSS.

The compiler also calls hex encoding for filename identities, package hashes and
build-cache serialization. The seed is refreshed, then rebuilt through the
seed-to-source path so the distributed compiler actually runs the new constructor.
Compiler emission measurements are recorded below separately from program figures.

## Reproduction and safeguards

```sh
python3 bootstrap/measure-bytes-hex.py /path/to/before /path/to/after \
  --out /tmp/bytes-hex-measurements.json
python3 bootstrap/string-growth-check.py /path/to/after --sanitize
PLUM=/path/to/after bootstrap/alloc-check
bootstrap/property-check /path/to/after
```

The measurement harness verifies every output byte, length and NUL through an
allocation-free native observer. Setup and input allocations are excluded from
construction traffic/live deltas but included in the whole-process peak. Input
stays live across observation; the result's CStr keeps its owner alive. Both
debug and release builds are measured without modifying emitted LLVM.

The shared storage guard exercises actual emitted runtime functions at O0/O2,
with/without heap accounting and optionally ASan/LSan. It checks exact requests,
ByteStorage accounting through release, borrowed unique input with spare
capacity, empty output, multiplication/header overflow and injected allocation
failure. Public fixtures independently check the expected encoding of all 256
byte values, immutable String/Bytes/CStr aliases and larger binary input. Public
properties check concatenation order, doubled length and the lowercase alphabet.

## Production measurements

[Program measurements](bytes-hex-measurements.json) confirm the exact-fill row
above for both modes, using unmodified before/after compiler binaries. At 32,768
input bytes, traffic falls from 1,662,641 to 65,553 requested bytes and process
peak from 1,236,913 to 98,338 bytes. The 65,553-byte result request is unchanged.
An empty conversion falls from one 17-byte allocation to no allocation.

[Compiler measurements](bytes-hex-compiler-measurements.json) use the same final
source tree with both compilers, after a warmup:

| Compiler emission | Before requested bytes | After requested bytes | Reduction |
| --- | ---: | ---: | ---: |
| Default/debug | 1,078,067,127 | 1,077,771,839 | 295,288 |
| Release | 911,422,472 | 911,265,130 | 157,342 |

These are cumulative allocation traffic while compiling, not memory saved by
every produced executable. The default/debug and release compiler processes
respectively reached 131,516 / 131,452 KiB and 116,520 / 116,392 KiB RSS before /
after in the recorded sample. These small differences do not establish an RSS
improvement, and no timing claim is made.

The related historical [issue #274](https://github.com/bradcypert/plum/issues/274)
was closed as a false-positive stack claim after its generated tail calls were
reviewed. This change targets temporary storage, not stack exhaustion, and does
not reopen or claim to fix that issue.
