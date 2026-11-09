# Checked, exact-size String joining

`Array.join` built an intermediate `Array[String]` alternating input parts and
separator references, then passed it to `Array.concat_all`. Uniqueness-based array
growth kept this linear, but still allocated a second pointer array and retained
its elements before copying the output.

`Array.concat_all` already used a direct two-pass copy, but accumulated byte
lengths and added the cell header without overflow checks. The next step is one
shared checked constructor, rather than an independent special case for join.

## Alternatives compared

A bounded experiment compared the existing fragment array, an owned String
accumulator using ordinary Perceus concat reuse, and an exact-size fill. All
passed comparisons for empty arrays, empty parts, binary and Unicode content,
and empty/multibyte separators in debug and release. The exact-fill prototype
replaced only the measured wrapper's LLVM; its unchecked bounded implementation
was exploratory. The production measurements below use unmodified compiler
builds and the checked constructor.

For 32,768 strings cycling through `a`, `é`, NUL, `🙂`, and empty, joined with
`<NUL>` (three bytes), debug and release reported the same figures:

| Approach | Construction allocations | Cumulative requested bytes | Retained result request | Whole-process peak requested bytes |
| --- | ---: | ---: | ---: | ---: |
| Intermediate fragment array | 16 | 1,731,482 | 150,746 | 1,589,136 |
| Owned String accumulator | 23 | 582,348 | 194,187 | 725,006 |
| Exact-size fill | 1 | 150,746 | 150,746 | 597,952 |

The accumulator removes pointer-array storage but repeatedly grows/copies output
and retains reserve capacity. A checked exact-size pass fits both joining APIs:
they already know all input strings. Introducing a public mutable builder or a
new representation is unnecessary for this operation.

## Shared constructor and ownership

The private planning helper validates the array count before calculating pointer
indices, checks `(count - 1) * separator_length`, and uses checked addition for
every part. Existing helpers enforce the signed size limit including the
16-byte header and trailing NUL. Invalid sizes use the existing allocation-free
`invalid concatenation size` fatal diagnostic.

The fill helper borrows the array, every String element, and the separator. It
never mutates them, even if the array or an element is unique, and returns a +1
String. Nonempty output requests exactly `payload_length + 17` bytes through the
existing allocator and ByteStorage accounting. Zero-byte output returns shared
static storage. Empty parts still have separators between array entries;
`Array.concat_all` uses the same constructor with an empty separator.

This keeps Perceus ARC and compiler-selected reuse. No writable views, new
ownership policy or cell layout are introduced. The inputs must remain valid
and immutable while borrowed, as before. Checks validate representable lengths;
they do not make invalid native pointers safe.

## Larger memory story

The main saving is in **compiled programs using `Array.join`**: remove the
intermediate fragment array, per-element retain/release traffic, and its overlap
with the final result. Nonempty result storage is unchanged; zero-byte results
avoid the previous 17-byte allocation. `Array.concat_all` keeps its direct-copy
strategy while gaining size checks and the static empty result.

The compiler uses these APIs for emitted fragments, diagnostics, source slices,
build-cache serialization and generated documentation. Its own savings require
a refreshed seed and a compiler built through the actual seed-to-source path.
Compiler emission measurements are separate from the joined-program figures.
Cumulative requested bytes, retained requested storage and process peak requested
bytes are distinct; none of them alone proves an RSS reduction.

Issue searches found no dedicated current `Array.join` or `concat_all` ticket.
Related [#452](https://github.com/bradcypert/plum/issues/452), about reuse across
successive rebinding appends, is already closed as completed. This change uses
that existing ownership machinery where appropriate and makes no claim to fix
or reopen that issue. The fragment-array baseline already grows linearly.

## Reproduction and safeguards

```sh
python3 bootstrap/measure-string-join.py /path/to/before /path/to/after \
  --out /tmp/string-join-measurements.json
python3 bootstrap/string-growth-check.py /path/to/after --sanitize
bootstrap/property-check /path/to/after
PLUM=/path/to/after bootstrap/alloc-check
```

The measurement observer verifies all output bytes, length and NUL without Plum
allocations. Traffic/live deltas exclude setup; the process peak includes input
array construction, whose overlapping growth allocations dominate the exact-fill
peak in this workload. Input remains alive after observation and the output's
CStr keeps its owner alive.

The storage guard extracts actual emitted LLVM at O0/O2 with/without tracking
and optionally ASan/LSan. It checks borrowed unique inputs with spare capacity,
aliases between separators and elements, empty output, exact requests and
ByteStorage release, array-index limits, part/total/header overflow, separator
multiplication overflow and injected OOM before copying. Public fixtures cover
byte order, embedded NUL, unsafe raw bytes, empty parts, long separators, larger
arrays and String/Bytes/CStr aliases. Properties check partition composition,
output length and equivalence between concat_all and empty-separator join.


## Production measurements

[Program measurements](string-join-measurements.json) confirm the exact-fill
figures above in both modes using unmodified compiler builds. With an empty
separator, the same 32,768 inputs fall from 1,633,181 requested bytes to one
52,445-byte result request. Empty arrays avoid the old 17-byte result allocation.
Neither nonempty workload reduces the retained result size versus the old join.

[Compiler measurements](string-join-compiler-measurements.json) compare the same
final source tree after one warmup and two recorded samples per mode:

| Compiler emission | Before requested bytes | After requested bytes | Approximate reduction |
| --- | ---: | ---: | ---: |
| Default/debug | 1,078,196,051 | 1,075,622,539–1,075,622,540 | 2.57 MB |
| Release | 911,553,625–911,553,626 | 909,024,347 | 2.53 MB |

These figures describe allocation traffic **while compiling**, separately from
memory saved by a produced program's joins. RSS ranges overlap in the release
samples (before 116,700–117,220 KiB; after 116,284–116,872 KiB). No RSS or timing
improvement is claimed. The focused allocation fixture performs 1,000 joins on
static parts and requires exactly 1,000 allocations / 174,000 requested bytes;
input/setup storage does not hide temporary arrays in that guard.
