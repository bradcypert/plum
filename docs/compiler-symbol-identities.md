# Injective compiler symbol identities

Fixes #320 and follows [program-scoped resource/type facts](compiler-resource-type-facts.md)
in the compiler allocation work from #63. On merged main ac6083e, a fresh
helper-level release-emission probe attributes 250,114,480 requested bytes and
7,881,616 allocations exclusively to `cg_sanitize`, over 459,680 calls. Its
instrumented output matches ordinary emitted IR; probe timings are excluded.

## The correctness problem

Replacing punctuation with underscores merges distinct module names such as
`a.b` and `a_b`. Global slots, function definitions/calls and type helpers all
inherit that collision. Delimiter-only type rendering has additional collisions:
`Box[Int]` and a nominal type named `Box_Int` previously shared a spelling.
An ordinary `id__Int` function could collide with an `id[Int]` specialization,
and `Point.add` with `Point_add`. A user function such as `str_eq` could also
collide with a runtime helper. These are identity errors, not merely invalid
characters in LLVM text.

The old compiler rejects the new module fixture with a duplicate global,
rejects the function fixture with a duplicate specialization symbol, and
rejects the native fixture because a C extern named `plum_main` overlaps the
generated entry function. The type-helper fixture links but fails an assertion
because nominal and generic types share helpers.

## Design and alternatives

`codegen/symbols.plum` owns generated function identities, recursive type
encoding and the backend's defaulted structural type identity. C externs and
fixed runtime ABI names keep their existing spelling. Generated functions,
type helpers, lifted closures and task entries have separate namespaces.
The encoder consumes resolved identities; import-alias and source-discovery
work tracked by #440/#321 remains separate.

| Approach | Benefit | Cost / decision |
| --- | --- | --- |
| Escaped atoms plus a tagged, arity-delimited type grammar | Deterministic and injective without mutable naming state; readable ASCII portions | Generated names change; some symbols are longer; chosen |
| Quote source names as LLVM identifiers | Can preserve punctuation and UTF-8 directly | Still needs unambiguous module/function/type structure; adds quoting/escaping to every consumer and less portable native symbol spelling |
| Encode every name byte as hex or prefix every atom with a length | Straightforward to decode | Hex expands common ASCII names; lengths need extra rendering even for ordinary names; safe-byte atoms with a reserved terminator are sufficient |
| Assign numeric IDs through an interner | Small symbols and cheap repeated references | Adds a program-owned identity table, deterministic assignment and invalidation rules; a larger lifecycle change than this fix needs |
| Detect collisions after lossy mangling | Prevents silently wrong code | Rejects valid programs and retains the ambiguous identity scheme |

The [LLVM identifier grammar](https://llvm.org/docs/LangRef.html#identifiers)
permits the dots and dollar signs used here. This is an internal generated ABI
change: code that depends on private compiler-generated names must be rebuilt
and updated. Native C extern spelling and fixed runtime names are unchanged.
The existing build-cache identity includes the compiler executable's content
hash, so artifacts from the previous compiler do not become hits for this one.

## Encoding and identity

A name atom preserves ASCII letters, digits and underscores. Every other UTF-8
byte, including `$` and `.`, becomes `$hh` with exactly two lowercase hex digits.
Thus `a.b` becomes `a$2eb`, `a_b` remains `a_b`, and literal `$2e` becomes
`$242e`. A dot terminates an atom and cannot occur inside it. UTF-8 is encoded
as bytes, without normalization or loss of source spelling.

```text
function = plum.f. <module-atom> . <function-atom> . <type-list>
helper   = @plum.h. <fixed-family> . <type>
closure  = @plum.l. <site-number>             (release sibling: _rel)
task     = @plum.task. <site-number>

type-list = <decimal-count> . <that many types>
type = i | f | b | s | c | p | y | u
     | S <name-atom> . <type-list>            (struct)
     | E <name-atom> . <type-list>            (enum)
     | T <type-list>                         (tuple)
     | F <parameter-type-list> <return-type> (function)
     | R <type>                              (reference)
```

The primitive tags distinguish Int, Float, Bool, String, CStr, RawCStr, Bytes
and Unit. Struct and enum tags distinguish nominal types from primitives and
from each other. Counts determine exactly where nested argument lists end.
Each node therefore has one parse: atom escapes are fixed-width, atoms end at
an unescaped dot, and recursively encoded children have known counts.

Unconstrained inference variables retain the backend's existing Unit default.
Unsubstituted generic parameters remain an internal error. The identity unit
pins both rules so discovery and emission cannot disagree about a specialization.

Discovery now compares structural identities rather than rendering strings.
Specialization comparisons check module, function name and defaulted type
arguments. Equality/hash/release discovery sets use `Map[ITy, Bool]`; the
channel-copy type scan uses the same defaulting rule. Already concrete type
trees are shared, while only trees containing unresolved variables need new
defaulted nodes. Symbol rendering happens where emitted names are required.

## Memory and Perceus

The initial grammar implementation made fewer allocations but slightly more
requested bytes. Empty argument lists now use a constant encoding, and identity
checks avoid name construction entirely. The atom encoder scans a temporary
byte-value array, keeps ASCII runs as fragments, and joins escaped output once.
An entirely safe atom returns the original string. This avoids constructing
and mapping an array of separately allocated character strings.

All buffers are ordinary owned Plum values. The existing Perceus array reuse
and reference-counting paths release byte arrays, fragments and intermediate
strings. There is no new native builder, ownership representation or global
symbol cache. Runtime cell layout, dup/drop placement and resource finalization
rules are unchanged.

Existing helper-discovery sets now retain descriptive type keys rather than
rendered strings. Those trees can share storage with types already held by the
compiler; the sets reset at the next program emission, as before. They hold no
actual user resources or native payloads. Longer emitted names and retained type
metadata can affect the peak independently of cumulative allocation savings,
so both requested bytes and RSS must be measured. No diagnostic registry or
NativeBuffer accounting change is involved.

Whole-compiler requested-byte budgets tighten to 2.5 GB for debug emission and
2.3 GB for release (from 2.9/2.7 GB). RSS ceilings retain Linux's 144 MiB debug,
128 MiB release and 64 MiB checking limits, and macOS's 192/192/80 MiB limits.

## Verification

The identity unit checks punctuation, escape-like text, UTF-8, embedded NUL,
component boundaries and defaulting. It checks 585 distinct primitive, nominal,
tuple, reference and function type shapes for unique encodings. Runtime fixtures
exercise distinct module functions/globals, differing type layouts, equality,
hashing, rendering, destructors, generic specializations, associated functions,
function values and native extern linkage. Tracked sanitizer checks run all four
fixtures in debug and release.

Generated-symbol consumers in sanitizer, literal-allocation, debug-info and
object-cache tests use the new names; literal names are regex-escaped where
needed. Debugger source-location checks still require the actual function symbol
and line, rather than skipping validation after a rename.

Whole-compiler IR is compared after normalizing global identifier tokens in
first-occurrence order, leaving quoted string payloads unchanged. The instruction
text and emission order match exactly; raw output intentionally differs because
generated names have changed. Bootstrap checks additionally compare new-compiler
stages byte for byte. Functional and sanitizer fixtures cover the collision
cases where preserving old output would be incorrect.

## Measurements

Medians use decimal GB of requested allocation traffic and MiB of peak RSS.
[Raw samples, input/binary hashes and normalized IR hashes](compiler-symbol-identities-measurements.json)
record three measured rounds. Both ordinary binaries process the same changed
source, including the new unit and identity module. The baseline is merged main
ac6083e; the changed compiler is built through its refreshed seed.

| Whole-compiler workload | Allocated GB before → after | Allocations before → after | Wall seconds before → after | Peak RSS MiB before → after |
| --- | --- | --- | --- | --- |
| Debug emit | 2.397 → 2.220 | 29,177,390 → 22,105,017 | 8.223 → 6.629 | 127.0 → 129.3 |
| Release emit | 2.227 → 2.050 | 25,858,426 → 18,792,657 | 8.003 → 6.338 | 113.7 → 114.8 |
| Check | 0.320 → 0.320 | 9,894,961 → 9,894,897 | 2.097 → 2.083 | 48.0 → 48.0 |

Release traffic falls 7.9%, debug traffic 7.4%, saving about 176 MB per
invocation. Allocation counts fall 27.3% and 24.2% respectively. Release
emission takes 7.997–8.412 s before and 6.317–6.521 s after; debug emission
takes 8.220–8.345 s before and 6.572–6.651 s after. The medians improve 20.8%
and 19.4%. Checking has overlapping timing ranges; its tiny allocation change
(64 allocations, about 1.7 KB) accompanies replacement of the old global
character whitelist with the constant hex table.

Emitted IR grows by 426,644 bytes in both modes (about 1.6% release, 1.3%
debug) because symbol spellings change. The normalized instruction text and
emission order match in both modes. Peak RSS rises about 1.1 MiB for release
and 2.3 MiB for debug. This change improves identity correctness, transient
allocation traffic and observed LLVM emission time; it does not improve the
retained-memory peak. The timing excludes Clang and does not establish an
end-to-end native-build speedup. Reachability queue copying and cycle searches
remain separate targets for a later profile.

The next pass [batches pending callgraph work](compiler-callgraph-worklists.md)
to remove repeated frontier copies while preserving discovery order and fuel.

## Reproduction

Build merged-main ac6083ee749ef30659d915430d9ea6fc45ccc3f5 and this branch through
their own checked-in seed-to-source paths. Use both ordinary compilers on the
same current source tree:

```sh
python3 bootstrap/compiler-profile.py --before /path/to/main-compiler \
  --after /path/to/changed-compiler --samples 3 \
  --case check-compiler --case emit-compiler --case emit-compiler-debug \
  --out /tmp/plum-symbols-profile
```

The benchmark uses one warmup and three serial interleaved measured samples,
alternating order with isolated caches and no diagnostic address registry.
Requested bytes measure allocation traffic; RSS measures the process peak.
Probe/instrumented timing is excluded, and ordinary timing is observational.
