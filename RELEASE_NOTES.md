Plum is a small, statically typed, compiled language.

The API reference did not list `Array.map`. It does now.

## The reference was missing the builtins

`plum doc` reads declarations, and 31 of the standard library's methods
are implemented by the compiler and have no declaration anywhere. So
nineteen of them appeared on no page at all:

```
Array.map     Array.filter   Array.fold    Array.len     Array.push
Array.remove  Array.set      String.concat String.as_cstr to_string
Bytes.as_cstr CStr.as_string Float.to_int  Float.round_to_int Int.to_float
Ref.get       Ref.set        Sender.send   Receiver.recv
```

`Ref`, `CStr`, `Sender` and `Receiver` are entirely builtin and had no
page whatsoever. The reference on
[plumlang.org](https://plumlang.org/api/index.html) documented 335
declared functions and not the most-used one in the language.

They are documented now, with prose written for each rather than a bare
signature, and they are in the search index. The mechanism is worth a
sentence because it is the reason this was one function rather than a
second copy of the generator: each builtin is turned into a real item by
writing Plum and parsing it, so module grouping, namespace pages,
anchors, the HTML emitter and search all work on it unchanged.

## One reference, not two

`STDLIB.md` and the `plum stdlib-reference` command are **removed**.
The flat list of every declaration is now
[`docs/stdlib/index.md`](docs/stdlib/index.md), written by the same pass
as the pages, from the same items, with the same anchors.

If you linked to `STDLIB.md`, that is the replacement. On the site it is
`/api/index.html`.

Two generators for one library is two chances to disagree, and they had,
in both directions: `STDLIB.md` listed `Map.get` and `String.trim` twice
each, once as a declaration and once as a builtin, while `docs/stdlib/`
was missing the nineteen above.

Both documents were generated. Both were verified against the compiler
by a harness. Both harnesses passed every day while the reference was
incomplete, because a generated file is only as complete as what its
generator can reach, and neither could ask whether the two described the
same library.

`plum doc` also writes `index.md` for **your** project now, which a
command that only knew about this compiler's standard library could
never do.

## Wrapping arithmetic

```plum fragment
Int.wrapping_add(a, b)
Int.wrapping_sub(a, b)
Int.wrapping_mul(a, b)    // or a.wrapping_mul(b)
```

`+`, `-` and `*` on `Int` still abort on overflow. That is deliberate
and is not weakened: an integer that silently went negative is a wrong
answer that keeps running.

Some algorithms are defined over a fixed-width word, though, and could
not be written in Plum at all. PCG and splitmix64 need a wrapping 64-bit
multiply; so do FNV-1a, xxHash and MurmurHash; so do checksums and
binary protocols specified mod 2^64.

The cost was already visible in three places in this repository. `Rng`
is L'Ecuyer's 1988 generator, chosen because every intermediate stays
inside `i64` by construction rather than on merit. `String.hash` is a
runtime primitive, and the irony sits in the runtime itself: that
primitive is FNV-1a, and its `mul` is exactly the wrapping multiply Plum
could not express. And the property tests' own generator is a 31-bit
LCG, with a comment explaining that the usual 64-bit constants would
kill the test process rather than wrap.

Named functions rather than operators, so wrapping is asked for and
visible at the call site. It is also the additive choice: an operator
can be layered on later without breaking anything written against these.

The property asserted is that they agree with the checked operators
wherever the checked ones do not overflow, because `wrapping_mul` is not
a different multiply. It is the same one with an answer in the one place
`*` refuses to give one.

## Also in this release

- Three harness comments described `plum stdlib-reference` after it
  stopped existing, one of them wrongly describing how arguments
  dispatch.
- `site/content/install.md` was generated, gitignored, and committed
  anyway. It is no longer tracked.
