# Reusable radix formatting in Plum

`Int.to_radix` now builds least-significant digits in one owned accumulator,
appending through existing Perceus String reuse, then reverses the digits into
compact final storage. The shared digit selector returns immutable String
literals instead of constructing a temporary Array[Int], Bytes cell and Result
for each digit. The same selector also serves `Int.to_hex_bits` and decimal
rounding inside `Float.to_fixed`.

This is a production prelude change. No runtime intrinsic, allocator, cell
layout, public syntax or ownership ABI is added. Compiler output for the
accumulator contains `plum_str_concat_reuse` and a guaranteed `musttail` call.

## Related decisions

Searches covered radix, integer/float formatting and allocation. Completed
[#23](https://github.com/bradcypert/plum/issues/23) established these formatting
functions. Open [#33](https://github.com/bradcypert/plum/issues/33) defers
interpolation specifier syntax pending application usage; this change stays
inside the existing functions and does not close it. Completed
[#267](https://github.com/bradcypert/plum/issues/267) established/documented
Float.to_fixed's 0..17 precision clamp; that limit and its half-even rounding
decisions are unchanged.

## Alternatives and focused comparison

The measurement harness can reproduce source prototypes with the before
compiler. Both prototypes preserve radix/sign semantics and are verified by an
independent unsigned-arithmetic C oracle, including Int::MIN.

| Approach | Int::MAX binary allocations / requested traffic | Other tradeoff |
| --- | ---: | --- |
| Previous computed digits and prepending | 245 / 7101 bytes | Temporary Array, Bytes and Result per digit |
| Static digits, retaining prepend accumulation | 63 / 3087 bytes | Removes digit construction but continues copying growing suffixes |
| Static digits, append and reverse | 5 / 252 bytes | Reuses the accumulator, then makes one final copy; selected |
| Eager global chars_of digit table | Not selected | Historically added 36 character Strings plus an Array at startup, including unused programs |
| Native exact count/write formatter | Not prototyped | Could reduce allocation calls further, while adding a formatting kernel/primitive; selected source design uses existing tested primitives |

These example numbers are Linux requested-byte measurements with ordinary
allocator rounding. Counts vary when malloc exposes less spare usable capacity.
The static-only prototype requests 37 bytes for hexadecimal 255, versus 38 for
the selected approach; both use two allocations there. The selected design
avoids a size-dependent strategy threshold for this one-byte difference while
substantially reducing long conversions and avoiding allocation entirely for
nonnegative single digits. The before implementation requests 132 bytes in six
allocations for that same two-character result.

The production alphabet has 36 finite literal values in static module data,
with no eager heap table or per-call digit cells. The unused-formatting and
single-digit allocation guards both stay at zero. Static data is a different
cost from the heap traffic reported below; zero heap allocation does not mean
the digit literals occupy no process data.

## Signed arithmetic and result ownership

Radix still clamps to 2..36. Zero returns `"0"`. For a nonzero value, compute
`q = n / r` and `tail = n - q*r`, and select the absolute value of the last
digit. Dividing toward zero means `q*r` stays between n and zero, and the
remainder's magnitude is below r. Negating the quotient and remainder is safe
even for Int::MIN, since r is at least two. The original input is never negated
as a whole.

When q is zero, nonnegative output returns the literal digit and negative
output prepends a minus sign directly. Otherwise, the remaining nonnegative
magnitude is divided repeatedly, appending its digits after the seed digit.
For a negative number the minus sign is appended last, so one reversal puts it
at the beginning. The completed accumulator has at most 65 ASCII bytes.

The accumulator is never exposed to callers. Its last use transfers it through
the existing consuming append path; final reversal borrows it while copying,
and normal releases reclaim it afterward. The result is owned, and normal
String/Bytes/CStr aliases and copy-on-write updates remain immutable.
Nonnegative single digits share immortal literal cells; multi-digit radix
results use an exact requested-size final cell. `Int.to_hex_bits` keeps its
existing append loop and gains static digits; `Int.to_bits` is unchanged and
is measured as a control.

## Produced-program memory results

```sh
python3 bootstrap/measure-radix-formatting.py BEFORE AFTER --compare-designs \
  --out docs/radix-formatting-measurements.json
```

The artifact contains 300 rows across ordinary debug/release builds, values,
radices (including clamped inputs), production APIs and the two source
prototypes. No emitted LLVM is substituted. Parsing is checked before the
observation, and an allocation-free C observer verifies every byte, length and
terminating NUL using unsigned arithmetic. Explicit half-even examples verify
rounding without depending on libc's tie policy. Traffic/live deltas exclude
setup; process requested heap peak includes setup.

Both build modes agree on these examples:

| Operation | Before allocations / requested traffic | After | Result requested live bytes before → after |
| --- | ---: | ---: | ---: |
| Radix 10, value 1 | 3 / 66 bytes | 0 / 0 bytes | 18 → 0 |
| Radix 16, value 255 | 6 / 132 bytes | 2 / 38 bytes | 18 → 19 |
| Radix 2, Int::MAX | 245 / 7101 bytes | 5 / 252 bytes | 80 → 80 |
| Radix 2, Int::MIN | 250 / 7330 bytes | 5 / 254 bytes | 82 → 82 |
| Radix 10, Int::MAX | 69 / 1623 bytes | 3 / 82 bytes | 36 → 36 |
| Hex bit pattern, Int::MAX | 50 / 1101 bytes | 2 / 45 bytes | 27 → 27 |
| Fixed rounding of 1.5, precision 0 | 50 / 1091 bytes | 47 / 1025 bytes | 18 → 18 |

The Int::MAX binary case saves 6849 cumulative requested bytes per call; it
still returns an 80-byte requested result cell. Its measured process peak is
177 → 155 bytes, not a 6849-byte RAM saving. Many small cases have the same
peak because argument/setup storage dominates. For short strings, a new exact
result can request a byte or two more than an older reused cell's original
request: the allocator rounded that old cell up, and diagnostics preserve its
original requested size as reuse fills its usable capacity. This does not
establish an RSS difference.

## Guard portability

The binary guard permits at most 12 allocations / 600 requested bytes per
Int::MAX conversion; the hex-bits guard permits 10 / 300. These are deliberate
ceilings rather than a snapshot of Linux's rounded capacities. With ASan,
which exposes exactly the requested allocation size, the observed counts for
1000 calls are 11,000 / 439,000 bytes and 8000 / 193,000 bytes respectively.
Ordinary Linux allocation uses 5000 / 252,000 and 2000 / 45,000. Both fit the
same guards; restoring per-digit heap construction or exact-copy-per-append
behavior exceeds them. Single-digit and unused-formatting guards require zero.

Existing String growth/reversal bounds and OOM fallback remain responsible for
buffer construction. This change adds no new low-level failure path. Public
properties decode into a negative accumulator to round-trip the full Int range
in every base without attempting to negate Int::MIN. The new corpus fixture
checks all bases and clamp extremes against a separate native oracle, plus
partial applications, fixed-width bit patterns, rounding and String/Bytes/CStr
aliases kept alive across appends.

## Compiler impact and larger memory story

`docs/radix-compiler-measurements.json` compares the merged #495 compiler with
one rebuilt through the refreshed seed, using identical final sources verified
by recorded hashes, one warmup and two alternating measured samples per mode.

| Compiler operation | Before requested traffic | After |
| --- | ---: | ---: |
| Release LLVM emission | 880,892,995–880,892,996 bytes | 881,045,977–881,045,978 bytes |
| Debug LLVM emission | 1,047,851,632 bytes | 1,048,034,000–1,048,034,001 bytes |

Compiler traffic increases by about 153 KB in release (0.0174%) and 182 KB in
debug (0.0174%). The measurements include parsing/construction of the larger
prelude body and static digit cases; they do not establish a compiler-memory
or RSS reduction. Runtime buffer and ownership machinery are unchanged.

The intended benefit is less allocation churn in produced programs that use
radix/hex-bit formatting, plus removing avoidable digit-cell construction from
fixed rounding. Formatting an Int is bounded to at most 65 output bytes, so
thousands of saved cumulative bytes per call are not thousands of bytes of
peak memory saved. Final multi-digit strings generally have the same footprint;
nonnegative single-digit results use static storage. No new ARC or FBIP pass
is introduced: the source algorithm makes the accumulator the reusable left
operand of existing String append. This trades a bounded reversal copy for
many fewer temporary cells and prefix copies, without changing ownership.

The actual regression fixture also cross-compiles for Windows x64 and macOS
arm64 and runs under QEMU on Linux arm64/musl. These checks supplement native
CI; they do not claim local execution on Windows/macOS. Existing formatting
and precision fixtures/properties remain in the corpus with their recordings
unchanged.
