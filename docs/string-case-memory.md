# Checked case conversion with shared unchanged results

`String.to_upper` and `String.to_lower` retain their existing platform
one-codepoint Unicode mapping. This also resolves the documentation mismatch
in [#120](https://github.com/bradcypert/plum/issues/120): these operations have
never been limited to ASCII in the generated runtime. They do not perform
multi-codepoint expansions or full Unicode case folding. POSIX uses the
initialized UTF-8 locale; Windows maps the Basic Multilingual Plane and leaves
supplementary codepoints unchanged. OS Unicode tables can differ.

## Alternatives

| Approach | Allocation and ownership | Decision |
| --- | --- | --- |
| Keep copying every result; only move decoder slots | Constant stack, but unchanged results still allocate and duplicate bytes | Leaves avoidable traffic in place |
| Share unchanged input; one common checked count/fill routine | Zero result allocations for identity, one exact cell for changed output; borrowed input, owned result | Selected; fits current Perceus ABI |
| Consume uniquely owned input and map in place | Could also avoid changed-result allocations when output fits | Requires a consuming compiler/runtime seam, capacity rules, and handling changing UTF-8 widths; a separate design discussion |
| Reserve a maximum-size buffer before mapping | One mapping pass, but oversized storage or a later compacting copy | Trades away the exact-storage property |
| Introduce platform-independent full Unicode tables | New mapping/versioning semantics and expansions | A different language/API decision, unnecessary for this ownership fix |

Both directions delegate to `plum_str_case`; only the platform mapping direction
differs. The routine checks the input length before reading bytes, counts the
exact output length with checked addition, and records whether any codepoint
changes. Empty output uses immortal static storage. An unchanged nonempty
result increments the input reference. A changed result gets one exact-sized
ByteStorage allocation and never mutates the input, even when it is unique.

The input contract remains a valid UTF-8 String. A bounded width check also
rejects a truncated final codepoint before the decoder reads beyond its input.
This is not a new arbitrary-byte-to-String decoder. Count/fill arithmetic and
header overhead are checked; the fill pass checks space before every encoded
write and requires the final length to match. Thus a process locale change
between passes cannot turn a changed mapping width into an out-of-bounds write.
It does not promise a consistent linguistic result under concurrent locale
changes.

The decoder scratch slot now lives in the entry block and is reused by both
loops. Previously each loop iteration executed `alloca i64`; at `-O0`, those
allocations accumulated until the function returned. Extracting the previous
compiler's actual emitted `plum_str_to_upper` and UTF-8 helpers and running them
at `-O0` on 1,048,576 ASCII characters with an 8 MiB stack reproduced SIGSEGV.
The C driver supplied ordinary storage and an ASCII mapping shim, isolating
the emitted converter's stack behavior. The current native guard exercises a
million-character changed conversion at both `-O0` and `-O2`, using the actual
emitted routines and platform compatibility shim.

## Produced-program measurements

Reproduce with ordinary compilers, without replacing emitted LLVM:

```sh
python3 bootstrap/measure-string-case.py BEFORE AFTER \
  --out docs/string-case-measurements.json
```

The allocation-free C observer verifies every output byte, including embedded
NUL and UTF-8, the byte length, and terminating NUL. Input remains live after
the result is observed. Traffic and additional live bytes exclude setup;
peak requested heap covers the whole process. Debug and release agree on the
following 262,144-byte inputs (32,768 repetitions of an eight-byte unit):

| Operation | Before result allocations / requested bytes | After | Requested process heap peak before → after |
| --- | ---: | ---: | ---: |
| Unchanged uppercase | 1 / 262,161 | 0 / 0 | 524,322 → 262,161 |
| Unchanged lowercase | 1 / 262,161 | 0 / 0 | 524,322 → 262,161 |
| Changed uppercase | 1 / 262,161 | 1 / 262,161 | 524,322 → 524,322 |
| Changed lowercase | 1 / 262,161 | 1 / 262,161 | 524,322 → 524,322 |

The measured changed strings have the same encoded length as their input.
Native tests also cover mappings which shrink and grow UTF-8 on platforms
that support those mappings. Empty conversion drops the previous 17-byte
result allocation; process peak in the small measurement remains dominated
by setup. These are requested heap byte measurements, not allocator usable
size, stack bytes, RSS, or throughput. The separate stack regression is not
included in the heap totals.

## Perceus and the larger memory story

This improves programs emitted by Plum, including a source-built compiler
when it calls these methods. It does not add a compiler reuse analysis pass.
The existing borrowed-argument/owned-result convention, reference counting,
last-use releases, cell layout, and consuming append paths remain intact.

Sharing avoids both allocation traffic and a second live byte buffer when
the caller keeps its input. It can also retain spare capacity: a one-byte
String with capacity 4096 occupies a requested 4113-byte cell. After an
identity conversion and release of the original caller reference, the result
still owns all 4113 bytes until its final release. The native guard pins this
tradeoff and verifies final release returns the tracked live bytes to zero.
An alias can also prevent an append from using unique ownership. Changed
conversion still compacts into one exact-sized independent result, while an
empty result never retains an oversized empty input cell.

The optimization therefore reduces unnecessary copies without claiming all
programs retain less memory. Constant decoder stack space is a separate
correctness improvement. Compiler allocation traffic is measured separately
below; no RSS or speed improvement is inferred from requested byte counts.

## Verification

The historical `string_case` fixture and its output remain in place, extended
with method/namespaced/first-class calls, embedded NUL, Unicode, empty results,
large changed input, and immutable String/Bytes/CStr aliases. The existing
ASCII properties remain; a public API storage-semantics property adds Unicode
and NUL coverage. Allocation fixtures bound identity and empty results at zero
and 1000 changed eight-byte results at 1000 allocations / 25,000 requested bytes.

The storage guard extracts actual emitted LLVM, uses the real platform shim,
and tests 8192 conversions across special and randomized valid codepoints,
including supplementary characters. Its independent UTF-8 encoder and platform
mapping oracle verify bytes, exact requests, identity references, input
immutability, retained capacity, static empties, final releases, invalid
headers, truncated input, near-limit scalar sums, and allocation failure.
Deterministic mapping-shim fault injection also changes the mapping width
between passes, testing both the pre-write bound and final-length refusal.
Invalid header tests have no payload; allocation failure uses valid input
because the counting pass necessarily reads it before allocation.

## Compiler measurements

`docs/string-case-compiler-measurements.json` records a refreshed-seed-to-source
compiler comparison on identical current inputs, with one warmup and two
alternating measured samples per mode. The ordinary source-built before
compiler is the merged #492 implementation; the after compiler is rebuilt
through the new checked-in seed.

| Compiler operation | Before requested allocation traffic | After |
| --- | ---: | ---: |
| Release LLVM emission | 880,142,749 bytes | 880,152,763–880,152,765 bytes |
| Debug LLVM emission | 1,046,899,089–1,046,973,644 bytes | 1,046,865,354–1,046,865,355 bytes |

Release traffic increases by about 10 KB (0.0012%); debug traffic decreases
by about 34–108 KB. This is not a material compiler-memory saving claim.
The larger runtime and corrected embedded documentation are included in the
comparison. The clear measured benefit is produced-program identity
conversion; the existing URL parsing allocation fixture also drops from
8,523,646 to 8,523,446 allocations, and its threshold is tightened accordingly.
RSS varies across samples and is recorded without attributing an improvement.

The native guard also compile/links for Windows x64 and macOS arm64, and
passes at `-O0` under QEMU with a statically linked Linux arm64/musl build.
Those cross checks supplement native CI; they do not claim native execution
on Windows or macOS. The whole compiler/shim cross-check passes all four
supported test targets and executes its Linux arm64 smoke test under QEMU.
