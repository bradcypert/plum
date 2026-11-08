# Encode symbol atoms without materializing integer bytes

A fresh exclusive-allocation profile on merged main `e4bb637` attributes
48,211,482 requested bytes and 652,834 allocations to `cg_symbol_atom` across
185,419 calls during release self-emission. Its temporary `Bytes.to_array`
result is built even when a name contains only ordinary ASCII and the
function returns the original String. The instrumented profile balances all
frames and produces LLVM identical to the ordinary compiler. Instrumented
timing and RSS are not used as performance measurements.

The encoder now scans the existing Bytes view directly. It returns the input
when every byte is a letter, digit or underscore. The first scan stops at the
first escape; an owned fragment collector continues there. Safe ASCII runs
are copied, escape fragments reference the existing lowercase hex table, and
one final join produces the encoded String. There is no integer-byte array.
Fragment collection starts only when encoding is actually required.

## Design and alternatives

| Approach | Assessment |
| --- | --- |
| Direct byte scan, lazy fragments, unchanged format | Chosen: removes the measured temporary array with existing runtime primitives and normal ownership |
| Remove only the empty fragment initializer | Does not remove the integer-byte array, which dominates the ordinary-name path; an empty literal can already be static |
| Append into an owned String | Measured as well: attractive on small escapes but fails the dense-escape allocation guard because current String growth relies on allocator slack; follow-up #478 |
| Cache encoded names | Avoids some repeated work but adds persistent retention and cache/key/lifetime policy |
| Change to compact hashes or numeric symbol IDs | Changes identity/debuggability and collision handling; not needed for this storage improvement |
| Add a shared native LLVM/symbol escape primitive | Could reduce per-byte RC overhead, but adds runtime/ABI/seed plumbing and couples two different escape policies |
| Share source character buffers between lexer/parser | Addresses larger frontend allocations but requires a separate source-context design |

The same fresh profile attributes 136.9 MB to final text joins, 65.2 MB to
lexing, 54.9 MB to six-pass LLVM text escaping, and 47.0 MB to parser source
context setup. Joins include required final-output copies. LLVM replacement
uses native scans and copies; replacing it with a per-byte Plum loop needs a
separate CPU/memory comparison. Symbol encoding offers a contained way to
remove clearly unnecessary storage first.

The owned-String experiment used the same direct byte scan and unchanged
format, replacing only the fragment collector with an owned String passed
through tail calls. It produced identical LLVM and slightly lower traffic on
today's compiler input, but its dense case grew poorly. At width 32, 100 dense
encodings requested 138,066 bytes in 1,310 allocations; at width 256 they
requested 7,542,152 bytes in 9,716 allocations. Fragments requested
619,966/5,054,052 bytes in 810/1,116 allocations at those widths. This rejects
the smaller-average-allocation design on worst-case growth, rather than
choosing solely by the whole-compiler total.

The runtime's String concat reuse path copies to an exact-sized allocation
when capacity is insufficient, unlike the Array growth path's geometric
reserve. A separate public String.repeat reproduction confirms that behavior
independently of symbol encoding. #478 records the measurements and asks for
a growth-policy discussion before changing emitted-program runtime behavior.
This PR retains owned fragments; no experimental String collector is shipped.

## Identity and ownership

The encoding from #320 is unchanged. ASCII letters, digits and underscore
pass through. Every other byte becomes `$hh`, with lowercase hex digits;
escaping '$' itself preserves injectivity, and escaping '.' preserves atom
boundaries. Unicode and malformed native bytes are encoded byte for byte,
not decoded or normalized. Function/module atom separation, recursive type
grammar, C extern names and runtime-helper names retain their existing rules.

The implementation is isolated in `codegen/symbol_atoms.plum`. Scalar reads
use the existing runtime primitive and explicit length checks. Both the first
scan and escaped-path traversal are tail recursive. The collector owns its
fragment array so ordinary Perceus reuse can grow it geometrically, including
the three consecutive pushes forming each escape. The existing checked
ASCII-run conversion and its fail-closed error path remain in place.

Ordinary names share their original immutable String. Escaped results own a
joined String; fragment runs and arrays are released normally. The hex table
already existed and is shared; no persistent cache, new runtime primitive,
String representation, ARC rule or FBIP inference is introduced.

This improves the compiler, not the allocation behavior of arbitrary emitted
programs. Generated identities and code stay the same. Cumulative allocation
traffic and peak resident memory are measured separately; removing temporary
arrays does not by itself establish a lower whole-compiler live-heap peak.
Compiler source is directly compiled by the existing seed, so no prelude/
runtime seed refresh is needed when seed equivalence passes.

## Coverage and allocation guards

An independent unit checks all 256 actual byte values against membership in a
literal ASCII alphabet and public byte-to-hex conversion. It verifies each
input's bytes, constructs NUL explicitly, and uses unsafe CStr copying only
for non-NUL raw single bytes. Mixed pairs exercise Unicode sequences, malformed
bytes, NULs, '$', '.', and safe prefixes/suffixes. Long ASCII and dense-escape
inputs exercise bounded stack usage; aliases and CStr owners survive a source
replacement. Existing identity/collision/type-grammar tests remain intact.

`bootstrap/symbol-encoding-check.py` copies the production module into an
isolated executable. Only its defensive `cg_fail` dependency is supplied as a
panic, preserving the failure contract. It checks ordinary ASCII, one late
escape between long ASCII runs, and dense escapes at three lengths, in both
debug and release modes. Input construction uses explicit doubling. ASCII
allocation counts exclude any per-encoding allocation; mixed/dense byte
budgets guard selected-result copying and owned-array growth. Dense budgets
leave allocator size-class headroom while rejecting repeated fragment copies.
The guard runs from `mem-check`; its sanitizer variant also runs the complete
byte oracle in both modes and is part of `sanitizer-check.py`.

For 100 encodings of a 160-byte ASCII input, the baseline requests 293,195
bytes across 605 allocations and fails the guard. The new implementation
requests 395 bytes across five allocations, all from input construction.
At 1,280/10,240 input bytes it requests 2,686/20,657 bytes across 8/11
allocations. The encodings themselves allocate nothing on that path.

All three focused workloads improve. At the largest width (2,048), 100
encodings in either mode request:

| Workload | Requested bytes before → after | Allocations before → after |
| --- | --- | --- |
| ASCII | 19,657,457 → 20,657 | 1,211 → 11 |
| Mostly ASCII with a dot | 43,495,573 → 4,199,573 | 2,015 → 715 |
| Dense '$' and '.' | 50,367,626 → 40,559,626 | 2,522 → 1,422 |

The dense case still pays for its required escape fragments and final String.
Its linear budgets intentionally leave room for allocator capacity rounding;
they reject repeatedly copying the growing fragment array.

## Related issues

#320's injective symbol identities remain the compatibility requirement.
Closed #449 established NUL-safe LLVM escaping, which is preserved rather
than mixed into this optimization. Closed #156 concerns JSON construction,
whose escape policy and output interface differ. #63 remains the broader
compiler-cost work; this PR does not close it.

## Whole-compiler measurements

The baseline is the final #477 seed-to-source binary, whose source tree is
identical to merged main `e4bb637`. The candidate is built directly from the
same seed and current source. Both process the same current inputs, with one
warmup and three serial interleaved measured rounds on Linux x86_64 using
Clang 23.1.4. [Raw samples, profile attribution and hashes](compiler-symbol-bytes-measurements.json)
include warmups. Whole-compiler check output and release/debug LLVM match
byte for byte across implementations and every round.

| Whole-compiler workload | Requested MB before → after | Allocations before → after | Median seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Release emit | 1072.17 → 1034.79 | 16,085,951 → 15,632,491 | 4.638 → 4.625 | 115.1 → 115.1 |
| Debug emit | 1238.75 → 1201.92 | 19,335,952 → 18,888,010 | 4.825 → 4.829 | 129.4 → 129.2 |
| Check | 285.50 → 285.50 | 8,618,024 → 8,618,024 | 1.703 → 1.700 | 47.9 → 47.5 |

Requested MB are decimal cumulative allocation traffic. Release emission
saves 37.38 MB (3.5%) and 453,460 allocations; debug emission saves 36.83 MB
(3.0%) and 447,942 allocations. Checking does not use this encoder and its
allocation totals are unchanged. Timing ranges overlap, and RSS is effectively
flat. No general build-speed or peak-heap improvement is claimed. Existing
whole-compiler memory ceilings remain intact; the focused guards reject the
old encoder and repeated fragment copies independently of RSS.

```sh
python3 bootstrap/compiler-profile.py --before /tmp/plum-qualified-final \
  --after /tmp/plum-symbol-final --samples 3 --case check-compiler \
  --case emit-compiler --case emit-compiler-debug --out /tmp/plum-symbol-bench
```
