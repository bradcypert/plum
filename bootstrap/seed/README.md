# The bootstrap seed

`plum.ll` is the self-hosted Plum compiler, as LLVM IR.

It exists to answer one question: **you just cloned this repo — how do
you get a compiler?** The self-hosted compiler is written in Plum, so
building it needs a Plum compiler, and this is that compiler in a form
`clang` can turn into a binary:

```
./bootstrap/from-seed                       # clang only, no Rust
./sh.seed build bootstrap/self_host -o sh.real
```

## Why IR, and not a binary

IR is text: readable, diffable, and reviewable in a pull request the way
a checked-in `.so` never is. It is also platform-independent enough to
build anywhere clang runs, which a committed binary is not. The cost is
6MB of generated text, which is why it is refreshed rarely (below).

## Why this exists at all

The Rust compiler in `crates/` can also build the self-hosted compiler,
and did, for the whole of its development. But depending on it means the
self-hosted compiler's own source is confined to the INTERSECTION of the
two languages: `bootstrap/self_host/main.plum` has to compile under
both. That was a real, recurring tax — a `mkdir` primitive, three stdin
primitives, an extern block's placement and a `CStr` lifetime were each
designed twice because of it.

With a seed, the compiler's source can use the compiler's own features,
and the seed is refreshed when it can no longer keep up.

## Refreshing it

Do not refresh on every change. The seed does not need to be CURRENT; it
needs to be new enough to compile today's source.

```
./bootstrap/check-seed     # fails when the seed has fallen behind
./bootstrap/gen-seed       # then, and only then, refresh it
```

`check-seed` proves three things, the third being the one that matters:
clang can build the seed; the seed compiler can build today's source;
and what it produces emits IR identical to the current compiler's. A
seed that built but produced a DIFFERENT compiler would silently
bootstrap something other than this source tree.

Each refresh puts ~6MB of generated text into history, so it should be
a deliberate commit with a reason, not a reflex.
