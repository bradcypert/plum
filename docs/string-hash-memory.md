# Allocation-free byte reads in String.hash

The named `String.hash(text)` form read each byte through public `Bytes.at`,
creating a temporary `Some(Int)` cell for every byte. These cells were freed
while scanning, so the problem was allocation traffic rather than a retained
buffer. The existing private bounded byte read removes that traffic while
keeping the FNV-1a algorithm in Plum.

## Alternatives and chosen boundary

| Approach | Allocation traffic for a 524,288-byte input | Additional live bytes after hashing | Whole-process peak requested bytes |
| --- | ---: | ---: | ---: |
| Original named String.hash | 524,288 allocations / 12,582,912 bytes | 0 | 524,329 |
| Direct-byte Plum prototype | 0 / 0 | 0 | 524,305 |
| Existing method `text.hash()` | 0 / 0 | 0 | 524,305 |
| Final named String.hash | 0 / 0 | 0 | 524,305 |

[Measurements](string-hash-measurements.json) cover ASCII, multibyte UTF-8,
embedded NUL, line breaks and repeated calls over five sizes in debug/release.
The observer independently computes unsigned 64-bit FNV-1a in C and verifies
every returned value before recording. Before/after are ordinary compiler
builds; the direct-byte prototype and existing method control are compiled by
the before compiler. No LLVM substitution is used.

Replacing `Bytes.at(...).unwrap_or(0)` with the existing private byte read is
valid because the helper begins at zero and stops at `i >= n`, where `n` is the
borrowed byte length. Every read therefore satisfies `0 <= i < n`. The increment
cannot overflow a valid String length. The offset basis, XOR, explicitly wrapping
multiply, byte order and final sign-bit mask remain unchanged. This does not
change public `Bytes.at` or its bounds/error behavior.

Delegating the named form to the native method also achieves zero allocations,
but provides no further memory savings over the direct-byte source scan.
[#48](https://github.com/bradcypert/plum/issues/48) deliberately moved this
public algorithm into Plum once wrapping arithmetic made that possible. The
chosen fix preserves that boundary. Moving all generic/structural Hash lowering
to a common Plum implementation would involve compiler reachability and helper
generation; it is a larger design change, not necessary to eliminate these cells.

Caching a hash in every String would change cell layout, retention costs and
possibly concurrent initialization. A separate cache would add its own storage
and lifetime management. Those approaches target repeated hashing time, whereas
this change already removes all temporary hashing allocations without extra
retained state. No such redesign is introduced.

## Exact behavior and ownership

The hash still processes UTF-8 **bytes**, including embedded NUL, using:

- Offset basis `14695981039346656037` (signed `-3750763034362895579`).
- XOR followed by multiply by `1099511628211`, modulo 2^64.
- A final mask of `9223372036854775807`, producing a nonnegative Int.

Existing historical vectors and their recorded output are retained. New tests
compare named, method and first-class function forms against those vectors, the
original public byte-read algorithm, and independently generated FNV values.
They include all individual byte values, unsafe high-byte strings, valid Unicode,
embedded NUL, long strings, aliases and collection lookups. Hash compatibility
and Map/Set behavior remain unchanged. As documented publicly, these are
noncryptographic bucket hashes, not a persistence or security contract.

`String.as_bytes` borrows/shares the same cell without copying. The hash returns
a scalar Int, creates no result buffer, never mutates input and does not retain
it after the call. String/Bytes/CStr aliases keep their usual owners; no new
ownership or cell-layout rule is needed. Perceus ARC/last-use behavior remains
in force. This is a library allocation improvement, not a new ARC/FBIP compiler
pass or a collection representation change.

Related [#123](https://github.com/bradcypert/plum/issues/123) already established
hash/equality consistency for collection keys. Its behavior is unchanged.
[#276](https://github.com/bradcypert/plum/issues/276#issuecomment-6132630944) was
closed after its stack-exhaustion claim was disproved by guaranteed tail calls.
This change does not reopen or claim to fix that false positive; the same tail
scan remains.

## Larger memory story

The savings apply to **compiled programs calling `String.hash(text)`**, including
first-class references to that function. The method `text.hash()` and generic
String hashing in Map/Set already use the allocation-free compiler helper;
their allocation traffic does not improve here. Ordinary compiler emission
mostly uses that generic path, so its measurements are reported separately.

For one 524,288-byte Unicode/NUL input, eliminating 12,582,912 requested bytes
means eliminating cumulative malloc/free traffic. It does **not** mean the
program holds 12 MB less live memory. The previous implementation held only one
24-byte optional cell at a time, and retained no hash output. Peak requested
storage in that workload falls by only 24 bytes; the original input cell stays
live in both cases. No RSS or timing improvement is claimed.

The observer excludes input construction from traffic/live deltas and keeps
input alive through observation. Whole-process peak includes setup. C allocator
internals and RSS are separate from Plum's requested-byte counters.

## Reproduction and safeguards

```sh
python3 bootstrap/measure-string-hash.py /path/to/before /path/to/after \
  --out /tmp/string-hash-measurements.json
bootstrap/property-check /path/to/after
PLUM=/path/to/after bootstrap/alloc-check
python3 bootstrap/string-growth-check.py /path/to/after --sanitize
```

The 120 measurement rows compare four approaches, two modes, five sizes and
three byte patterns, including repeated short calls and single long calls. An
allocation-free native reference verifies every result before snapshots are
reported. The public compatibility fixture runs debug/release, tracked/untracked
and with ASan/LSan through the existing storage guard. Its historical five output
vectors remain intact; new tests extend rather than replace them. Public property
tests use only public APIs. Example output is checked without re-recording it.

Two allocation fixtures cover named and first-class calls: repeated short hashes
must request **zero allocations / zero bytes**; repeated hashes of a 262,144-byte
input must allocate only its single **262,161-byte input cell**. These budgets
would reject the original per-byte optional reads. Aliases and returned hashes
are validated alongside the allocation counts.

The seed is regenerated, rebuilt to the source compiler and checked for bootstrap
consistency/native filename bytes. Source documentation is regenerated with the
source-built compiler. Native Windows/macOS execution remains CI coverage; the
actual compatibility fixture is also compiled/linked for four cross targets and
its aarch64 Linux form runs under QEMU.

## Compiler measurements

[Separate compiler measurements](string-hash-compiler-measurements.json) use the
predecessor and refreshed seed-to-source compilers on identical final compiler
and stdlib sources. Each mode has one warmup and two recorded samples:

| Compiler emission | Before requested bytes | After requested bytes |
| --- | ---: | ---: |
| Default/debug | 1,076,986,512–1,077,134,331 | 1,077,035,685 |
| Release | 909,840,793–909,840,794 | 909,881,522–909,881,523 |

Debug is within the prior sampled range. Release traffic increases by about
**41 KB** in these samples. This combined module/runtime overhead is separate
from the eliminated named-hash traffic; no compiler-memory saving is claimed.
RSS is recorded separately and no RSS reduction or timing improvement is
inferred. Other validation ran concurrently, so elapsed times are omitted.
Existing compiler memory guards continue to enforce their budgets.

To reproduce the compiler comparison on Linux:

```sh
python3 bootstrap/compiler-profile.py --before /path/to/before --after /path/to/after \
  --case emit-compiler --case emit-compiler-debug --samples 2 --out /tmp/hash-compiler-profile
```
