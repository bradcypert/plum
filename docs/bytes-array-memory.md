# Exact-sized Bytes to Array[Int] conversion

`Bytes.to_array` knows its result length before it starts. Previously it pushed
one byte at a time into a growing Array, using the existing Perceus reuse and
capacity policy. That was already an amortized linear traversal; this change
does not claim a quadratic or recursive-stack defect. It replaces growth and
capacity copying with one checked allocation and direct unsigned widening.

No open issue specifically describing `Bytes.to_array` was found in searches
for to_array, array overflow, and Bytes allocation. Related completed work:
[#16](https://github.com/bradcypert/plum/issues/16) established Bytes as a
separate binary type, and [#437](https://github.com/bradcypert/plum/issues/437)
established opt-in allocation diagnostics. This change preserves both layouts
and reports result storage with the existing Array origin.

## Design comparison

| Approach | Tradeoff | Decision |
| --- | --- | --- |
| Existing push loop | Reuses unique Array storage, but grows and copies it despite knowing the final length | Replaced for this conversion |
| General public Array reserve/builder API | Could support more constructors; adds generic initialization and ownership surface | Not necessary for this existing API |
| Private exact constructor returning Array[Int] | Uses the current 64-bit Int slots, one pass, no callbacks or element boxes | Selected |
| Lazy Byte-to-Int view | Avoids widening storage, but changes the public Array representation/indexing semantics | Separate representation design |
| Consume and widen a unique Byte cell in place | Needs enough capacity and a consuming ownership/retagging contract | Separate compiler/runtime design |

The private prelude stub follows existing raw primitive lowering. Its input is
borrowed and its output is owned. It does not mutate input, even when unique,
and it never aliases ByteStorage as Array storage. This preserves normal
String/Bytes/CStr aliases and Array copy-on-write behavior.

The physical Array[Int] layout is unchanged: 16-byte RC/length header followed
by eight-byte unboxed Int slots. Each input byte is zero-extended to Int, so
0x80..0xFF become 128..255, and NUL is an ordinary zero element. There is no
Array terminator. The result still uses roughly eight times its input byte
length; this optimization removes growth traffic and spare capacity, not the
representation's inherent width.

## Checked size and allocation policy

`plum_bytes_array_size(n)` rejects negative lengths and lengths above
`floor((Int::MAX - 16) / 8) = 1,152,921,504,606,846,973`. Only then does it form
`8*n + 16`. Thus multiplication, header addition and every element offset fit
before allocation or payload access. Scalar tests cover the largest valid
request without attempting that allocation, and header-only fault tests cover
negative lengths, multiplication overflow and header overflow.

A nonempty result requests exactly `8*n + 16` bytes with Array provenance.
The fill loop reads each byte once and writes each Int once, without recursion,
per-element stack allocations or heap boxes. Empty input returns an immortal
zero-length Array constant; ordinary Array updates already handle immortal
references and allocate independent storage when needed.

The existing preferred/minimum allocator now delegates to a shared
`plum_alloc_preferred_or_fail` implementation with a caller-selected fatal
message. String callers keep their existing signature, messages, reserve retry,
counters and origins. Bytes conversion selects `Plum Bytes: cannot allocate
array storage` instead of reporting a String allocation error. Both entry
points share the same allocation policy and successful-cell registration.
This is an internal diagnostic parameter, not a public allocator or ownership
redesign. The existing allocation-failure suite checks the retained String
fallback behavior as well as the new Bytes error. OOM is tested with a header
only: it must fail before reading any input bytes.

## Perceus and the larger memory story

The old push loop already benefited from uniqueness-driven reuse. Knowing the
final size lets this constructor avoid growth entirely; no new Perceus analysis
or FBIP pass is introduced. Existing last-use releases and Array element-aware
release remain responsible for lifetimes. Array[Int] owns scalar elements, so
there is no child heap storage to retain or release for each value.

The result owns no reference to the original Byte cell. Dropping the input can
release ByteStorage while the Array remains live, including when the input had
large reserved capacity. Empty conversion also retains no oversized input cell.
Tests pin the physical Array origin and its final release back to zero live
bytes independently of ByteStorage.

An exact request does not mean allocator usable capacity equals length; the
allocator can round up. Removing intentional spare capacity can make later
appends grow sooner than they did after the old conversion. General Array
growth and copy-on-write rules are unchanged. This tradeoff favors compact
known-size conversion over speculative reserve for future appends.

## Produced-program measurement

```sh
python3 bootstrap/measure-bytes-array.py BEFORE AFTER \
  --out docs/bytes-array-measurements.json
```

The harness builds ordinary programs using each compiler, with no LLVM
substitution. An allocation-free native observer snapshots immediately after
conversion; subsequent assertions verify every numeric element against an
independent pattern. Both input and output stay live. Setup and verification
are excluded from traffic/live deltas; peak requested heap includes setup.
Array-origin live bytes are recorded separately. These are requested byte
counts, not RSS, allocator usable size or throughput measurements.

The source fixture and properties exercise all 256 values, truncating
Bytes.from_array round trips, method/namespaced/first-class calls, empty result
mutation, String/Bytes/CStr input aliases, independent Array aliases/updates,
and a large output. The emitted native guard checks 514 unique/shared
conversions across lengths 0..256, validates every widened value, exact requests,
source immutability, Array vs ByteStorage origins, final-owner lifetimes, OOM,
near-limit arithmetic, and a million-element output at O0/O2 with and without
heap tracking. Existing String failure/fallback tests also run through the
shared allocator implementation.

## Measured results

`docs/bytes-array-measurements.json` contains 60 rows across debug/release,
three byte patterns, and five sizes. Both modes agree on these largest cases:

| Input bytes / result Ints | Before allocations / requested traffic | After allocations / requested traffic | Array live bytes before → after | Process requested heap peak before → after |
| --- | ---: | ---: | ---: | ---: |
| 262,144 Unicode/NUL | 17 / 6,413,888 | 1 / 2,097,168 | 3,211,216 → 2,097,168 | 5,078,961 → 2,359,329 |
| 131,072 ASCII | 16 / 3,202,672 | 1 / 1,048,592 | 1,605,584 → 1,048,592 | 2,539,441 → 1,179,681 |
| 32,768 zero bytes | 14 / 794,320 | 1 / 262,160 | 401,360 → 262,160 | 630,737 → 294,945 |

The 256 KiB example saves 4,316,720 cumulative requested bytes per conversion
(about 4.32 MB), 1,114,048 bytes of retained Array capacity, and 2,719,632 bytes
of measured peak requested heap. Its 2,097,168-byte output still contains the
same 262,144 Int values. Empty conversion remains zero-allocation in both
implementations, with small process peak dominated by setup.

## Compiler measurements

`docs/bytes-array-compiler-measurements.json` compares the merged #494 compiler
with a compiler rebuilt through the refreshed seed, on identical current
sources verified by recorded hashes. One warmup precedes two alternating
measured samples per mode:

| Operation | Before requested traffic | After |
| --- | ---: | ---: |
| Release LLVM emission | 881,065,368 bytes | 880,794,282 bytes |
| Debug LLVM emission | 1,048,035,529–1,048,035,531 bytes | 1,047,847,627 bytes |

These are small compiler traffic decreases: 271,086 bytes in release and
187,902–187,904 bytes in debug. They include module/prelude/runtime construction
changes, not just execution of conversion calls. The current compiler's lexer
uses conversion for unexpected-character diagnostics; the larger measured
benefit is to produced programs that convert substantial byte buffers. No
compiler RSS or speed improvement is inferred from these samples.

The emitted native guard also compile/links for Windows x64 and macOS arm64
and passes at O0 under QEMU with a static Linux arm64/musl build. These are
additional checks; native Windows/macOS execution remains for CI.
