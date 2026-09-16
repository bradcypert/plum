Plum is a small, statically typed, compiled language.

Plum can hash now, and a quadratic went with it.

## `Crypto.sha256`

```plum fragment
use Crypto;
use Encoding;

let digest (s: String): String =
    Encoding.hex_encode(Crypto.sha256(Bytes.from_string(s)))
```

Pure Plum. No C shim, no platform branch.

It is a new module rather than a function in `Encoding`, because hex,
base64 and percent-encoding are all reversible transformations and a
digest is one-way. They compose without being neighbours, as above.

**Cryptographic**, unlike `String.hash`, which is FNV-1a for bucketing
and says so in its own documentation. Use `Crypto.sha256` when the
answer has to mean something to somebody else.

It is writable in Plum at all because of two earlier releases:
`Int.wrapping_add` from 0.0.29, added because hashing needs it, and the
bitwise operators from 0.0.26. `Int` is 64 bits, so the 32-bit lanes are
masked explicitly.

Checked against all four of NIST's published vectors, which is firmer
ground than most things here get: the expected answer is what the
standard says rather than what this compiler produced last time.

## `String.repeat` was quadratic

```plum fragment
String.repeat("-", 40)
```

That was fine. This was not:

| characters | allocated, before | after |
|---|---|---|
| 25,000 | 313 MB | 19.6 MB |
| 100,000 | 5.0 GB | 313 MB |
| 1,000,000 | never finished | 657 MB, 44ms |

It was written as `s.concat(String.repeat(s, n - 1))`, which
concatenates onto a string one character shorter each time, so the bytes
allocated are 1+2+3+...+n.

`String.pad_left`, `pad_right` and `pad_center` are all built on it, so
padding to a large width was quadratic too.

It is a loop with a self-rebinding accumulator now, which the compiler
can grow in place. The allocation counter shows the difference: 100,000
concatenations used to cause 100,000 allocations and now cause 6,251.

Found by SHA-256's million-character test vector, which would not run.
It is the seventh accidental quadratic in this project, and the first
found by a test vector rather than by a harness.

## Also in this release

- A name declared inside a standard-library module could delete a
  compiler builtin of the same name from the generated reference. They
  are different functions that merely spell alike. Not reachable today,
  which is why it was worth fixing before it became so.
