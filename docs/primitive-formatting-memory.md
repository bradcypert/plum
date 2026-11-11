# Primitive formatting storage

Integer and float formatting used two allocations: raw temporary storage for
the C formatter, then a compact String cell copied from that storage. The
temporary was freed correctly, but every conversion still visited the heap
twice. Boolean formatting copied one of two fixed words into a new cell.

## Design and alternatives

Numeric formatting now reserves its existing bounded scratch in the function's
entry block: 32 bytes for Int, 64 for general Float, and 512 for the raw fixed
expansion. The formatter and its arguments are unchanged. `plum_str_new` copies
the bytes into the same compact owned result before the function returns.
Scratch cannot escape, is private to each invocation, and does not accumulate
with loop iterations. No global buffer, thread-local allocation, new primitive,
or ownership representation is introduced.

A signed Int needs at most 20 characters plus NUL. General Float formatting
uses at most 17 significant digits plus sign, point, exponent and NUL. The
prelude's fixed expansion requests 18 fractional digits; even the largest
finite double has only 309 integer digits, fitting comfortably in 512 bytes.
These are separate call frames, with 512 bytes the largest scratch buffer;
they are not input-sized stack allocations. This does not extend the private
fixed formatter to arbitrary precisions.

`true` and `false` use two static String cells with reference count -1, exactly
the existing literal convention. They need no eager heap initialization.
Retain/release skips immortal cells; consuming append copies them through the
existing Perceus path. Public String, Bytes and CStr aliases remain immutable.

| Alternative | Evidence and tradeoff |
| --- | --- |
| Existing heap scratch plus compact copy | Two numeric allocation requests per conversion; correct compact ownership. |
| Bounded local scratch plus compact copy | Selected. One numeric allocation request, identical result size and formatting behavior. |
| Format directly into maximum-sized result storage | Prototyped against emitted LLVM. Removes the copy but retains 48/80/528-byte cells even for tiny results. A fixed expansion of 1.0 retains 528 bytes instead of 37. |
| Count, allocate exactly, then format again | Avoids scratch and copy, but repeats formatting; shortest-round-trip Float already tries up to three precisions. No measured evidence justifies that extra conversion work. |
| Shared global or cached thread-local scratch | Adds synchronization, recursion or retained-storage concerns for at most 512 bytes of bounded temporary data. |
| Replace decimal conversion algorithms | Broader semantic work. The existing Float exponent normalization, round-trip behavior and Plum half-even rounding remain authoritative. |

This follows completed #23's shared conversion path and #267's documented
0..17 public fixed-precision limit. Open #33 still defers interpolation format
specifier syntax; this work changes storage behind existing operations.

## What the numbers mean

`PLUM_RT_STATS` and `Memory.snapshot` count Plum cells, not the former raw
numeric scratch. Looking only at those counters would hide the numeric
improvement. The focused harness extracts the actual emitted formatters and
their allocator/release helpers, then observes their requests at test-only
boundaries. It includes raw scratch and result storage, excludes registry
indexing and allocations internal to libc, and validates every result and
final release. No production formatting algorithm is replaced for before or
after. The direct-result alternative is explicitly a throwaway LLVM prototype.

```sh
python3 bootstrap/primitive-format-check.py AFTER --before BEFORE \
  --out docs/primitive-formatting-measurements.json
python3 bootstrap/primitive-format-check.py AFTER --sanitize
```

The committed artifact contains 72 rows covering three designs, tracked and
ordinary allocation, and -O0/-O2. All modes agree on these requested-byte
measurements:

| Operation | Heap requests before → after | Cumulative requested heap bytes | Result live bytes |
| --- | ---: | ---: | ---: |
| Int::MIN.to_string | 2 → 1 | 69 → 37 | 37 → 37 |
| Float 0.1 + 0.2, shortest round trip | 2 → 1 | 100 → 36 | 36 → 36 |
| Raw fixed expansion of 1.0 at 18 places | 2 → 1 | 549 → 37 | 37 → 37 |
| Raw fixed expansion of maximum finite Float | 2 → 1 | 857 → 345 | 345 → 345 |
| true.to_string | 1 → 0 | 21 → 0 | 21 → 0 |
| false.to_string | 1 → 0 | 22 → 0 | 22 → 0 |

Numeric isolated conversion peak requested heap equals the traffic in this
table: scratch and result used to overlap until the copy completed. Result
sizes stay identical. Scratch now occupies bounded stack storage instead of
heap storage; a 512-byte heap reduction is not a 512-byte reduction in total
memory or RSS. The Boolean words occupy static module data, not zero physical
space. Allocator rounding, stack pages and libc internals are outside these
requested-byte counts.

`Float.to_fixed` still performs its existing digit slicing and rounding after
the raw expansion; the table does not claim that the complete public operation
now allocates only once. Its half-even ties, negative zero, non-finite behavior
and precision clamp are unchanged. Numeric output strings still use ARC, with
existing Perceus reuse available to subsequent owned appends.

## Compiler memory

The compiler uses these same primitive formatters when emitting register names,
constants and diagnostics. `primitive-formatting-compiler-measurements.json`
compares the merged #496 source-built compiler with the refreshed seed-to-source
compiler on identical final inputs. It includes one warmup and two alternating
ordinary samples, plus separate instrumented heap observations. The existing
Linux/GNU-linker probe now reports cumulative successful direct allocation
requests as well as live/peak bytes, exposing temporary native scratch absent
from Plum cell counters.

| Compiler emission mode | Native heap requests before → after | Native cumulative requested bytes before → after | Native peak requested heap before → after |
| --- | ---: | ---: | ---: |
| Release | 15,570,506 → 15,114,739 | 905,760,695 → 891,197,740 | 75,311,000 → 75,310,844 |
| Debug | 19,200,869 → 18,356,991 | 1,086,844,645 → 1,059,875,660 | 86,495,315 → 86,494,838 |

That is 455,767 fewer native requests / 14,562,955 fewer cumulative requested
bytes for release emission, and 843,878 / 26,968,985 for debug. Peak requested
heap is essentially flat. Ordinary Plum cell traffic slightly increases by
about 34 KB in release and 52 KB in debug (about 0.004–0.005%); those cell
counters omit the removed native scratch. This is a reduction in allocator
traffic during compilation, not a claim that compilation requires 14.6 or
27 MB less RAM. No RSS or throughput improvement is claimed.

Reproduce using `bootstrap/compiler-profile.py --case emit-compiler --case
emit-compiler-debug --samples 2`, the before/after binaries and optional
`--before-heap`/`--after-heap` binaries built as described in
`bootstrap/profiling/README.md`. The artifact records binaries, input hashes,
commands and probe hash. Debug observation exceeded the probe's default 2^20
slots, so both observed compilers use the same configurable 2^22 capacity.
This is excluded probe bookkeeping, not a change to production allocation.
Instrumented timings/RSS include that overhead and are not performance results.

## Validation

The native check verifies full-range integers, round-trip floats, signed zero,
subnormals, maximum finite values, infinities and NaN. Raw fixed text is checked
against the exact CRT symbol used by emitted LLVM, obtained through a small
IR function-address bridge. [MinGW's C headers](https://github.com/mirror/mingw-w64/blob/master/mingw-w64-headers/crt/stdio.h) can redirect an ordinary C
`snprintf` reference to a different formatter with different final digits;
that is not an oracle for the unchanged raw runtime path. Public fixtures preserve
Plum's explicit half-even expectations. Surviving results are retained across
other conversions, and 10,000 fixed conversions check bounded lifetimes.
Every native allocation is matched to its final release. An explicit i64-to-i1
LLVM bridge keeps private Boolean calls out of C's narrow-integer ABI.

The public fixture checks method/interpolation agreement, String/Bytes/CStr
aliases after append, precision boundaries, full-range values and concurrent
formatting. The native and public checks run in ordinary/tracked debug/release
modes, with ASan/LSan on Linux and macOS. Windows runs the same checks without
sanitizers; the suite remains in its required 30-minute storage job. A zero-cell,
zero-byte Boolean allocation guard prevents the finite results regressing to
heap copies.

The seed is regenerated and rebuilt through the normal seed-to-source path so
the distributed compiler also receives the improved runtime. Native shim
content and the cell ABI are unchanged.
