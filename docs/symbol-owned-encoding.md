# Owned symbol rendering after geometric String growth

Follow-up to #479/#480 and related to #320/#478. The `$hh` byte encoding,
ASCII input sharing, String/Bytes/CStr representation and Perceus ownership
rules remain unchanged. This change optimizes the compiler's working memory;
it does not change the generated program's runtime or symbol spelling.

## Compared approaches

All three implementations used the same merged #480 runtime. Each encoded
ASCII, mixed and dense inputs in debug/release at three sizes, and passed the
independent all-256-byte and pair oracle, including raw invalid UTF-8 and NUL.

| Approach | Dense traffic, 100 encodings at width 2,048 | Allocations | Dense release probe size | Assessment |
| --- | ---: | ---: | ---: | --- |
| Owned fragment array + exact join | 40,559,626 B | 1,422 | 145,664 B | Avoids repeated prefix copying, but retains three pointers per escaped byte. |
| Owned String + shared 16-digit hex table | 5,127,726 B | 1,722 | 141,512 B | Chosen. Geometric growth removes the earlier quadratic-copy objection; less temporary storage and a smaller interface/state. |
| Owned String + 256 complete escapes | 5,127,926 B | 1,722 | 159,280 B | Similar traffic and timing; adds roughly 18 KB of read-only executable data versus the chosen version. |

Dense probe process medians were approximately 16.0 ms, 9.8 ms and 9.7 ms,
respectively, across nine interleaved release runs. These small process
measurements do not establish a speed difference between the two String
versions or promise faster whole-compiler builds. Mixed/ASCII timings likewise
did not justify the larger table. Executable sizes describe these probes,
not user-program output sizes.

An exact-size specialized byte writer could avoid spare output capacity, but
would add a new writable-buffer/runtime boundary and another encoding
implementation. It is deferred: the existing consuming String interface now
provides bounded growth and passes the real compiler and dense-input guards.
A public builder or cell-layout change is unnecessary for this operation.

## Memory impact

A single dense input with 4,096 bytes produces 12,288 encoded bytes:

| Requested storage | Fragments | Chosen String |
| --- | ---: | ---: |
| Traffic for encoding | 405,265 B | 50,946 B |
| Result's retained request | 12,305 B | 17,043 B |
| Process peak | 298,993 B | 32,511 B |

The dense probe makes more small growth allocations, but requests far fewer
bytes overall. The owned String releases the fragment array cost, while retaining some spare
capacity. It does not guarantee a smaller final result allocation. A mixed
3,073-byte input produced the same 3,092-byte result request in both versions;
traffic was 6,358 versus 6,246 B and process peaks 9,641 versus 9,561 B.
These are diagnostic builds' original requests, including headers, not RSS or
allocator rounding. Allocation-free native observers checked every output
byte, its logical length, and its terminating NUL. Both inputs stayed alive
at the observation point, so live-byte deltas isolated the result cell.

On identical final compiler sources, measured with the actual source-built
compiler (and the same merged seed/runtime):

| Emission | Fragment traffic | Owned String traffic | Fewer allocations |
| --- | ---: | ---: | ---: |
| Debug | 1,086,926,260 B | 1,077,964,953 B | 123,445 |
| Release | 920,072,731 B | 910,940,921 B | 125,369 |

That is about **9 MB less cumulative allocation traffic per emission**.
Compiler-only RSS stayed near 128 MiB debug and 114 MiB release; the small
observed differences do not establish a peak-RSS improvement. Exact allocation
figures can vary with allocator placement/rounding. Compiler output was
byte-for-byte identical in both modes, checked with SHA-256. This includes the
compiler's complete LLVM output, not just hand-picked mangled names.

## Implementation and regression protection

The scanner still stops at the first byte needing escape and shares the
original String if none exists. The escaped path copies the initial ASCII
run, then moves one output owner through tail calls. It appends ASCII runs
and `$`/hex digits through ordinary compiler-selected Perceus concat reuse.
The shared input Bytes view is read-only; no fragments array or new allocator
interface is needed. The final output may later become shared normally.

The focused guard compiles the actual source module and checks proportional
traffic budgets at widths 32, 256 and 2,048 in both modes. Its tightened dense
budget rejects the old fragment traffic as well as exact-growth copying.
ASan/LSan uses the same budgets. Additional ownership assertions retain a
reserved encoded result as String/Bytes/CStr, append to the original binding,
and verify all aliases and the input remain unchanged.

Raw [comparison records](symbol-owned-comparison.json) contain all variants,
probe timings, retained/peak observations and final compiler hashes. Normal
and sanitizer guards are reproducible with:

```sh
python3 bootstrap/symbol-encoding-check.py /path/to/source-built-compiler
python3 bootstrap/symbol-encoding-check.py /path/to/source-built-compiler --sanitize
```

The seed already supplies #480's runtime; this source-only renderer change
requires no runtime or seed refresh. The seed → source reproduction gate
checks the resulting compiler separately.
